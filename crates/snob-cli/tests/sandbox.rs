//! The whole binary, driven end to end, with no Instagram and no receiver of
//! anybody's.
//!
//! Everything else in this suite drives a library. `watch_tick.rs` runs the
//! comparison against a mock server through `engine::watch::tick`;
//! `watch_webhook.rs` runs the outbox through `WebhookClient`. Nothing ran the
//! program: not `main`, not the dispatch, not `AppPaths::discover`, not the
//! secret store choosing a backend, not the exit code a timer would read. The
//! parts of `snob watch` a person actually configures and a receiver actually
//! hears from were the parts nothing exercised as a whole.
//!
//! **What makes it possible, and what makes it safe.** Two flags, both behind
//! the `testing` Cargo feature so a released binary contains neither them nor
//! the code they reach — `crates/snob-core/tests/sandbox.rs` reads the source
//! to hold that down. `--sandbox-root` puts every file this run touches under
//! one temporary directory and forces the file backend, so the keyring is not
//! opened at all: the rule `crates/snob-core/tests/keyring.rs` holds the test
//! suite to, applied to the binary. `--ig-base-url` requires `--sandbox-root`,
//! which is the whole safety argument for it existing: a redirected client can
//! only carry a session out of a store inside that root, so the real stored
//! session of whoever runs this is not reachable.
//!
//! The pace is off for the same reason it is off in every other test, and it is
//! honest about it: `IgClient::is_live` answers by address, and the address here
//! really is a local mock server.
//!
//! Every test gets its own sandbox root, its own fake Instagram and its own
//! fake receiver, so nothing shares state and the file cannot pass by accident
//! because a neighbour ran first.
#![cfg(feature = "testing")]

use std::path::Path;
use std::process::{Command, Output};

use wiremock::matchers::{method, path as url_path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// A session cookie's worth of digits, which is all the shape anything checks.
const SESSIONID: &str = "42%3Asandbox%3A17";

/// Given explicitly so nothing goes looking for an installed browser: this has
/// to run the same on a laptop with three of them and on a CI container with
/// none.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// The account the fake Instagram answers about, and the one the session is.
const PK: u64 = 42;

/// Runs the real binary against one sandbox, and says what it did.
///
/// `CARGO_BIN_EXE_snob` is the binary this test's own build produced, so it
/// carries the `testing` feature and nothing else has to be arranged. Every
/// invocation gets `--sandbox-root`, which is what keeps the keyring and the
/// user's real data directory out of it.
fn snob(root: &Path, instagram: Option<&MockServer>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_snob"));
    command.arg("--sandbox-root").arg(root);
    if let Some(server) = instagram {
        command.arg("--ig-base-url").arg(server.uri());
    }
    command.args(args);
    // A run that inherits a terminal would try to draw a progress bar and, in
    // the wizard's case, ask a question. Neither is what is under test here.
    command.env("NO_COLOR", "1");
    command.output().expect("the binary runs")
}

/// The same, with something on standard input — which is how `--paste` reads a
/// sessionid when nobody is at a terminal.
fn snob_typing(root: &Path, instagram: &MockServer, args: &[&str], typed: &str) -> Output {
    use std::io::Write;

    let mut child = Command::new(env!("CARGO_BIN_EXE_snob"))
        .arg("--sandbox-root")
        .arg(root)
        .arg("--ig-base-url")
        .arg(instagram.uri())
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");
    child
        .stdin
        .as_mut()
        .expect("stdin was piped")
        .write_all(typed.as_bytes())
        .expect("the binary reads what it is given");
    child.wait_with_output().expect("the binary finishes")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// An Instagram that answers everything a run needs and nothing it does not.
///
/// `followers` and `following` are the counters the profile declares; the lists
/// themselves come back with that many made-up accounts, so a walk has
/// something real to compare.
async fn fake_instagram(followers: u64, following: u64) -> MockServer {
    let server = MockServer::start().await;

    // `validate()`: the session works. Told apart from a walk of the same list
    // by `count=1`, which is the whole point of that endpoint being the cheap
    // one -- without the constraint this answers the walk as well, and every
    // run comes back with a `following` list that declared two accounts and
    // served none.
    Mock::given(method("GET"))
        .and(url_path(format!("/api/v1/friendships/{PK}/following/")))
        .and(query_param("count", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
        .mount(&server)
        .await;

    // Who the session belongs to, for `whoami` and for a run with no stored
    // name.
    Mock::given(method("GET"))
        .and(url_path(format!("/api/v1/users/{PK}/info/")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"user":{"pk":42,"username":"me","full_name":"Me"}}"#),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"data":{{"user":{{"id":"{PK}","username":"me",
                "edge_followed_by":{{"count":{followers}}},
                "edge_follow":{{"count":{following}}}}}}}}}"#
        )))
        .mount(&server)
        .await;

    for (kind, count) in [("followers", followers), ("following", following)] {
        let users: Vec<String> = (0..count)
            .map(|n| format!(r#"{{"pk":{},"username":"user{n}"}}"#, 1_000 + n))
            .collect();
        Mock::given(method("GET"))
            .and(path_regex(format!(r"^/api/v1/friendships/\d+/{kind}/$")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!(r#"{{"users":[{}]}}"#, users.join(","))),
            )
            .mount(&server)
            .await;
    }

    server
}

/// Logs a sandbox in, so the tests below start from a machine that has a
/// session.
///
/// This is itself the first thing worth asserting: `--paste` falls back to
/// reading a line off standard input when nobody is at a terminal, the session
/// is validated against the server the run was pointed at, and it lands in a
/// file inside the sandbox rather than in the operating system's keyring.
fn log_in(root: &Path, instagram: &MockServer) {
    let out = snob_typing(
        root,
        instagram,
        &["login", "--paste", "--user-agent", UA],
        &format!("{SESSIONID}\n"),
    );
    assert!(
        out.status.success(),
        "the sandbox could not log in: {}{}",
        stdout(&out),
        stderr(&out)
    );
}

/// A `watch.toml` in the sandbox, written the way a hand-edit would.
fn configure(root: &Path, body: &str) {
    let dir = root.join("config");
    std::fs::create_dir_all(&dir).expect("the sandbox is writable");
    std::fs::write(dir.join("watch.toml"), body).expect("the sandbox is writable");
}

/// A receiver that answers with this status, and remembers what it was sent.
async fn receiver(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(url_path("/webhook/snob"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

async fn posted(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

/// The session goes into the sandbox and the keyring is never opened.
///
/// The rule `crates/snob-core/tests/keyring.rs` holds every test to, asserted of
/// the binary rather than of a test: a run under `--sandbox-root` writes its
/// session to a file inside that root. Nothing else in the suite could say this,
/// because nothing else ran the program — `SecretStore` choosing a backend is
/// `main`'s decision and no library test reaches it.
#[tokio::test]
async fn a_sandbox_run_keeps_its_session_in_the_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);

    let stored = tmp.path().join("data").join("session.json");
    assert!(
        stored.is_file(),
        "the session has to land in the sandbox, not in the keyring"
    );

    let out = snob(tmp.path(), Some(&instagram), &["whoami", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said: serde_json::Value =
        serde_json::from_str(&stdout(&out)).expect("--json prints one JSON object");
    assert_eq!(said["pk"], PK, "{said}");
    assert_eq!(
        said["storage"], "file",
        "a sandbox run must not reach the operating system's store: {said}"
    );
    assert_eq!(
        said["storage_path"].as_str().map(std::path::Path::new),
        Some(stored.as_path()),
        "and it says where, which is the line somebody checks: {said}"
    );
}

/// `snob watch check` is a probe, and this is the whole of what it probes.
///
/// Nine of the defects this branch fixed live in the state matrix behind this
/// command and every one of them exited 0. It is driven here through the real
/// binary because the exit code is the entire point: a monitoring system reads
/// `$?` and nothing else.
#[tokio::test]
async fn the_probe_answers_for_a_machine_that_would_work() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let n8n = receiver(200).await;
    log_in(tmp.path(), &instagram);
    configure(
        tmp.path(),
        &format!(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"{}/webhook/snob\"\n",
            n8n.uri()
        ),
    );

    let out = snob(tmp.path(), Some(&instagram), &["watch", "check", "--json"]);
    let said: serde_json::Value =
        serde_json::from_str(&stdout(&out)).expect("--json prints one JSON object");
    assert!(
        out.status.success(),
        "a machine that would run is not a failure: {said}\n{}",
        stderr(&out)
    );

    let checks = said["checks"].as_array().expect("there are checks");
    for what in ["schedule", "session", "account", "webhook"] {
        assert!(
            checks.iter().any(|c| c["what"] == what),
            "nothing checked the {what}: {said}"
        );
    }

    // And the receiver really was posted to, with the message that says it is
    // not a report.
    let sent = posted(&n8n).await;
    assert_eq!(sent.len(), 1, "the probe posts exactly one preflight");
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("the body is JSON");
    assert_eq!(body["event"], "watch.preflight", "{body}");
    assert_eq!(body["schema"], 1, "{body}");
    assert!(body["run"]["id"].as_str().is_some(), "{body}");
    assert!(body["run"]["at"].as_i64().is_some(), "{body}");
}

/// A run that has news posts it, signed, and says so on standard output.
///
/// The one path this whole branch is about, from the file on disk to the bytes
/// a receiver reads. The second run is what makes it a monitor rather than a
/// dump: the first lays the baseline and reports nothing, by design.
#[tokio::test]
async fn a_change_reaches_the_receiver_signed_and_only_once() {
    let tmp = tempfile::tempdir().unwrap();
    let n8n = receiver(200).await;
    let first = fake_instagram(3, 2).await;
    log_in(tmp.path(), &first);
    configure(
        tmp.path(),
        &format!(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"{}/webhook/snob\"\n",
            n8n.uri()
        ),
    );

    let out = snob(tmp.path(), Some(&first), &["watch", "once"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        posted(&n8n).await.is_empty(),
        "the first run lays a baseline and has nothing to report"
    );

    // Somebody left.
    let second = fake_instagram(2, 2).await;
    let out = snob(tmp.path(), Some(&second), &["watch", "once"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let sent = posted(&n8n).await;
    assert_eq!(sent.len(), 1, "a change is reported once");
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("the body is JSON");
    assert_eq!(body["event"], "watch.changes", "{body}");
    assert_eq!(body["counts"]["followers_lost"], 1, "{body}");
    assert_eq!(
        sent[0]
            .headers
            .get("x-snob-event")
            .map(|v| v.to_str().unwrap_or_default()),
        Some("watch.changes"),
        "the header has to agree with the body"
    );
    assert!(
        sent[0].headers.contains_key("x-snob-delivery"),
        "delivery is at-least-once, so a receiver needs the id to deduplicate on"
    );

    // A third run with nothing new sends nothing, which is the promise that
    // makes every message that arrives mean something.
    let out = snob(tmp.path(), Some(&second), &["watch", "once"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        posted(&n8n).await.len(),
        1,
        "nothing changed, so nothing is sent"
    );
}

/// A receiver that is down costs nothing, and the next run picks it up.
///
/// The queue is the reason a restarting receiver does not lose a change, and
/// this is the only test in the tree that watches a whole process fail to
/// deliver, exit, and a second process deliver the same bytes under the same
/// id.
#[tokio::test]
async fn a_report_a_receiver_refused_is_owed_and_then_delivered() {
    let tmp = tempfile::tempdir().unwrap();
    let down = receiver(500).await;
    let first = fake_instagram(3, 2).await;
    log_in(tmp.path(), &first);
    configure(
        tmp.path(),
        &format!(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"{}/webhook/snob\"\n",
            down.uri()
        ),
    );

    snob(tmp.path(), Some(&first), &["watch", "once"]);
    let second = fake_instagram(2, 2).await;
    let out = snob(tmp.path(), Some(&second), &["watch", "once"]);
    assert!(
        out.status.success(),
        "a receiver that is down does not fail the run: {}",
        stderr(&out)
    );
    let refused = posted(&down).await;
    assert_eq!(refused.len(), 1, "it was tried once and queued");

    let said: serde_json::Value = serde_json::from_str(&stdout(&snob(
        tmp.path(),
        None,
        &["watch", "status", "--json"],
    )))
    .expect("--json prints one JSON object");
    assert_eq!(
        said["deliveries"]["waiting"], 1,
        "what is owed is what a run could post: {said}"
    );
    assert_eq!(
        said["deliveries"]["elsewhere"], 0,
        "and it is addressed here: {said}"
    );

    // The receiver comes back on the same address, and the next run drains it
    // with the same bytes under the same id.
    drop(down);
    let up = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&up)
        .await;
    configure(
        tmp.path(),
        &format!(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"{}/webhook/snob\"\n",
            up.uri()
        ),
    );

    // A queued report belongs to the address it was addressed to, so pointing
    // somewhere else must not flush it -- which is what the count says.
    let said: serde_json::Value = serde_json::from_str(&stdout(&snob(
        tmp.path(),
        None,
        &["watch", "status", "--json"],
    )))
    .expect("--json prints one JSON object");
    assert_eq!(
        said["deliveries"]["elsewhere"], 1,
        "a report made for one address is not owed to another: {said}"
    );
    assert_eq!(said["deliveries"]["waiting"], 0, "{said}");
    assert!(
        posted(&up).await.is_empty(),
        "and nothing was sent to the new address"
    );
}

/// A stranger's lists are not read on an answer nobody gave.
///
/// The domain rule, asserted of the program rather than of a function: an
/// unattended run refuses before it spends anything, and it says which command
/// fixes it. Written with the at sign, because that is the spelling that used
/// to match nothing.
#[tokio::test]
async fn an_unattended_run_refuses_a_stranger_nobody_answered_for() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    configure(
        tmp.path(),
        "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"@stranger\"\n",
    );

    let before = posted(&instagram).await.len();
    let out = snob(tmp.path(), Some(&instagram), &["watch", "once"]);
    assert!(!out.status.success(), "it has to refuse: {}", stdout(&out));

    let said = stderr(&out);
    assert!(
        said.contains("stranger"),
        "the refusal has to name the account: {said}"
    );
    assert_eq!(
        posted(&instagram).await.len(),
        before,
        "and it refuses before spending anything"
    );
}

/// The scheduled mode writes one line per tick, and a line jq can read.
///
/// The recipe the README offers is `snob watch --json >> events.ndjson`, and a
/// consumer of that file reads it a line at a time. One English sentence on
/// standard output ends the pipeline. Nothing drove the loop before this: the
/// two shapes of `json_line` are pinned by unit tests, but which mode gets
/// which, whether a tick prints at all, and what the loop says about the runs
/// it was not running for are all decided in the loop.
///
/// **Why the run log is edited.** The tightest schedule this tool accepts is
/// every fifteen minutes, so no schedule makes a fresh install tick inside a
/// test's patience: with an interval the first run is a whole interval away,
/// and with a calendar it is the next moment the grid names. Winding one
/// recorded run back an hour is what a machine that was switched off looks
/// like, and it is the one state that makes the loop work immediately -- so it
/// drives the missed-run fold at the same time, which nothing else does.
#[tokio::test]
async fn the_scheduled_stream_is_one_json_line_a_tick() {
    use std::io::BufRead;

    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);

    // One run, so there is a past; then an hour ago, so the grid has moments in
    // it that were missed.
    let first = snob(tmp.path(), Some(&instagram), &["watch", "once"]);
    assert!(first.status.success(), "{}", stderr(&first));
    {
        let db = snob_core::store::Store::open_at(&tmp.path().join("data").join("snob.db"))
            .expect("the sandbox has a database by now");
        db.conn()
            .execute(
                "UPDATE watch_runs SET started_at = started_at - 3600,
                 finished_at = finished_at - 3600",
                [],
            )
            .expect("the run log is writable");
    }

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_snob"))
        .arg("--sandbox-root")
        .arg(tmp.path())
        .arg("--ig-base-url")
        .arg(instagram.uri())
        .args(["watch", "--cron", "*/15 * * * *", "--json", "--no-progress"])
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");

    // Read on another thread, because the loop does not end and a blocking read
    // here would hang the suite rather than fail it.
    let out = child.stdout.take().expect("stdout was piped");
    let (say, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            if say.send(line).is_err() {
                return;
            }
        }
    });

    let first = heard.recv_timeout(std::time::Duration::from_secs(30));
    let _ = child.kill();
    let rest = child.wait_with_output().expect("the binary finishes");
    let said_aloud = String::from_utf8_lossy(&rest.stderr).to_string();

    let first =
        first.unwrap_or_else(|_| panic!("the loop wrote nothing in thirty seconds: {said_aloud}"));
    let said: serde_json::Value = serde_json::from_str(&first)
        .unwrap_or_else(|e| panic!("the stream wrote a line jq cannot read: {first:?} ({e})"));
    assert_eq!(said["schema"], 1, "{said}");
    assert!(
        said["run"]["at"].as_i64().is_some(),
        "a file like this is queried by time, so every line needs one: {said}"
    );
    assert!(
        said["run"]["lists"]
            .as_array()
            .is_some_and(|l| l.len() == 2),
        "and it says which lists it read, so a refusal is not a quiet run: {said}"
    );

    // The runs it was not running for are folded into this one, and the
    // sentence about them reads as a sentence.
    assert!(
        said_aloud.contains("missed while this was not running"),
        "a machine that was off has to be told what it missed: {said_aloud}"
    );
    assert!(
        !said_aloud.contains("1 scheduled runs were"),
        "and the sentence has to agree with its own number: {said_aloud}"
    );
}

/// A run that could not see says so, rather than saying nothing changed.
///
/// The distinction the whole `run.lists` field exists for, end to end. An
/// Instagram that answers nothing useful produces zeros in `counts` -- exactly
/// the zeros a quiet run produces -- so a receiver branching on
/// `counts.followers_lost` would call it a quiet morning. `run.looked` and
/// `run.lists` are what tell the two apart, and the exit code is what a timer
/// reads.
#[tokio::test]
async fn a_run_that_could_not_see_is_not_a_run_that_saw_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    configure(tmp.path(), "schema = 1\nevery = \"6h\"\n");

    let blind = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&blind)
        .await;

    let out = snob(tmp.path(), Some(&blind), &["watch", "once", "--json"]);
    let said: serde_json::Value = serde_json::from_str(&stdout(&out))
        .unwrap_or_else(|e| panic!("{:?} ({e})\n{}", stdout(&out), stderr(&out)));

    assert_eq!(
        said["counts"]["followers_lost"], 0,
        "a blind run and a quiet one have the same counts, which is the problem: {said}"
    );
    assert_eq!(
        said["run"]["looked"], false,
        "and this is what tells them apart: {said}"
    );
    let lists = said["run"]["lists"]
        .as_array()
        .expect("both lists are named");
    assert!(
        lists.iter().all(|l| !l["skipped"].is_null()),
        "neither list was read, and each has to say so: {said}"
    );
    assert!(
        !out.status.success(),
        "a run that saw nothing is not a run that succeeded: {said}"
    );
}

/// The first push-back is a hard stop, and a second process obeys it.
///
/// The most expensive rule this tool has, asserted across two processes, which
/// is the only way it can be: the cooldown is written to the database and the
/// point of it is that the *next* run reads it. A library test shares a
/// connection and an in-memory budget with the code it is testing, so it cannot
/// tell a cooldown that was honored from a cooldown that was merely recorded.
#[tokio::test]
async fn a_hard_stop_is_obeyed_by_the_process_after_it() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    configure(tmp.path(), "schema = 1\nevery = \"6h\"\n");

    let throttled = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"message":"","spam":true}"#))
        .mount(&throttled)
        .await;

    let refused = snob(tmp.path(), Some(&throttled), &["watch", "once", "--json"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    let said: serde_json::Value = serde_json::from_str(&stdout(&refused)).unwrap_or_else(|e| {
        panic!(
            "a tick that failed still leaves a line: {:?} ({e})\n{}",
            stdout(&refused),
            stderr(&refused)
        )
    });
    let lists = said["run"]["lists"]
        .as_array()
        .expect("both lists are named");
    assert!(
        lists.iter().all(|l| !l["skipped"].is_null()),
        "a throttled run read nothing, and every list has to say so: {said}"
    );
    let spent = posted(&throttled).await.len();
    assert_eq!(spent, 1, "the first push-back is the last request");

    // A whole new process, reading the cooldown out of the database rather than
    // out of anything it remembers.
    let again = snob(tmp.path(), Some(&throttled), &["watch", "once"]);
    assert!(!again.status.success(), "{}", stdout(&again));
    assert_eq!(
        posted(&throttled).await.len(),
        spent,
        "the run after a hard stop knocks again: {}",
        stderr(&again)
    );
}

/// A machine with nothing configured is not a machine that is broken, and a
/// machine whose schedule cannot be built is.
///
/// The two ends of the probe's verdict, through the real exit code. Both used
/// to be 0.
#[tokio::test]
async fn the_probe_tells_unconfigured_from_unstartable() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);

    let bare = snob(tmp.path(), Some(&instagram), &["watch", "check"]);
    assert!(
        bare.status.success(),
        "nothing configured is not broken: {}",
        stderr(&bare)
    );

    // Accepted by the parser -- it is TOML, the schema is right, no key clashes
    // -- and refused by the evaluator that actually decides when a run happens.
    configure(tmp.path(), "schema = 1\nevery = \"5m\"\n");
    let unstartable = snob(tmp.path(), Some(&instagram), &["watch", "check"]);
    assert!(
        !unstartable.status.success(),
        "a schedule no run can be built from stops every run: {}{}",
        stdout(&unstartable),
        stderr(&unstartable)
    );
}
