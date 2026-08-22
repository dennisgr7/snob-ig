//! End-to-end test of the cache policy.
//!
//! It is what decides how many requests each invocation costs, so it is checked
//! by counting what reaches the mock server rather than by looking at output.

use snob_core::Pk;
use snob_core::model::ListKind;
use snob_core::session::{Session, SessionOrigin};
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use snob_store::store::Store;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use snob_cli::app::{App, Viewer};
use snob_cli::cli::ListArgs;
use snob_cli::engine::cooldown::check_same_moment;
use snob_cli::engine::{self, ListOutcome, Provenance, ResultSource};

mod common;
use common::{SID, UA, args};

/// An app pointed at the mock server, over a database that outlives it.
///
/// A fresh one per invocation on purpose: that is what a second run of the
/// command really is, and it is the only way the stored snapshot gets to prove
/// it survives the process.
fn app(server: &MockServer, db: Store) -> App {
    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());

    App::for_test(
        client,
        db,
        Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        },
    )
}

fn open_db(root: &std::path::Path) -> Store {
    Store::open_at(&root.join("test.db")).unwrap()
}

/// A profile with whatever follower count is asked for.
async fn mount_profile(server: &MockServer, followers: u64) {
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"data":{{"user":{{"id":"42","username":"me",
                "edge_followed_by":{{"count":{followers}}},
                "edge_follow":{{"count":10}}}}}}}}"#
        )))
        .mount(server)
        .await;
}

async fn mount_list(server: &MockServer, how_many: u64) {
    let users: Vec<String> = (0..how_many)
        .map(|i| format!(r#"{{"pk":{i},"username":"u{i}"}}"#))
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/followers/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"users":[{}]}}"#, users.join(","))),
        )
        .mount(server)
        .await;
}

/// The other side of a crossing. Only the tests that walk both lists need it.
async fn mount_following(server: &MockServer, how_many: u64) {
    let users: Vec<String> = (0..how_many)
        .map(|i| format!(r#"{{"pk":{i},"username":"u{i}"}}"#))
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/following/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"users":[{}]}}"#, users.join(","))),
        )
        .mount(server)
        .await;
}

async fn execute(
    server: &MockServer,
    root: &std::path::Path,
    args: &ListArgs,
) -> anyhow::Result<(Vec<snob_core::model::User>, ListOutcome)> {
    let mut app = app(server, open_db(root));
    engine::list(&mut app, args, ListKind::Followers).await
}

/// How many requests the server has received so far.
async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn the_first_run_walks_the_list_and_later_ones_reuse() {
    let server = MockServer::start().await;
    mount_profile(&server, 30).await;
    mount_list(&server, 30).await;

    let tmp = tempfile::tempdir().unwrap();
    let args = args();

    // First: nothing stored, so it walks.
    let (found, outcome) = execute(&server, tmp.path(), &args).await.unwrap();
    assert_eq!(found.len(), 30);
    assert_eq!(outcome.source(), ResultSource::Fetched);
    let after_first = requests(&server).await;
    assert!(after_first >= 1);

    // Second, with the counter unchanged: a single request, the poll.
    let (found, outcome) = execute(&server, tmp.path(), &args).await.unwrap();
    assert_eq!(found.len(), 30);
    assert_eq!(outcome.source(), ResultSource::Cached);
    assert_eq!(
        requests(&server).await - after_first,
        1,
        "reusing must cost exactly one request"
    );
}

#[tokio::test]
async fn with_cache_the_network_is_not_touched() {
    let server = MockServer::start().await;
    mount_profile(&server, 30).await;
    mount_list(&server, 30).await;

    let tmp = tempfile::tempdir().unwrap();

    execute(&server, tmp.path(), &args()).await.unwrap();
    let before = requests(&server).await;

    let mut args = args();
    args.cache = true;
    let (found, outcome) = execute(&server, tmp.path(), &args).await.unwrap();

    assert_eq!(found.len(), 30);
    assert_eq!(outcome.source(), ResultSource::Cached);
    assert_eq!(outcome.requests, 0);
    assert_eq!(
        requests(&server).await,
        before,
        "--cache must not ask for anything"
    );
}

/// A crossing asks for two lists through one `App`, which is what `sets` and
/// `scan` really do — unlike `execute` above, which builds a fresh one per call
/// to imitate a second run of the command.
///
/// When both lists come out of storage, that is the whole cost of the run: one
/// answer names the account and carries both of its counters, so asking a second
/// time was asking Instagram the identical question about the identical account
/// seconds apart.
#[tokio::test]
async fn a_cached_crossing_asks_about_the_account_once() {
    let server = MockServer::start().await;
    mount_profile(&server, 30).await;
    mount_list(&server, 30).await;
    mount_following(&server, 30).await;

    let tmp = tempfile::tempdir().unwrap();
    let mut args = args();
    args.target = Some("someone".into());
    args.yes = true;

    // Populate both lists, in runs of their own.
    execute(&server, tmp.path(), &args).await.unwrap();
    {
        let mut app = app(&server, open_db(tmp.path()));
        engine::list(&mut app, &args, ListKind::Following)
            .await
            .unwrap();
    }
    let before = requests(&server).await;

    // Now the crossing, in one run, with both lists already stored.
    let mut app = app(&server, open_db(tmp.path()));
    let (_, followers) = engine::list(&mut app, &args, ListKind::Followers)
        .await
        .unwrap();
    let (_, following) = engine::list(&mut app, &args, ListKind::Following)
        .await
        .unwrap();

    assert_eq!(followers.source(), ResultSource::Cached);
    assert_eq!(following.source(), ResultSource::Cached);
    assert_eq!(
        requests(&server).await - before,
        1,
        "one answer names the account and counts it; the second list needs neither again"
    );
}

/// The counters are reused only while nothing has been spent. A walk takes
/// minutes, and a number read before it no longer says whether a stored list is
/// current — serving one on that evidence and calling it counter-verified is
/// the failure `Provenance` exists to stop.
#[tokio::test]
async fn a_walk_invalidates_the_counters_it_was_started_with() {
    let server = MockServer::start().await;
    mount_profile(&server, 30).await;
    mount_list(&server, 30).await;
    mount_following(&server, 30).await;

    let tmp = tempfile::tempdir().unwrap();
    let mut args = args();
    args.target = Some("someone".into());
    args.yes = true;

    let mut app = app(&server, open_db(tmp.path()));
    // Nothing stored, so this one walks.
    engine::list(&mut app, &args, ListKind::Followers)
        .await
        .unwrap();

    let before = requests(&server).await;
    engine::list(&mut app, &args, ListKind::Following)
        .await
        .unwrap();

    let profiles = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().contains("web_profile_info"))
        .count();
    assert_eq!(
        profiles, 2,
        "after a walk the counters have to be read again"
    );
    assert!(requests(&server).await > before);
}

/// `--cache` promises not to spend a request, so nothing in the run checked
/// whether the stored list is still true — and a list nobody checked must not
/// be crossed against another one.
///
/// This is the wiring behind the bug: two lists three months apart were crossed
/// with no date comparison at all, because the outcome said only "not a
/// cooldown" and `--cache` is not a cooldown. `snob unfollowers --cache` then
/// reported everyone who had followed the account since as an unfollower.
#[tokio::test]
async fn a_cached_pair_carries_no_evidence_and_cannot_be_crossed() {
    let server = MockServer::start().await;
    mount_profile(&server, 30).await;
    mount_list(&server, 30).await;

    let tmp = tempfile::tempdir().unwrap();
    execute(&server, tmp.path(), &args()).await.unwrap();

    let mut args = args();
    args.cache = true;
    let (_, outcome) = execute(&server, tmp.path(), &args).await.unwrap();

    assert_eq!(
        outcome.provenance,
        Provenance::CacheFlag,
        "nothing was spent finding out whether this is still true"
    );
    assert!(
        !outcome.provenance.describes_now(),
        "a list nobody checked cannot be crossed"
    );

    // What the rule then does with that is `check_same_moment`'s own tests;
    // what this one is for is the wiring, because the wiring is where the bug
    // was — the rule was right and never got told.
    assert!(
        check_same_moment(&outcome, &outcome).is_ok(),
        "one list is one moment"
    );
}

#[tokio::test]
async fn refresh_walks_again_even_with_no_changes() {
    let server = MockServer::start().await;
    mount_profile(&server, 30).await;
    mount_list(&server, 30).await;

    let tmp = tempfile::tempdir().unwrap();
    execute(&server, tmp.path(), &args()).await.unwrap();

    let mut args = args();
    args.refresh = true;
    let (_, outcome) = execute(&server, tmp.path(), &args).await.unwrap();

    assert_eq!(outcome.source(), ResultSource::Fetched);
}

/// `--refresh` reads "walk the list again", and a failed counter poll answered
/// it with whatever was stored: any age, exit 0, nothing on screen saying
/// which. The arm that serves the stored list returns above the statement that
/// consults the flag, so the flag was never reached.
///
/// The second half is the rule that arm exists for, still standing: without
/// the flag, a poll that failed is a reason to reuse rather than to walk.
#[tokio::test]
async fn refresh_walks_again_when_the_counter_poll_fails() {
    let server = MockServer::start().await;
    mount_profile(&server, 30).await;
    mount_list(&server, 30).await;

    let tmp = tempfile::tempdir().unwrap();
    execute(&server, tmp.path(), &args()).await.unwrap();

    // The profile endpoint stops answering. The list still does, which is the
    // case that matters: the counter is the cheap question, not the only one.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    mount_list(&server, 30).await;

    let mut refreshing = args();
    refreshing.refresh = true;
    let (found, outcome) = execute(&server, tmp.path(), &refreshing).await.unwrap();

    assert_eq!(
        outcome.provenance,
        Provenance::Walked,
        "the flag asked for a walk and nothing else can answer it"
    );
    assert_eq!(outcome.source(), ResultSource::Fetched);
    assert_eq!(found.len(), 30);

    let (_, outcome) = execute(&server, tmp.path(), &args()).await.unwrap();
    assert_eq!(
        outcome.provenance,
        Provenance::PollFailed,
        "without the flag, a service already in trouble is not walked"
    );
    assert!(
        !outcome.provenance.describes_now(),
        "and what it served cannot be crossed"
    );
}

#[tokio::test]
async fn an_old_snapshot_is_walked_again() {
    let server = MockServer::start().await;
    mount_profile(&server, 30).await;
    mount_list(&server, 30).await;

    let tmp = tempfile::tempdir().unwrap();
    execute(&server, tmp.path(), &args()).await.unwrap();

    // Age the snapshot past the limit.
    open_db(tmp.path())
        .conn()
        .execute("UPDATE snapshots SET taken_at = taken_at - 100000", [])
        .unwrap();

    let (_, outcome) = execute(&server, tmp.path(), &args()).await.unwrap();
    assert_eq!(
        outcome.source(),
        ResultSource::Fetched,
        "past the maximum age it has to walk again even if the counter has not moved"
    );
}

#[tokio::test]
async fn with_nothing_stored_cache_fails_instead_of_lying() {
    let server = MockServer::start().await;
    mount_profile(&server, 30).await;

    let tmp = tempfile::tempdir().unwrap();
    let mut args = args();
    args.cache = true;

    assert!(execute(&server, tmp.path(), &args).await.is_err());
}

/// `--cache` promises not to touch the network. Since the local username
/// lookup exists, that now covers resolving a named target too.
#[tokio::test]
async fn cache_with_a_named_target_stays_off_the_network() {
    let seed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        // The declared count matches what the list below actually serves. A
        // walk that ends far short of what was declared is truncation, not a
        // complete list, so a mismatch here would be testing the wrong thing.
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"user":{"id":99,"username":"ghost",
                "edge_followed_by":{"count":2},"edge_follow":{"count":1}}}}"#,
        ))
        .mount(&seed)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/99/followers/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"users":[{"pk":1,"username":"u1"},{"pk":2,"username":"u2"}]}"#,
            ),
        )
        .mount(&seed)
        .await;

    let tmp = tempfile::tempdir().unwrap();

    let mut named = args();
    named.target = Some("@ghost".into());
    execute(&seed, tmp.path(), &named).await.unwrap();

    // A server with nothing mounted: any request would fail loudly.
    let empty = MockServer::start().await;
    let mut cached = args();
    cached.target = Some("@Ghost".into());
    cached.cache = true;
    let (found, outcome) = execute(&empty, tmp.path(), &cached).await.unwrap();

    assert_eq!(found.len(), 2);
    assert_eq!(outcome.source(), ResultSource::Cached);
    assert_eq!(requests(&empty).await, 0);
}

#[tokio::test]
async fn cache_with_an_unknown_target_fails_without_the_network() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();

    let mut args = args();
    args.cache = true;
    args.target = Some("@nobody".into());
    let error = execute(&server, tmp.path(), &args).await.unwrap_err();

    assert!(
        error.to_string().contains("--cache says not to look"),
        "{error}"
    );
    assert_eq!(
        snob_cli::exit::ExitCode::from_chain(&error),
        Some(snob_cli::exit::ExitCode::Error),
        "and it carries the code and the advice its four siblings carry"
    );
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn the_page_cap_leaves_the_list_marked_incomplete() {
    let server = MockServer::start().await;
    mount_profile(&server, 500).await;
    // There are always more pages, with distinct cursors.
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/followers/"))
        .and(query_param("count", "50"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"users":[{"pk":1,"username":"u1"}],"next_max_id":"next"}"#),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();

    let mut args = args();
    args.max_pages = Some(1);
    let (_, outcome) = execute(&server, tmp.path(), &args).await.unwrap();

    assert!(!outcome.is_complete());

    // And it is not available to compare against.
    assert!(
        snob_store::store::snapshots::latest_complete(
            open_db(tmp.path()).conn(),
            Pk::new(42),
            ListKind::Followers
        )
        .unwrap()
        .is_none(),
        "a list cut short cannot be the basis of a comparison"
    );
}

/// An interrupted walk is left for the next run to pick up.
///
/// The walk asks the store whether its partial could be continued, so it can
/// print "run it again to continue where it left off" — and it asked with the
/// function that *takes* the claim, so the process handed itself the claim it
/// had just released and then exited. The next invocation is a different
/// process: it found a claim it was not allowed to adopt and, because
/// `delete_partials` spares a live claim, could not clear it either. Every
/// interrupted walk of every list started again at page one, at the price of the
/// requests it had already spent, while the advice on screen said the opposite.
#[tokio::test]
async fn an_interrupted_walk_is_left_unclaimed_for_the_next_run() {
    let server = MockServer::start().await;
    mount_profile(&server, 500).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/followers/"))
        .and(query_param("count", "50"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"users":[{"pk":1,"username":"u1"}],"next_max_id":"next"}"#),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();

    let mut args = args();
    args.max_pages = Some(1);
    let (_, outcome) = execute(&server, tmp.path(), &args).await.unwrap();

    assert!(
        outcome.resumable,
        "it stopped with a cursor, inside the window"
    );

    let holder: Option<String> = open_db(tmp.path())
        .conn()
        .query_row(
            "SELECT claimed_by FROM snapshots WHERE complete = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        holder, None,
        "the process that stopped must not still hold the walk it stopped"
    );
}

/// Resolving a name and polling its counters are the same call to the same
/// endpoint. Asking twice doubled the cheapest part of every run against
/// somebody else's account, and quadrupled it for a crossing.
#[tokio::test]
async fn a_named_target_is_asked_about_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"user":{"id":99,"username":"ghost",
                "edge_followed_by":{"count":2},"edge_follow":{"count":1}}}}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/99/followers/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"users":[{"pk":1,"username":"u1"},{"pk":2,"username":"u2"}]}"#,
            ),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let mut named = args();
    named.target = Some("@ghost".into());
    let (found, outcome) = execute(&server, tmp.path(), &named).await.unwrap();

    assert_eq!(found.len(), 2);
    let profile_requests = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/v1/users/web_profile_info/")
        .count();
    assert_eq!(profile_requests, 1, "the profile must be asked for once");

    // And the reported cost is what was really spent: the profile plus the
    // single page.
    assert_eq!(outcome.requests, 2);
}

/// A crossing walked in one run has to still be crossable when it is read back.
///
/// This is the user-visible half of the same-moment rule, and the wiring is
/// what it checks: the two `started_at` values have to travel from the stored
/// rows into the outcomes, or the rule is right and never gets told.
///
/// The dates are back-dated to what a real account produces. `taken_at` is when
/// a walk **finished**, and walking six thousand accounts takes about twenty
/// minutes at the documented pace — so the two lists of one perfectly good
/// `snob unfollowers` run finish far more than fifteen minutes apart. Comparing
/// finishing times refused exactly that pair, every time it was read back with
/// `--cache`, with an answer that was correct sitting in the database.
#[tokio::test]
async fn a_pair_walked_in_one_run_can_be_crossed_from_the_cache_afterwards() {
    let server = MockServer::start().await;
    mount_profile(&server, 30).await;
    mount_list(&server, 30).await;
    mount_following(&server, 30).await;

    let tmp = tempfile::tempdir().unwrap();
    execute(&server, tmp.path(), &args()).await.unwrap();
    let mut following_args = args();
    following_args.cache = false;
    {
        let mut app = app(&server, open_db(tmp.path()));
        engine::list(&mut app, &following_args, ListKind::Following)
            .await
            .unwrap();
    }

    // One run of a real size: followers from 0 to +400, following from +400 to
    // +1600. Nothing happened in between, and the finishing times are 1200
    // seconds apart.
    {
        let db = open_db(tmp.path());
        db.conn()
            .execute(
                "UPDATE snapshots SET started_at = 1000, taken_at = 1400 WHERE kind = 'followers'",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "UPDATE snapshots SET started_at = 1400, taken_at = 2600 WHERE kind = 'following'",
                [],
            )
            .unwrap();
    }

    let mut cached = args();
    cached.cache = true;
    let (_, followers) = execute(&server, tmp.path(), &cached).await.unwrap();
    let following = {
        let mut app = app(&server, open_db(tmp.path()));
        engine::list(&mut app, &cached, ListKind::Following)
            .await
            .unwrap()
            .1
    };

    assert_eq!(followers.provenance, Provenance::CacheFlag);
    assert_eq!(following.provenance, Provenance::CacheFlag);
    assert!(
        (following.taken_at - followers.taken_at).abs() > 900,
        "the finishing times are far apart; that is the shape being tested"
    );
    check_same_moment(&followers, &following)
        .expect("the two walks touched, so nothing happened that only one of them saw");
}

/// An interrupted walk is continued, not begun again.
///
/// The store half and the pager half are each tested on their own — `resumable`
/// hands back a partial, and a walker given a cursor sends it — but nothing
/// tested the wiring between them, and the wiring is `walk::open_snapshot`.
/// `let pending = None;` there passes the whole suite while every interrupted
/// walk restarts at page one and pays again for the requests it had already
/// spent, with the advice on screen promising the opposite. `no_resume` is
/// `false` at all three construction sites and was never `true` in a test
/// either, so the flag that turns this off had no coverage at all.
#[tokio::test]
async fn an_interrupted_walk_continues_from_the_cursor_it_stored() {
    let server = MockServer::start().await;
    mount_profile(&server, 2).await;

    // Page two first: wiremock matches in the order mocks were mounted, and
    // the page-one mock below matches any request to this path.
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/followers/"))
        .and(query_param("max_id", "next"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"users":[{"pk":2,"username":"u2"}]}"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/followers/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"users":[{"pk":1,"username":"u1"}],"next_max_id":"next"}"#),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();

    // A walk stopped after one page, with a cursor saved.
    let mut capped = args();
    capped.max_pages = Some(1);
    execute(&server, tmp.path(), &capped).await.unwrap();

    let partial: i64 = open_db(tmp.path())
        .conn()
        .query_row("SELECT id FROM snapshots WHERE complete = 0", [], |row| {
            row.get(0)
        })
        .unwrap();

    // And the run after it, with no cap, which has to pick that one up.
    let (found, _) = execute(&server, tmp.path(), &args()).await.unwrap();

    let asked: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter_map(|r| r.url.query().map(str::to_string))
        .collect();
    assert!(
        asked.iter().any(|q| q.contains("max_id=next")),
        "the stored cursor has to be sent, or the requests already spent are spent again: \
         {asked:?}"
    );

    let (id, members, resumes): (i64, i64, i64) = open_db(tmp.path())
        .conn()
        .query_row(
            "SELECT id, member_count, resumes FROM snapshots WHERE complete = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        id, partial,
        "it continued the capture rather than opening one"
    );
    assert_eq!(resumes, 1, "and recorded that it did");
    assert_eq!(members, 2, "page one's members survived the resume");
    assert_eq!(found.len(), 2);
}

/// And `--no-resume` is what starts over, which nothing exercised either.
///
/// Measured in requests rather than in the capture's id: SQLite reuses a rowid
/// after a delete — the same property `run_id` exists because of — so the fresh
/// capture is very often numbered the same as the one it replaced.
#[tokio::test]
async fn no_resume_walks_the_list_again_from_the_first_page() {
    let server = MockServer::start().await;
    mount_profile(&server, 2).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/followers/"))
        .and(query_param("max_id", "next"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"users":[{"pk":2,"username":"u2"}]}"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/followers/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"users":[{"pk":1,"username":"u1"}],"next_max_id":"next"}"#),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();

    let mut capped = args();
    capped.max_pages = Some(1);
    execute(&server, tmp.path(), &capped).await.unwrap();
    let after_the_first = requests(&server).await;

    let mut fresh = args();
    fresh.no_resume = true;
    execute(&server, tmp.path(), &fresh).await.unwrap();

    let resumes: i64 = open_db(tmp.path())
        .conn()
        .query_row(
            "SELECT resumes FROM snapshots WHERE complete = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(resumes, 0, "--no-resume continues nothing");
    assert_eq!(
        requests(&server).await - after_the_first,
        3,
        "the counter poll and both pages again, where a resume would have asked for one"
    );
}
