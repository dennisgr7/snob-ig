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
//! because a neighbor ran first.
#![cfg(feature = "testing")]

use std::path::Path;
use std::process::{Command, Output};

use snob_core::Pk;
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
const PK: Pk = Pk::new(42);

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
    assert_eq!(said["pk"], PK.get(), "{said}");
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

    // A second receiver, on an address of its own.
    //
    // **Started before the first is dropped, and that ordering is the test.**
    // Taking `down` away first frees its port, and an operating system is
    // entitled to hand the very same one straight back — macOS does, reliably
    // enough that this failed there and passed on Linux and Windows. Both
    // servers then have one address, the queued row is addressed to it after
    // all, and the assertion below reads `waiting: 1` where it wants
    // `elsewhere: 1`: a test about two addresses, quietly run against one.
    let up = MockServer::start().await;
    drop(down);
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
        let db = snob_store::store::Store::open_at(&tmp.path().join("data").join("snob.db"))
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

/// A reel with one photo and one video, mounted on an existing fake Instagram.
///
/// Two candidate sizes on the photo so the listing has something to choose
/// wrongly: the client is told to take the largest, and taking the first is the
/// mistake that would otherwise pass every assertion.
async fn with_stories(server: &MockServer) {
    Mock::given(method("GET"))
        .and(url_path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"reels_media":[{"items":[
                {"pk":"1","media_type":1,"taken_at":1000,"expiring_at":99999999999,
                 "image_versions2":{"candidates":[
                    {"url":"https://scontent.cdninstagram.com/small.jpg","width":320,"height":320},
                    {"url":"https://scontent.cdninstagram.com/big.jpg","width":1080,"height":1920}]}},
                {"pk":"2","media_type":2,"taken_at":2000,"expiring_at":99999999999,
                 "video_versions":[
                    {"url":"https://scontent.cdninstagram.com/clip.mp4","width":720,"height":1280}]}
            ]}]}"#,
        ))
        .mount(server)
        .await;
}

/// The listing numbers the stories, and those numbers are what `--download`
/// takes.
#[tokio::test]
async fn stories_are_listed_with_the_numbers_download_takes() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_stories(&instagram).await;
    log_in(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["stories", "me", "--format", "json"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));

    let listed: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("it is JSON");
    let stories = listed["stories"].as_array().expect("an array");
    assert_eq!(stories.len(), 2);
    assert_eq!(stories[0]["number"], 1);
    assert_eq!(stories[0]["kind"], "photo");
    assert_eq!(stories[1]["kind"], "video");
    assert_eq!(
        stories[0]["url"], "https://scontent.cdninstagram.com/big.jpg",
        "the largest candidate wins, not the first"
    );
}

/// **Nothing that would register a view goes out.** The whole reason the
/// command exists in the shape it does, asserted against what the server
/// actually received rather than against what the code appears to do.
#[tokio::test]
async fn listing_stories_sends_nothing_that_marks_them_seen() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_stories(&instagram).await;
    log_in(tmp.path(), &instagram);

    snob(tmp.path(), Some(&instagram), &["stories", "me"]);

    for request in posted(&instagram).await {
        assert!(
            !request.url.path().contains("seen"),
            "a request went to {} while only reading stories",
            request.url.path()
        );
        assert_eq!(
            request.method,
            wiremock::http::Method::GET,
            "reading stories sent a {} to {}",
            request.method,
            request.url.path()
        );
    }
}

/// A number nobody has is refused by name rather than by panic, and an
/// out-of-range one does not wrap round to the last story.
///
/// Zero is refused earlier than nine, and differently: the value parser
/// answers it, so it is exit 2 with nothing fetched, while nine parses and is
/// refused against the tray. Both were "there is no story" until `-d` learned
/// sets and ranges, which moved the zero check to where a typo costs nothing.
#[tokio::test]
async fn a_story_number_nobody_has_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_stories(&instagram).await;
    log_in(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["stories", "me", "--download", "0"],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "zero is a parse error, answered before anything was spent"
    );
    assert!(
        stderr(&out).contains("the listing starts at 1"),
        "{}",
        stderr(&out)
    );

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["stories", "me", "--download", "9"],
    );
    assert!(
        !out.status.success(),
        "story 9 should not have been downloaded"
    );
    assert!(
        stderr(&out).contains("there is no story"),
        "{}",
        stderr(&out)
    );

    // A set with one bad number downloads nothing: the refusal comes before
    // the first request, not after two good stories are already on disk.
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["stories", "me", "--download", "1,9"],
    );
    assert!(!out.status.success(), "1,9 should refuse as a whole");
    assert!(
        stderr(&out).contains("there is no story 9"),
        "{}",
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("Saved") && !stdout(&out).contains("Saved"),
        "nothing may be saved when part of the set is refused"
    );
}

/// A reel whose media the fake Instagram itself serves, so a download has
/// somewhere to go. The client downloads from the origin it was pointed at
/// as readily as from the CDN, which is what makes this reachable offline.
async fn with_downloadable_stories(server: &MockServer) {
    let base = server.uri();
    Mock::given(method("GET"))
        .and(url_path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"reels_media":[{{"items":[
                {{"pk":"1","media_type":1,"taken_at":1000,"expiring_at":99999999999,
                 "image_versions2":{{"candidates":[
                    {{"url":"{base}/big.jpg","width":1080,"height":1920}}]}}}},
                {{"pk":"2","media_type":2,"taken_at":2000,"expiring_at":99999999999,
                 "video_versions":[
                    {{"url":"{base}/clip.mp4","width":720,"height":1280}}]}}
            ]}}]}}"#
        )))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/big.jpg"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(b"\xff\xd8\xff\xe0 a picture".to_vec()),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/clip.mp4"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(b"\x00\x00\x00\x18ftypisom a clip".to_vec()),
        )
        .mount(server)
        .await;
}

/// The binary, run from a directory of the test's choosing -- which is where a
/// story lands when `-o` says nothing.
fn snob_from(cwd: &Path, root: &Path, instagram: &MockServer, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_snob"));
    command.arg("--sandbox-root").arg(root);
    command.arg("--ig-base-url").arg(instagram.uri());
    command.args(args);
    command.env("NO_COLOR", "1");
    command.current_dir(cwd);
    command.output().expect("the binary runs")
}

/// The whole tool, end to end, in the order a person meets it.
///
/// One session, one fake Instagram, every read command the binary has, each
/// asserted on what it printed, what it wrote and what it exited with -- and
/// then the two commands that take it all away again. The other tests here
/// each pin one behavior; this one pins that the commands agree with each
/// other over one store: the crossings add up to the lists, the export on
/// disk is the list on screen, and `purge` leaves nothing of any of it.
///
/// The fixture serves the same made-up accounts for both lists: followers
/// `user0..user2`, following `user0..user1`. So `fans` is `user2`, `friends`
/// is `user0` and `user1`, and nobody is an unfollower.
#[tokio::test]
async fn the_whole_tool_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let here = tmp.path().join("here");
    std::fs::create_dir(&here).unwrap();
    let run = |args: &[&str]| snob_from(&here, tmp.path(), &instagram, args);
    let json = |out: &Output| -> serde_json::Value {
        serde_json::from_str(&stdout(out))
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}{}", stdout(out), stderr(out)))
    };
    let names = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .expect("a list")
            .iter()
            .map(|u| u["username"].as_str().unwrap().to_string())
            .collect()
    };

    // Nothing yet: every command that needs a session says so, with the code.
    let out = run(&["followers"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));

    log_in(tmp.path(), &instagram);

    let out = run(&["whoami", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(json(&out)["username"], "me");

    // The two lists, walked. Down a pipe the format is JSON on its own.
    let out = run(&["followers"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(names(&json(&out)), ["user0", "user1", "user2"]);
    let out = run(&["following"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(names(&json(&out)), ["user0", "user1"]);

    // The crossings, served out of what was just stored. The arithmetic is
    // the one AGENTS.md says a test asserts: unfollowers + friends is
    // everyone you follow, fans + friends everyone who follows you.
    let out = run(&["unfollowers", "--cache"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(names(&json(&out)).is_empty(), "{}", stdout(&out));
    let out = run(&["fans", "--cache"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(names(&json(&out)), ["user2"]);
    let out = run(&["friends", "--cache"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(names(&json(&out)), ["user0", "user1"]);

    let out = run(&["scan", "--cache", "--format", "json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let scan = json(&out);
    assert_eq!(scan["counts"]["followers"], 3, "{scan}");
    assert_eq!(scan["counts"]["following"], 2, "{scan}");
    assert_eq!(scan["counts"]["fans"], 1, "{scan}");
    assert_eq!(scan["counts"]["friends"], 2, "{scan}");
    assert_eq!(scan["counts"]["unfollowers"], 0, "{scan}");
    assert_eq!(scan["lists"]["followers"]["source"], "cached", "{scan}");

    // The filters, on the list with the most in it.
    let out = run(&["followers", "--cache", "--limit", "1"]);
    assert_eq!(names(&json(&out)), ["user0"]);
    std::fs::write(here.join("skip.txt"), "@ user1\n# a comment\nUSER0\n").unwrap();
    let out = run(&["followers", "--cache", "--exclude-list", "skip.txt"]);
    assert_eq!(names(&json(&out)), ["user2"], "{}", stderr(&out));

    // Every export format, to a file whose extension chooses it.
    for (file, expect) in [
        ("list.csv", "user0"),
        ("list.md", "| [user0](https://www.instagram.com/user0/)"),
        ("list.ndjson", "\"username\":\"user0\""),
        ("list.json", "\"username\": \"user0\""),
    ] {
        let out = run(&["followers", "--cache", "-o", file]);
        assert!(out.status.success(), "{file}: {}", stderr(&out));
        let written = std::fs::read_to_string(here.join(file)).expect(file);
        assert!(written.contains(expect), "{file}: {written}");
        assert_eq!(
            stdout(&out),
            "",
            "{file}: the result went to the file, not the screen"
        );
    }
    let out = run(&["followers", "--cache", "-o", "list.xlsx"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let workbook = std::fs::read(here.join("list.xlsx")).unwrap();
    assert_eq!(&workbook[..2], b"PK", "an xlsx is a zip");
    // A name this program did not choose is not written over twice: the
    // user named it, so the second run replaces it without complaint.
    assert!(
        run(&["followers", "--cache", "-o", "list.csv"])
            .status
            .success()
    );

    // The session, taken away, and the command that needed it says so again.
    let out = run(&["logout"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Session deleted"), "{}", stdout(&out));
    let out = run(&["followers", "--cache"]);
    assert_eq!(out.status.code(), Some(3));

    // And everything else: shown first, then removed, only on --yes.
    let out = run(&["purge", "--dry-run"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Nothing was deleted"),
        "{}",
        stdout(&out)
    );
    assert!(
        tmp.path().join("data").is_dir(),
        "a dry run deletes nothing"
    );
    let out = run(&["purge"]);
    assert_eq!(
        out.status.code(),
        Some(130),
        "a question nobody could answer is not a yes: {}",
        stderr(&out)
    );
    assert!(tmp.path().join("data").is_dir());
    let out = run(&["purge", "--yes"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(
        !tmp.path().join("data").exists(),
        "purge left the data directory behind"
    );
    // The exports are the user's, in the user's directory, and stay.
    assert!(here.join("list.csv").is_file());
}

/// A failure is told in the language the answer was going to be in.
///
/// `snob followers --format json` against nothing stored used to print
/// "No session stored." in English on standard error and exit 3 -- a stable
/// code and nothing a program could parse, not the hint and not the address
/// a challenge carries. The same run asked for prose gets the prose, with
/// the advice set apart as a hint.
#[test]
fn a_failure_is_json_when_the_answer_would_have_been() {
    let tmp = tempfile::tempdir().unwrap();

    let out = snob(tmp.path(), None, &["followers", "--format", "json"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let told: serde_json::Value =
        serde_json::from_str(stderr(&out).trim()).expect("standard error is one JSON object");
    assert_eq!(told["error"]["code"], "no_session");
    assert_eq!(told["error"]["exit"], 3);
    assert_eq!(told["error"]["message"], "no session is stored");
    assert_eq!(told["error"]["hint"], "run \"snob login\"");
    assert_eq!(
        stdout(&out),
        "",
        "nothing on standard output to mistake for a result"
    );

    let out = snob(tmp.path(), None, &["followers", "--format", "md"]);
    assert_eq!(out.status.code(), Some(3));
    let said = stderr(&out);
    assert!(said.contains("error: no session is stored"), "{said}");
    assert!(said.contains("hint:  run \"snob login\""), "{said}");
}

/// A reader that leaves early is not an error of this program's.
///
/// `println!` panics on a closed pipe, and with `panic = "abort"` a release
/// binary died with a message and whatever status an abort gets the moment
/// `head` had read its line. The lists went through a writer that tolerated
/// it; the prose did not.
#[test]
fn a_closed_pipe_ends_the_output_and_not_the_program() {
    use std::process::Stdio;

    let tmp = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_snob"))
        .arg("--sandbox-root")
        .arg(tmp.path())
        .args(["watch", "status"])
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    // Close the reading end before the program has written anything.
    drop(child.stdout.take());
    let out = child.wait_with_output().expect("the binary finishes");

    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!said.contains("panicked"), "{said}");
    assert!(
        out.status.code().is_some_and(|code| code <= 1),
        "a closed pipe exited {:?}: {said}",
        out.status
    );
}

/// **A story can actually be saved.** The name this command invents carries
/// a hyphen, and the allowlist the name is checked against did not -- so
/// every `--download` fetched the bytes and then refused its own name. No
/// test drove the command to the write, which is the only place that shows.
#[tokio::test]
async fn a_story_is_downloaded_under_the_name_the_listing_implies() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_stories(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = tmp.path().join("here");
    std::fs::create_dir(&here).unwrap();

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "--download", "1"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_eq!(
        std::fs::read(here.join("me-1.jpg")).expect("the story was written where the command ran"),
        b"\xff\xd8\xff\xe0 a picture"
    );

    // A second download of the same story neither replaces the file -- the
    // name was invented here, not typed -- nor fetches it: the look at the
    // directory comes before the request, so the CDN sees nothing.
    let fetched_before = instagram
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/big.jpg")
        .count();
    let again = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "--download", "1"],
    );
    assert!(
        again.status.success(),
        "{}{}",
        stdout(&again),
        stderr(&again)
    );
    assert!(
        stderr(&again).contains("Already saved"),
        "{}",
        stderr(&again)
    );
    let fetched_after = instagram
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/big.jpg")
        .count();
    assert_eq!(
        fetched_before, fetched_after,
        "the story was fetched again for a file already on disk"
    );
    assert_eq!(
        std::fs::read(here.join("me-1.jpg")).unwrap(),
        b"\xff\xd8\xff\xe0 a picture",
        "the file on disk was left alone"
    );
}

/// A download that stopped halfway leaves a `.part`, never a truncated file
/// under the real name -- which the look-before-fetch above would otherwise
/// take for a finished one. The next ask for the same story replaces it.
#[tokio::test]
async fn a_leftover_part_file_is_replaced_and_never_taken_for_the_story() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_stories(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = tmp.path().join("here");
    std::fs::create_dir(&here).unwrap();
    std::fs::write(here.join("me-1.part"), b"half of").unwrap();

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "--download", "1"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(
        !here.join("me-1.part").exists(),
        "the leftover was not cleaned up"
    );
    assert_eq!(
        std::fs::read(here.join("me-1.jpg")).unwrap(),
        b"\xff\xd8\xff\xe0 a picture"
    );
    assert!(
        stderr(&out).contains("Saved ") && !stderr(&out).contains("Already"),
        "{}",
        stderr(&out)
    );
}

/// `--all -o somewhere` puts every story in `somewhere`, and not in the
/// working directory next to an empty folder of that name.
#[tokio::test]
async fn all_stories_land_in_the_directory_that_was_named() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_stories(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = tmp.path().join("here");
    std::fs::create_dir(&here).unwrap();

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "--all", "-o", "saved"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(here.join("saved").join("me-1.jpg").is_file());
    assert!(here.join("saved").join("me-2.mp4").is_file());
    assert!(
        !here.join("me-1.jpg").exists(),
        "a story leaked into the working directory"
    );

    // Run again into the same directory: both are already there, both are
    // said to be, nothing is fetched, and the run is a success -- a second
    // `-d all` is the ordinary way of asking "anything new?", and it used to
    // download the whole tray again only to refuse every name.
    let fetched_before = instagram.received_requests().await.unwrap().len();
    let again = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "--all", "-o", "saved"],
    );
    assert!(again.status.success(), "{}", stderr(&again));
    let said = stderr(&again);
    assert_eq!(
        said.matches("Already saved").count(),
        2,
        "both stories should be reported as already saved: {said}"
    );
    let fetched_after = instagram.received_requests().await.unwrap().len();
    // The tray listing itself is one request; the two media files are none.
    assert!(
        fetched_after - fetched_before <= 2,
        "media was fetched again for files already on disk: {} requests",
        fetched_after - fetched_before
    );
    assert!(
        !instagram
            .received_requests()
            .await
            .unwrap()
            .iter()
            .skip(fetched_before)
            .any(|r| r.url.path() == "/big.jpg" || r.url.path() == "/clip.mp4"),
        "a media URL was requested on the second run"
    );
}

/// Stories download several at a time, and the report still reads in the
/// listing's order: one that the CDN refuses is named by its number among the
/// ones that were saved, not by whichever finished first.
#[tokio::test]
async fn a_failed_story_is_reported_by_number_among_the_saved_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let base = instagram.uri();
    Mock::given(method("GET"))
        .and(url_path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"reels_media":[{{"items":[
                {{"pk":"1","media_type":1,"taken_at":1000,"expiring_at":99999999999,
                 "image_versions2":{{"candidates":[
                    {{"url":"{base}/one.jpg","width":1080,"height":1920}}]}}}},
                {{"pk":"2","media_type":1,"taken_at":2000,"expiring_at":99999999999,
                 "image_versions2":{{"candidates":[
                    {{"url":"{base}/gone.jpg","width":1080,"height":1920}}]}}}},
                {{"pk":"3","media_type":1,"taken_at":3000,"expiring_at":99999999999,
                 "image_versions2":{{"candidates":[
                    {{"url":"{base}/three.jpg","width":1080,"height":1920}}]}}}}
            ]}}]}}"#
        )))
        .mount(&instagram)
        .await;
    for name in ["/one.jpg", "/three.jpg"] {
        Mock::given(method("GET"))
            .and(url_path(name))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(b"\xff\xd8\xff\xe0 a picture".to_vec()),
            )
            .mount(&instagram)
            .await;
    }
    Mock::given(method("GET"))
        .and(url_path("/gone.jpg"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&instagram)
        .await;
    log_in(tmp.path(), &instagram);
    let here = tmp.path().join("here");
    std::fs::create_dir(&here).unwrap();

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "-d", "all"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(here.join("me-1.jpg").is_file(), "{said}");
    assert!(here.join("me-3.jpg").is_file(), "{said}");
    assert!(!here.join("me-2.jpg").exists());
    assert!(
        !here.join("me-2.part").exists(),
        "a refused download left its partial behind: {said}"
    );
    assert!(
        said.contains("1 of 3 stories could not be downloaded"),
        "{said}"
    );
    // Down a pipe the failure is the JSON shape, so the newline is escaped.
    assert!(
        said.contains("\\n2: ") || said.contains("\n2: "),
        "the failure is named by number: {said}"
    );
    assert!(
        said.contains("Saved 1: ") && said.contains("Saved 3: "),
        "{said}"
    );
}

/// The same sandbox login, with a CSRF token, which is what
/// `snob login --browser` produces and what a write needs.
fn log_in_writing(root: &Path, instagram: &MockServer) {
    let out = snob_typing(
        root,
        instagram,
        &[
            "login",
            "--paste",
            "--user-agent",
            UA,
            "--csrftoken",
            "SANDBOXTOKEN",
        ],
        &format!("{SESSIONID}\n"),
    );
    assert!(
        out.status.success(),
        "the sandbox could not log in to write: {}{}",
        stdout(&out),
        stderr(&out)
    );
}

/// **A session that cannot write refuses before it spends anything.**
///
/// `log_in` pastes a sessionid and nothing else, which is exactly the session
/// `snob login --paste` produces, so this is the ordinary case rather than a
/// contrived one. The assertion that matters is the second: no request was
/// made, so no budget was charged and nothing reached Instagram.
#[tokio::test]
async fn a_write_without_a_csrf_token_never_reaches_instagram() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let before = posted(&instagram).await.len();

    let out = snob(tmp.path(), Some(&instagram), &["unfollow", "someone", "-y"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("CSRF"), "{}", stderr(&out));
    assert_eq!(
        posted(&instagram).await.len(),
        before,
        "the refusal must happen before any request goes out"
    );
}

/// With no terminal and no -y, a write is not made and the exit code says who
/// decided: 130, stopped by the user, rather than 1, failed.
#[tokio::test]
async fn a_write_nobody_could_confirm_is_not_made() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "someone"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"user":{"id":"9001","username":"someone","followed_by_viewer":false}}}"#,
        ))
        // Ahead of the catch-all mounted by `fake_instagram`, which answers for
        // any username with the session's own account. At equal priority
        // wiremock takes the first matching mount, and that one is first.
        .with_priority(1)
        .mount(&instagram)
        .await;
    log_in_writing(tmp.path(), &instagram);

    let out = snob(tmp.path(), Some(&instagram), &["follow", "someone"]);
    assert_eq!(
        out.status.code(),
        Some(130),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        posted(&instagram)
            .await
            .iter()
            .all(|r| r.method != wiremock::http::Method::POST),
        "nothing may be sent without an answer"
    );
}

/// Puts the mutation ids in the sandbox's database, as a machine that has
/// already discovered them would have.
///
/// **Discovery cannot be exercised here and that is deliberate.** It walks
/// `static.cdninstagram.com`, and the host is fixed in `graphql::bundles_in`
/// rather than taken from the page — the property that makes walking a
/// document's URLs safe at all, and therefore one that cannot be pointed at a
/// mock server without giving it up. The walk's two halves are pure functions
/// with tests of their own; what this file exercises is the request built from
/// what they found.
///
/// Written through the same `Store` the binary uses, so the row lands where the
/// run will look for it rather than where the test thinks it should.
fn remember_doc_ids(root: &Path) {
    let paths = snob_store::paths::AppPaths::rooted_at(root);
    let db = snob_store::store::Store::open(&paths).expect("the sandbox database opens");
    for (name, id) in [
        ("usePolarisFollowMutation", "26508036048874888"),
        ("usePolarisUnfollowMutation", "27789106940691111"),
    ] {
        db.remember(&format!("graphql.doc_id.{name}"), id)
            .expect("the sandbox database is writable");
    }
}

/// A confirmed unfollow sends one POST, to the right path, with the token.
#[tokio::test]
async fn a_confirmed_unfollow_sends_one_post() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    // The page that hands out the tokens a mutation needs, and the mutation.
    Mock::given(method("GET"))
        .and(url_path("/someone/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html>{"define":[["DTSGInitData",[],{"token":"DTSG"},1],
               ["LSD",[],{"token":"LSD"},2]]}
               <script src="https://static.cdninstagram.com/rsrc.php/v4/a.js"></script></html>"#,
        ))
        .with_priority(1)
        .mount(&instagram)
        .await;
    Mock::given(method("POST"))
        .and(url_path("/api/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"result":"unfollowed","status":"ok"}"#),
        )
        .mount(&instagram)
        .await;
    // The profile has to say the relationship exists, or the command correctly
    // decides there is nothing to do and sends nothing.
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "someone"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"user":{"id":"9001","username":"someone","followed_by_viewer":true}}}"#,
        ))
        // Ahead of the catch-all mounted by `fake_instagram`, which answers for
        // any username with the session's own account. At equal priority
        // wiremock takes the first matching mount, and that one is first.
        .with_priority(1)
        .mount(&instagram)
        .await;
    log_in_writing(tmp.path(), &instagram);
    remember_doc_ids(tmp.path());

    let out = snob(tmp.path(), Some(&instagram), &["unfollow", "someone", "-y"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));

    let writes: Vec<_> = posted(&instagram)
        .await
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(writes.len(), 1, "one account, one write");
    assert_eq!(writes[0].url.path(), "/api/graphql");
    assert_eq!(
        writes[0].headers.get("x-csrftoken").unwrap(),
        "SANDBOXTOKEN"
    );
    let sent = String::from_utf8_lossy(&writes[0].body);
    assert!(
        sent.contains("fb_api_req_friendly_name=usePolarisUnfollowMutation"),
        "{sent}"
    );
    // The tokens really came off the page the run fetched, rather than from
    // anywhere else.
    assert!(sent.contains("fb_dtsg=DTSG"), "{sent}");
    assert!(sent.contains("target_user_id"), "{sent}");
}

/// A relationship that already holds costs no request at all, which matters
/// when a write slot is a quarter of an hour.
#[tokio::test]
async fn unfollowing_somebody_you_do_not_follow_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "stranger"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"user":{"id":"9002","username":"stranger","followed_by_viewer":false}}}"#,
        ))
        // Ahead of the catch-all mounted by `fake_instagram`, which answers for
        // any username with the session's own account. At equal priority
        // wiremock takes the first matching mount, and that one is first.
        .with_priority(1)
        .mount(&instagram)
        .await;
    log_in_writing(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["unfollow", "stranger", "-y"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("do not follow"), "{}", stderr(&out));

    assert!(
        posted(&instagram)
            .await
            .iter()
            .all(|r| r.method != wiremock::http::Method::POST),
        "nothing needed changing, so nothing should have been sent"
    );
}
