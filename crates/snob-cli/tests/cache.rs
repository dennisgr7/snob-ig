//! End-to-end test of the cache policy.
//!
//! It is what decides how many requests each invocation costs, so it is checked
//! by counting what reaches the mock server rather than by looking at output.

use snob_core::model::ListKind;
use snob_core::session::{Session, SessionOrigin};
use snob_core::store::Store;
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use snob_cli::app::{App, Viewer};
use snob_cli::cli::ListArgs;
use snob_cli::engine::{self, ListOutcome, ResultSource};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
const SID: &str = "42%3AAbCdEfGh%3A20";

fn args() -> ListArgs {
    ListArgs {
        target: None,
        hide: vec![],
        only: vec![],
        no_verified: false,
        exclude_list: None,
        format: None,
        output: None,
        limit: None,
        refresh: false,
        cache: false,
        max_age: std::time::Duration::from_secs(6 * 3600),
        no_resume: false,
        max_pages: None,
        no_progress: true,
        yes: true,
    }
}

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
            pk: 42,
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
    assert_eq!(outcome.source, ResultSource::Fetched);
    let after_first = requests(&server).await;
    assert!(after_first >= 1);

    // Second, with the counter unchanged: a single request, the poll.
    let (found, outcome) = execute(&server, tmp.path(), &args).await.unwrap();
    assert_eq!(found.len(), 30);
    assert_eq!(outcome.source, ResultSource::Cached);
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
    assert_eq!(outcome.source, ResultSource::Cached);
    assert_eq!(outcome.requests, 0);
    assert_eq!(
        requests(&server).await,
        before,
        "--cache must not ask for anything"
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

    assert_eq!(outcome.source, ResultSource::Fetched);
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
        outcome.source,
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
    assert_eq!(outcome.source, ResultSource::Cached);
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

    assert!(error.to_string().contains("no snapshot"), "{error}");
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
        snob_core::store::snapshots::latest_complete(
            open_db(tmp.path()).conn(),
            42,
            ListKind::Followers
        )
        .unwrap()
        .is_none(),
        "a list cut short cannot be the basis of a comparison"
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
