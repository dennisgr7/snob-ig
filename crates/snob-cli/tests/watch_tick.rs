//! What a run of the monitor costs, and what it refuses to conclude.
//!
//! The other watch file drives the comparison over a database. This one drives
//! the whole tick against a mock Instagram, because the two questions it
//! answers can only be answered here: how many requests a run really spends,
//! and what happens when the run does not get a straight answer.

use std::sync::Arc;

use snob_core::model::ListKind;
use snob_core::paths::AppPaths;
use snob_core::session::{Session, SessionOrigin};
use snob_core::store::Store;
use snob_core::store::rate_budget::{RateBudget, SqliteRateBudget, UnlimitedRateBudget};
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use snob_cli::app::{App, Viewer};
use snob_cli::engine::Provenance;
use snob_cli::engine::watch::{self, Skipped, TickReport, Watched};

mod common;
use common::{SID, UA};

fn open_db(root: &std::path::Path) -> Store {
    Store::open_at(&root.join("test.db")).unwrap()
}

fn app_with(server: &MockServer, db: Store, budget: Arc<dyn RateBudget>) -> App {
    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::new(budget))
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

fn app(server: &MockServer, db: Store) -> App {
    app_with(server, db, Arc::new(UnlimitedRateBudget))
}

/// A profile answering with these two counters.
async fn mount_profile(server: &MockServer, followers: u64, following: u64) {
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"data":{{"user":{{"id":"42","username":"me",
                "edge_followed_by":{{"count":{followers}}},
                "edge_follow":{{"count":{following}}}}}}}}}"#
        )))
        .mount(server)
        .await;
}

async fn mount_list(server: &MockServer, kind: &str, pks: &[u64]) {
    let users: Vec<String> = pks
        .iter()
        .map(|i| format!(r#"{{"pk":{i},"username":"u{i}"}}"#))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/friendships/42/{kind}/")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"users":[{}]}}"#, users.join(","))),
        )
        .mount(server)
        .await;
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

/// A whole run: look, then record having reported it.
///
/// The command does these as two steps so the body can be built in between --
/// what a report looks like on the wire is presentation, and `engine` does not
/// decide how anything looks. A test that only called `tick` would leave the
/// marks where they were and prove nothing about the second run.
async fn run(app: &mut App, watched: &Watched) -> TickReport {
    let tick = watch::tick(app, watched).await.unwrap();
    watch::commit(app, &tick, None).unwrap();
    tick
}

/// The number the whole design rests on, and it is **one**.
///
/// One request, not one per list: `web_profile_info` answers with both counters
/// at once and `App::remember_counters` keeps them, so the second list asks
/// nothing. The design was budgeted at two on the assumption that each list
/// polls its own counter, and that assumption was already wrong in the
/// direction that costs less — the memo `engine::freshness` documents was put
/// there for exactly this, for the second half of a crossing.
///
/// Asserted rather than left as an observation: this is what makes running the
/// monitor every few hours affordable, so a change that turns it back into two
/// requests per run should have to argue for itself.
#[tokio::test]
async fn a_run_with_nothing_to_report_costs_one_request() {
    let server = MockServer::start().await;
    mount_profile(&server, 3, 2).await;
    mount_list(&server, "followers", &[1, 2, 3]).await;
    mount_list(&server, "following", &[8, 9]).await;

    let tmp = tempfile::tempdir().unwrap();

    // The first run walks both lists and lays down the baseline.
    let mut first = app(&server, open_db(tmp.path()));
    run(&mut first, &Watched::own()).await;
    drop(first);
    let after_first = requests(&server).await;

    // The second: both counters unchanged, so neither list is walked.
    let mut second = app(&server, open_db(tmp.path()));
    let tick = run(&mut second, &Watched::own()).await;

    assert_eq!(
        requests(&server).await - after_first,
        1,
        "one profile answer carries both counters, so one request settles both lists"
    );
    assert_eq!(
        tick.requests, 1,
        "and it has to report what it really spent"
    );
    assert!(tick.report.changes().is_empty());
    assert!(
        tick.looked(),
        "a counter that was checked is a list that was seen"
    );
}

/// The first run must say nothing, however many people are in the lists.
#[tokio::test]
async fn the_first_run_lays_a_baseline_and_reports_nothing() {
    let server = MockServer::start().await;
    mount_profile(&server, 3, 2).await;
    mount_list(&server, "followers", &[1, 2, 3]).await;
    mount_list(&server, "following", &[8, 9]).await;

    let tmp = tempfile::tempdir().unwrap();
    let mut app = app(&server, open_db(tmp.path()));
    let tick = run(&mut app, &Watched::own()).await;

    assert!(tick.report.changes().is_empty());
    assert_eq!(tick.report.followers.as_ref().unwrap().total, 3);
}

/// A counter that moved is what makes the monitor go and look, and the diff is
/// against what was last reported.
#[tokio::test]
async fn a_counter_that_moved_is_walked_and_the_arrival_is_reported() {
    let tmp = tempfile::tempdir().unwrap();

    {
        let server = MockServer::start().await;
        mount_profile(&server, 2, 2).await;
        mount_list(&server, "followers", &[1, 2]).await;
        mount_list(&server, "following", &[8, 9]).await;

        let mut app = app(&server, open_db(tmp.path()));
        run(&mut app, &Watched::own()).await;
    }

    // A fresh server, because the counter and the list both have to change.
    let server = MockServer::start().await;
    mount_profile(&server, 3, 2).await;
    mount_list(&server, "followers", &[1, 2, 3]).await;
    mount_list(&server, "following", &[8, 9]).await;

    let mut app = app(&server, open_db(tmp.path()));
    let tick = run(&mut app, &Watched::own()).await;
    let changes = tick.report.changes();

    assert_eq!(changes.followers.gained.len(), 1);
    assert_eq!(changes.followers.gained[0].username, "u3");
    assert!(
        changes.following.is_empty(),
        "the following counter did not move, so that list was not walked"
    );
}

/// A cooldown is the case this whole predicate exists for. Nothing may be
/// spent, so storage answers — and a stored list nothing verified must not
/// become the basis of a comparison, nor move the monitor on.
#[tokio::test]
async fn a_cooldown_stops_the_run_concluding_anything_and_leaves_the_mark() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let _schema = Store::open(&paths).unwrap();
    let budget = Arc::new(SqliteRateBudget::open(&paths).unwrap());

    let server = MockServer::start().await;
    mount_profile(&server, 2, 2).await;
    mount_list(&server, "followers", &[1, 2]).await;
    mount_list(&server, "following", &[8, 9]).await;

    // A first run, so there is something stored to serve.
    let mut app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());
    run(&mut app, &Watched::own()).await;
    drop(app);

    budget
        .start_cooldown("rate_limit", std::time::Duration::from_secs(3600))
        .unwrap();
    let before = requests(&server).await;

    let mut app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());
    let tick = run(&mut app, &Watched::own()).await;

    assert_eq!(
        requests(&server).await,
        before,
        "a cooldown means nothing is spent, not even the counter poll"
    );
    assert!(
        !tick.looked(),
        "nothing established that these lists are current"
    );
    for list in &tick.lists {
        assert!(matches!(
            list.skipped,
            Some(Skipped::NobodyLooked(Provenance::Cooldown))
        ));
    }
    assert!(tick.report.changes().is_empty());
}

/// And having refused, it has to stay refused: the mark must not move, or the
/// change that arrived during the cooldown is never reported by anybody.
#[tokio::test]
async fn a_refused_run_does_not_move_the_monitor_on() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let _schema = Store::open(&paths).unwrap();
    let budget = Arc::new(SqliteRateBudget::open(&paths).unwrap());

    {
        let server = MockServer::start().await;
        mount_profile(&server, 2, 2).await;
        mount_list(&server, "followers", &[1, 2]).await;
        mount_list(&server, "following", &[8, 9]).await;
        let mut app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());
        run(&mut app, &Watched::own()).await;
    }

    // Somebody arrives, and a run happens during a cooldown.
    {
        let server = MockServer::start().await;
        mount_profile(&server, 3, 2).await;
        mount_list(&server, "followers", &[1, 2, 3]).await;
        mount_list(&server, "following", &[8, 9]).await;

        let mut app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());
        run(&mut app, &Watched::own()).await;

        budget
            .start_cooldown("rate_limit", std::time::Duration::from_secs(3600))
            .unwrap();
        let mut app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());
        let refused = run(&mut app, &Watched::own()).await;
        assert!(!refused.looked());
    }

    // The arrival was reported by the run that could see, and the refused run
    // in between neither repeated it nor swallowed it.
    let db = Store::open(&paths).unwrap();
    let mark = snob_core::store::watch::mark(db.conn(), 42, ListKind::Followers)
        .unwrap()
        .expect("the run that could see left a receipt");
    assert!(mark.snapshot_id.is_some());
}

/// The distinction `watch_runs` exists for, and the reason `status` reads it.
///
/// A run that could not look moves no mark, so from the marks alone a monitor
/// sitting in a cooldown since Monday is indistinguishable from one that was
/// killed on Monday. The run has to be recorded either way, and it has to say
/// which of the two it was.
#[tokio::test]
async fn a_run_that_could_not_look_is_still_recorded_as_having_run() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let _schema = Store::open(&paths).unwrap();
    let budget = Arc::new(SqliteRateBudget::open(&paths).unwrap());

    let server = MockServer::start().await;
    mount_profile(&server, 2, 2).await;
    mount_list(&server, "followers", &[1, 2]).await;
    mount_list(&server, "following", &[8, 9]).await;

    {
        let mut app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());
        run(&mut app, &Watched::own()).await;
    }

    budget
        .start_cooldown("rate_limit", std::time::Duration::from_secs(3600))
        .unwrap();

    let mut app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());
    let blocked = run(&mut app, &Watched::own()).await;
    assert!(!blocked.looked());
    drop(app);

    let db = Store::open(&paths).unwrap();
    let last = snob_core::store::watch::last_run(db.conn(), 42)
        .unwrap()
        .expect("a run that concluded nothing still ran");

    assert_eq!(
        last.outcome.as_deref(),
        Some("rate_limited"),
        "it has to say why it could not look, not merely that it was quiet"
    );
    assert_eq!(last.changes, 0);
}

/// Two watched accounts are two accounts.
///
/// `run_one` shares one `App` across every configured `[[account]]`, and the
/// resolution memo on it used to be keyed on nothing at all. So the second
/// account silently reused the first one's target: never resolved, never
/// walked, and its report committed under the first account's marks. Every
/// change on every account after the first was lost, while `status` showed a
/// healthy mark and the webhook got the first account's report twice.
///
/// The assertion is the one that would have caught it: two distinct account ids.
#[tokio::test]
async fn a_second_watched_account_is_walked_as_itself() {
    let server = MockServer::start().await;

    // Two profiles, answering on the name each is asked about.
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .and(wiremock::matchers::query_param("username", "other"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"user":{"id":"99","username":"other",
                "edge_followed_by":{"count":1},"edge_follow":{"count":1}}}}"#,
        ))
        .mount(&server)
        .await;
    mount_profile(&server, 2, 2).await;

    mount_list(&server, "followers", &[1, 2]).await;
    mount_list(&server, "following", &[8, 9]).await;
    for kind in ["followers", "following"] {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/friendships/99/{kind}/")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"users":[{"pk":7,"username":"u7"}]}"#),
            )
            .mount(&server)
            .await;
    }

    let tmp = tempfile::tempdir().unwrap();
    let mut app = app(&server, open_db(tmp.path()));

    let mut seen = Vec::new();
    for watched in [
        Watched::own(),
        Watched::consented("other".into(), watch::Consent { given_at: 1 }),
    ] {
        seen.push(run(&mut app, &watched).await.report.account_pk);
    }

    assert_eq!(
        seen,
        vec![42, 99],
        "the second account was handed the first one's target"
    );

    let asked: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.to_string())
        .collect();
    assert!(
        asked.iter().any(|u| u.contains("/friendships/99/")),
        "the second account's lists were never requested: {asked:?}"
    );
}

/// Your own account needs nobody's permission; somebody else's does, and a
/// scheduled run has nobody to ask.
#[tokio::test]
async fn only_a_recorded_answer_lets_an_unattended_run_read_a_stranger() {
    assert!(Watched::own().may_run_unattended());
    assert!(!Watched::asking("someone".into()).may_run_unattended());
    assert!(
        Watched::consented("someone".into(), watch::Consent { given_at: 1_700 })
            .may_run_unattended()
    );
}

/// A report too old to be news settles even when no tick got as far as
/// reporting.
///
/// The expiry ran inside the step that commits a comparison, which is reached
/// only through the delivery step — so a run that ended earlier, because the
/// session had gone or a watched stranger went private, settled nothing. The row
/// stayed `pending` for ever: `deliveries::due` will not hand back an over-age
/// report, and the only thing that expires one is a failed attempt, which could
/// therefore never happen. `snob watch status` went on counting it as owed and
/// promising the next run would try it.
#[tokio::test]
async fn a_report_too_old_to_be_news_settles_without_a_comparison() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let app = app(&server, open_db(tmp.path()));

    let now = snob_core::store::now();
    let long_ago = now - snob_core::store::deliveries::MAX_AGE_SECS - 1;
    snob_core::store::users::upsert(
        app.db().conn(),
        &snob_core::model::User {
            pk: 42,
            username: "me".into(),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        },
    )
    .unwrap();
    snob_core::store::accounts::upsert(app.db().conn(), 42, true).unwrap();
    let id =
        snob_core::store::deliveries::enqueue(app.db().conn(), "run-1", 42, "{}", long_ago, None)
            .unwrap();

    assert!(
        snob_core::store::deliveries::due(app.db().conn(), now, 10, "https://receiver.example")
            .unwrap()
            .is_empty(),
        "an over-age report is not due, so nothing can expire it by failing"
    );

    watch::settle(&app, now);

    assert_eq!(
        snob_core::store::deliveries::state(app.db().conn(), id).unwrap(),
        Some("expired".to_string())
    );
    assert_eq!(
        snob_core::store::deliveries::pending(app.db().conn()).unwrap(),
        0,
        "status must not go on saying a report is owed"
    );
}
