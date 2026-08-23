//! What the list commands do while the account is in cooldown.
//!
//! The contract: nothing is spent, not even the counter poll. A stored
//! complete list is served with a warning; with nothing stored the command
//! refuses with the throttling exit code and names when the cooldown ends.

use std::time::Duration;

use std::sync::Arc;

use snob_core::Pk;
use snob_core::budget::{RateBudget, RateBudgetError, UnlimitedRateBudget};
use snob_core::model::ListKind;
use snob_core::session::{Session, SessionOrigin};
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use snob_store::paths::AppPaths;
use snob_store::store::Store;
use snob_store::store::rate_budget::SqliteRateBudget;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use snob_cli::app::{App, Viewer};
use snob_cli::cli::ListArgs;
use snob_cli::engine::{self, ListOutcome, Provenance, ResultSource};
use snob_cli::exit::ExitCode;

mod common;
use common::{SID, UA, args};

/// A budget over a database that exists.
///
/// Opening the store first is not optional: it is what creates the schema, and
/// the budget opens its own connection to the same file expecting tables.
fn budget_at(root: &std::path::Path) -> Arc<SqliteRateBudget> {
    let paths = AppPaths::rooted_at(root);
    let _schema = Store::open(&paths).unwrap();
    Arc::new(SqliteRateBudget::open(&paths).unwrap())
}

/// Reopens the database so each invocation is a separate run, the way two
/// consecutive commands really are.
fn reopen(root: &std::path::Path) -> Store {
    Store::open(&AppPaths::rooted_at(root)).unwrap()
}

async fn mount_profile(server: &MockServer, user: &str) {
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!(r#"{{"data":{{"user":{user}}}}}"#)),
        )
        .mount(server)
        .await;
}

async fn mount_list(server: &MockServer, pk: Pk, how_many: u64) {
    let users: Vec<String> = (0..how_many)
        .map(|i| format!(r#"{{"pk":{i},"username":"u{i}"}}"#))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/friendships/{pk}/followers/")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"users":[{}]}}"#, users.join(","))),
        )
        .mount(server)
        .await;
}

async fn execute_with(
    server: &MockServer,
    db: Store,
    budget: Arc<dyn RateBudget>,
    args: &ListArgs,
) -> anyhow::Result<(Vec<snob_core::model::User>, ListOutcome)> {
    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::new(budget))
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());

    let mut app = App::for_test(
        client,
        db,
        Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        },
    );
    engine::list(&mut app, args, ListKind::Followers).await
}

/// How many requests the server has received so far.
async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

/// Reports no cooldown for a fixed number of calls, then an active one: the
/// shape of a cooldown another process sets while a run is underway.
struct LateCooldown {
    calls_before: u32,
    seen: std::sync::atomic::AtomicU32,
}

impl LateCooldown {
    fn after(calls_before: u32) -> Self {
        Self {
            calls_before,
            seen: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

impl RateBudget for LateCooldown {
    fn reserve(&self) -> Result<std::time::Duration, RateBudgetError> {
        Ok(std::time::Duration::ZERO)
    }

    fn reserve_write(&self) -> Result<std::time::Duration, RateBudgetError> {
        self.reserve()
    }

    fn cooldown(&self) -> Result<Option<snob_core::EpochMs>, RateBudgetError> {
        let seen = self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok((seen >= self.calls_before)
            .then(|| snob_core::clock::now_ms() + std::time::Duration::from_millis(3_600_000)))
    }

    fn start_cooldown(
        &self,
        _: &str,
        _: std::time::Duration,
    ) -> Result<snob_core::EpochMs, RateBudgetError> {
        Ok(snob_core::EpochMs::new(0))
    }
}

fn assert_rate_limited(error: &anyhow::Error) {
    assert_eq!(ExitCode::from_chain(error), Some(ExitCode::RateLimited));
}

fn start_cooldown(budget: &Arc<SqliteRateBudget>) {
    budget
        .start_cooldown("rate_limit", Duration::from_secs(2 * 3600))
        .unwrap();
}

#[tokio::test]
async fn during_a_cooldown_the_stored_list_is_served_without_requests() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":"42","username":"me","edge_followed_by":{"count":30},"edge_follow":{"count":10}}"#,
    )
    .await;
    mount_list(&server, Pk::new(42), 30).await;

    let tmp = tempfile::tempdir().unwrap();
    let budget = budget_at(tmp.path());

    execute_with(
        &server,
        reopen(tmp.path()),
        Arc::new(UnlimitedRateBudget),
        &args(),
    )
    .await
    .unwrap();
    let seeded = requests(&server).await;

    start_cooldown(&budget);
    let (found, outcome) = execute_with(&server, reopen(tmp.path()), budget.clone(), &args())
        .await
        .unwrap();

    assert_eq!(found.len(), 30);
    assert_eq!(outcome.source(), ResultSource::Cached);
    assert_eq!(outcome.requests, 0);
    assert_eq!(outcome.provenance, Provenance::Cooldown);
    assert_eq!(
        requests(&server).await,
        seeded,
        "a cooldown must not spend a single request, not even the poll"
    );
}

/// The documented contract: served however old, ignoring --max-age. This is
/// what tells the cooldown path apart from the normal cache policy.
#[tokio::test]
async fn an_old_snapshot_is_still_served_during_the_cooldown() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":"42","username":"me","edge_followed_by":{"count":30},"edge_follow":{"count":10}}"#,
    )
    .await;
    mount_list(&server, Pk::new(42), 30).await;

    let tmp = tempfile::tempdir().unwrap();
    let budget = budget_at(tmp.path());

    execute_with(
        &server,
        reopen(tmp.path()),
        Arc::new(UnlimitedRateBudget),
        &args(),
    )
    .await
    .unwrap();

    // Age the snapshot far past the default --max-age of six hours.
    reopen(tmp.path())
        .conn()
        .execute("UPDATE snapshots SET taken_at = taken_at - 1000000", [])
        .unwrap();

    start_cooldown(&budget);
    let (found, outcome) = execute_with(&server, reopen(tmp.path()), budget.clone(), &args())
        .await
        .unwrap();

    assert_eq!(found.len(), 30);
    assert_eq!(outcome.source(), ResultSource::Cached);
    assert_eq!(outcome.requests, 0);
}

#[tokio::test]
async fn with_nothing_stored_a_cooldown_refuses_with_the_rate_limited_code() {
    let server = MockServer::start().await;

    let tmp = tempfile::tempdir().unwrap();
    let budget = budget_at(tmp.path());
    start_cooldown(&budget);

    let error = execute_with(&server, reopen(tmp.path()), budget.clone(), &args())
        .await
        .unwrap_err();

    assert!(error.to_string().contains("in cooldown until"), "{error}");
    assert_rate_limited(&error);
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn refresh_is_refused_while_the_cooldown_lasts() {
    let server = MockServer::start().await;

    let tmp = tempfile::tempdir().unwrap();
    let budget = budget_at(tmp.path());
    start_cooldown(&budget);

    let mut args = args();
    args.walk.refresh = true;
    let error = execute_with(&server, reopen(tmp.path()), budget.clone(), &args)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("--refresh"), "{error}");
    assert_rate_limited(&error);
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn a_named_target_is_resolved_locally_and_case_insensitively() {
    let seed = MockServer::start().await;
    mount_profile(
        &seed,
        r#"{"id":99,"username":"ghost","edge_followed_by":{"count":5},"edge_follow":{"count":1}}"#,
    )
    .await;
    mount_list(&seed, Pk::new(99), 5).await;

    let tmp = tempfile::tempdir().unwrap();
    let budget = budget_at(tmp.path());

    let mut named = args();
    named.target = Some("@ghost".into());
    execute_with(
        &seed,
        reopen(tmp.path()),
        Arc::new(UnlimitedRateBudget),
        &named,
    )
    .await
    .unwrap();

    start_cooldown(&budget);

    // A server with nothing mounted: any request would fail loudly.
    let empty = MockServer::start().await;
    let mut cased = args();
    cased.target = Some("@Ghost".into());
    let (found, outcome) = execute_with(&empty, reopen(tmp.path()), budget.clone(), &cased)
        .await
        .unwrap();

    assert_eq!(found.len(), 5);
    assert_eq!(outcome.source(), ResultSource::Cached);
    assert_eq!(requests(&empty).await, 0);
}

#[tokio::test]
async fn a_named_target_never_walked_is_refused() {
    let server = MockServer::start().await;

    let tmp = tempfile::tempdir().unwrap();
    let budget = budget_at(tmp.path());
    start_cooldown(&budget);

    let mut named = args();
    named.target = Some("@ghost".into());
    let error = execute_with(&server, reopen(tmp.path()), budget.clone(), &named)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("@ghost"), "{error}");
    assert_rate_limited(&error);
    assert_eq!(requests(&server).await, 0);
}

/// A cooldown that lands after the entry check — set by another process
/// sharing the database — must still stop the poll from firing.
#[tokio::test]
async fn a_cooldown_landing_after_the_entry_check_still_stops_the_poll() {
    let server = MockServer::start().await;

    let tmp = tempfile::tempdir().unwrap();
    let _schema = Store::open(&AppPaths::rooted_at(tmp.path())).unwrap();

    // Visible at the second look (before the poll), not at the entry check.
    let budget: Arc<dyn RateBudget> = Arc::new(LateCooldown::after(1));
    let error = execute_with(&server, reopen(tmp.path()), budget.clone(), &args())
        .await
        .unwrap_err();

    assert_rate_limited(&error);
    assert_eq!(requests(&server).await, 0, "the poll must never fire");
}

/// A cooldown that only becomes visible at the walker's own check still has
/// to come out as throttling, not as a generic failure.
#[tokio::test]
async fn a_cooldown_landing_after_the_poll_still_exits_throttled() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":"42","username":"me","edge_followed_by":{"count":30},"edge_follow":{"count":10}}"#,
    )
    .await;

    let tmp = tempfile::tempdir().unwrap();
    let _schema = Store::open(&AppPaths::rooted_at(tmp.path())).unwrap();

    // Visible only at the fourth look: entry check, pre-poll check, **the
    // pacer's own**, walker.
    //
    // The third of those is the backstop in `Pacer::clear`, which reads the
    // cooldown before every request rather than trusting the caller to have
    // asked. It moved this ordinal by one and nothing else: with `after(2)` the
    // pacer saw the cooldown first and the poll never went out, which is a
    // better outcome and a different test. This one is about the walk stopping
    // after a request that had already left, so the fixture has to let that
    // request leave.
    let budget: Arc<dyn RateBudget> = Arc::new(LateCooldown::after(3));
    let error = execute_with(&server, reopen(tmp.path()), budget.clone(), &args())
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("nothing can be walked"),
        "{error}"
    );
    assert_rate_limited(&error);
    assert_eq!(requests(&server).await, 1, "only the poll was spent");
}

/// Before the pre-check, `--cache` with a named target still spent one
/// profile request on resolving it. During a cooldown it no longer does.
#[tokio::test]
async fn cache_during_a_cooldown_skips_the_resolve_request() {
    let seed = MockServer::start().await;
    mount_profile(
        &seed,
        r#"{"id":99,"username":"ghost","edge_followed_by":{"count":5},"edge_follow":{"count":1}}"#,
    )
    .await;
    mount_list(&seed, Pk::new(99), 5).await;

    let tmp = tempfile::tempdir().unwrap();
    let budget = budget_at(tmp.path());

    let mut named = args();
    named.target = Some("@ghost".into());
    execute_with(
        &seed,
        reopen(tmp.path()),
        Arc::new(UnlimitedRateBudget),
        &named,
    )
    .await
    .unwrap();

    start_cooldown(&budget);

    let empty = MockServer::start().await;
    let mut cached = args();
    cached.target = Some("@ghost".into());
    cached.walk.offline = true;
    let (found, outcome) = execute_with(&empty, reopen(tmp.path()), budget.clone(), &cached)
        .await
        .unwrap();

    assert_eq!(found.len(), 5);
    assert_eq!(outcome.requests, 0);
    assert_eq!(requests(&empty).await, 0);
}

/// A cooldown that lands while the confirmation prompt is open costs nothing.
///
/// The second check sat past `target::resolve`, and resolving is a request —
/// so a named target spent exactly the counter poll `engine::cooldown` says
/// must never be spent: "nothing may be spent, not even the counter poll". The
/// regression test beside this one missed it because its fixture leaves
/// `target` as `None`, which is the one shape that resolves without asking
/// Instagram anything.
///
/// `after(1)`: the entry check sees nothing, and the cooldown is there by the
/// time the second one looks. That is the window the second check exists for.
#[tokio::test]
async fn a_cooldown_landing_before_the_resolve_spends_nothing() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":"7","username":"someone","edge_followed_by":{"count":30},"edge_follow":{"count":10}}"#,
    )
    .await;

    let tmp = tempfile::tempdir().unwrap();
    let _schema = Store::open(&AppPaths::rooted_at(tmp.path())).unwrap();

    let named = ListArgs {
        target: Some("someone".to_string()),
        ..args()
    };
    let budget: Arc<dyn RateBudget> = Arc::new(LateCooldown::after(1));
    let error = execute_with(&server, reopen(tmp.path()), budget, &named)
        .await
        .unwrap_err();

    assert_rate_limited(&error);
    assert_eq!(
        requests(&server).await,
        0,
        "resolving the name is a request, and a cooldown is a cooldown"
    );
}
