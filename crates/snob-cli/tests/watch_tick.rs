//! What a run of the monitor costs, and what it refuses to conclude.
//!
//! The other watch file drives the comparison over a database. This one drives
//! the whole tick against a mock Instagram, because the two questions it
//! answers can only be answered here: how many requests a run really spends,
//! and what happens when the run does not get a straight answer.

use std::sync::Arc;

use snob_core::budget::{RateBudget, UnlimitedRateBudget};
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

/// A capture nobody reported on is not a baseline.
///
/// A run compares against `watch_marks`, not against the newest capture, and
/// this check asked the captures -- so any two complete ones made it `ok`. One
/// `snob followers` and one `snob following`, which are the two commands the
/// README leads with, leave exactly that state. The wizard then printed
/// `ok  baseline`, suppressed the note that explains why a first scheduled run
/// reports nothing, and never made the offer to take the capture now, because
/// the offer gates on the same count. The one state the line exists to warn
/// about was the state it was silent for, and it self-heals only after a whole
/// interval has gone by with a user thinking the monitor is broken.
#[tokio::test]
async fn a_capture_nobody_reported_is_not_a_baseline() {
    use snob_cli::engine::check::{Verdict, baseline_of};

    let server = MockServer::start().await;
    mount_profile(&server, 3, 2).await;
    mount_list(&server, "followers", &[1, 2, 3]).await;
    mount_list(&server, "following", &[8, 9]).await;

    let tmp = tempfile::tempdir().unwrap();
    let mut app = app(&server, open_db(tmp.path()));

    // Both lists walked and stored, and nothing reported on them. `tick` without
    // `commit` is that state exactly: the marks are `commit`'s to move, which is
    // what makes this what a hand-run of the list commands leaves behind.
    let tick = watch::tick(&mut app, &Watched::own()).await.unwrap();

    let before = baseline_of(&app, 42);
    assert_eq!(
        before.verdict,
        Verdict::Warned,
        "two captures nobody has reported on are not something to compare against"
    );
    assert!(
        before.problem.is_some(),
        "and the note saying the first run reports nothing is the whole point of saying so"
    );

    // One run of the monitor, and now there is one.
    watch::commit(&mut app, &tick, None).unwrap();

    let after = baseline_of(&app, 42);
    assert_eq!(after.verdict, Verdict::Ok, "{:?}", after.problem);
    assert!(after.problem.is_none());
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
    let mark = snob_store::store::watch::mark(db.conn(), 42, ListKind::Followers)
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
    let runs = snob_store::store::watch::last_runs(db.conn()).unwrap();
    let last = runs
        .iter()
        .find(|run| run.account_pk == 42)
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
        Watched::consented("other".into(), watch::Consent),
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

/// A cooldown on an account nothing is stored about yet refuses both lists
/// before either has said whose they are -- and the report that came out of
/// that was the **viewer's**: `account.pk` 42 on the wire, and a
/// `rate_limited` row written against the viewer in `watch_runs`, displacing
/// that account's own newest row in `status`. A tick that learned nothing
/// about a named account has nothing to report under that name, and says so.
#[tokio::test]
async fn a_tick_that_could_not_look_at_a_stranger_is_not_reported_as_the_viewer() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let _schema = Store::open(&paths).unwrap();
    let budget = Arc::new(SqliteRateBudget::open(&paths).unwrap());
    let server = MockServer::start().await;

    budget
        .start_cooldown("rate_limit", std::time::Duration::from_secs(3600))
        .unwrap();
    let mut app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());

    let outcome = watch::tick(
        &mut app,
        &Watched::consented("stranger".into(), watch::Consent),
    )
    .await;

    let error = match outcome {
        Ok(tick) => panic!(
            "a report was made under account {}, which is nobody this run looked at",
            tick.report.account_pk
        ),
        Err(e) => e,
    };
    assert!(
        error.to_string().contains("stranger"),
        "the refusal names the account: {error:#}"
    );
    assert_eq!(requests(&server).await, 0, "a cooldown spends nothing");
}

/// Your own account needs nobody's permission; somebody else's does, and a
/// scheduled run has nobody to ask.
#[tokio::test]
async fn only_a_recorded_answer_lets_an_unattended_run_read_a_stranger() {
    assert!(Watched::own().may_run_unattended());
    assert!(!Watched::asking("someone".into()).may_run_unattended());
    assert!(Watched::consented("someone".into(), watch::Consent).may_run_unattended());
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

    let now = snob_core::clock::now();
    let long_ago = now - snob_store::store::deliveries::MAX_AGE_SECS - 1;
    snob_store::store::users::upsert(
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
    snob_store::store::accounts::upsert(app.db().conn(), 42, true).unwrap();
    let id =
        snob_store::store::deliveries::enqueue(app.db().conn(), "run-1", 42, "{}", long_ago, None)
            .unwrap();

    assert!(
        snob_store::store::deliveries::due(app.db().conn(), now, 10, "https://receiver.example")
            .unwrap()
            .is_empty(),
        "an over-age report is not due, so nothing can expire it by failing"
    );

    watch::settle(app.db(), now);

    assert_eq!(
        snob_store::store::deliveries::state(app.db().conn(), id).unwrap(),
        Some("expired".to_string())
    );
    assert_eq!(
        snob_store::store::deliveries::pending(app.db().conn()).unwrap(),
        0,
        "status must not go on saying a report is owed"
    );
}

/// A run that is interrupted does not go on to the next list.
///
/// A canceled walk comes back `Ok`, so the loop over the two lists went
/// straight on — and `App::resolved_target` drops the remembered counters as
/// soon as the pacer has moved, so the second list polled Instagram again. The
/// user had already been told "Stopping and saving what has been fetched…".
///
/// The cancellation is fired by the followers response itself, so it lands
/// exactly between the two lists with nothing to time.
#[tokio::test]
async fn a_canceled_run_does_not_walk_the_second_list() {
    struct CancelWhenAsked {
        token: snob_ig::pace::CancelToken,
        body: String,
    }

    impl wiremock::Respond for CancelWhenAsked {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            self.token.cancel();
            ResponseTemplate::new(200).set_body_string(self.body.clone())
        }
    }

    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut app = app(&server, open_db(tmp.path()));
    let token = app.cancel().clone();

    mount_profile(&server, 3, 2).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/followers/"))
        .respond_with(CancelWhenAsked {
            token,
            body: r#"{"users":[{"pk":1,"username":"u1"}]}"#.to_string(),
        })
        .mount(&server)
        .await;
    // Mounted so that a request for it would succeed: the assertion below has
    // to fail loudly if the guard ever goes away, rather than pass because the
    // mock was missing.
    mount_list(&server, "following", &[8, 9]).await;

    let tick = watch::tick(&mut app, &Watched::own()).await.unwrap();

    let following = tick
        .lists
        .iter()
        .find(|l| l.kind == ListKind::Following)
        .expect("both lists are still reported on");
    assert!(
        matches!(
            following.skipped,
            Some(Skipped::Incomplete(
                snob_core::model::StopReason::Canceled,
                _
            ))
        ),
        "a list nobody looked at must be refused, not concluded from: {:?}",
        following.skipped
    );

    // The count is the assertion, not the absence of a `/following/` page: the
    // walker's own loop already refuses to page after cancellation, so the
    // request that used to be spent anyway was the **counter poll**.
    // `App::resolved_target` drops the remembered counters as soon as the pacer
    // has moved, so the second list asked `web_profile_info` all over again —
    // one more request into an account the user had stopped asking about.
    let asked: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert_eq!(
        asked,
        vec![
            "/api/v1/users/web_profile_info/",
            "/api/v1/friendships/42/followers/",
        ],
        "nothing may be spent after the user asked it to stop"
    );
}

/// A list served with names rather than only ids, so a rename can be arranged.
async fn mount_named(server: &MockServer, kind: &str, users: &[(u64, &str)]) {
    let users: Vec<String> = users
        .iter()
        .map(|(pk, name)| format!(r#"{{"pk":{pk},"username":"{name}"}}"#))
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

/// The rename cursor does not step over a list this run could not read.
///
/// `renames_since` joins the members of one capture, so it only ever sees the
/// lists that were verified — but the cursor moved as soon as *any* of them was,
/// and there is one cursor for the account. On an account whose `following` is
/// refused, a rename of somebody only in `following` was stepped over
/// permanently: no later run and no `snob watch diff` would ever surface it,
/// because a rename moves nobody in or out of a list. `006_rename_cursor.sql`
/// calls that shape a defect in as many words.
///
/// Driven through `tick`, because that is the only entry point where a list is
/// refused at all — `look` reads every capture there is, so the test beside it
/// in `tests/watch.rs` cannot reach this.
///
/// A server per run, like the counter test above: a run that walks both lists
/// polls the profile twice, because `App::resolved_target` drops the remembered
/// counters as soon as the pacer has moved, so one server answering in sequence
/// cannot be lined up with the runs.
#[tokio::test]
async fn a_refused_list_holds_the_rename_cursor_where_it_is() {
    let tmp = tempfile::tempdir().unwrap();

    // Run one: both lists laid down as baselines. Nobody is in both, so a
    // rename in one is invisible to the other.
    {
        let server = MockServer::start().await;
        mount_profile(&server, 1, 1).await;
        mount_named(&server, "followers", &[(1, "one")]).await;
        mount_named(&server, "following", &[(2, "two")]).await;

        let mut app = app(&server, open_db(tmp.path()));
        run(&mut app, &Watched::own()).await;

        // A rename filed between runs, of somebody who is only in `following`.
        // Filing it through the store is what an ordinary `snob followers` by
        // hand does.
        snob_store::store::users::upsert(
            app.db().conn(),
            &snob_core::model::User {
                pk: 2,
                username: "two_renamed".into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            },
        )
        .unwrap();
    }

    // Run two: both counters have moved so both lists are walked, followers
    // answers and following does not.
    {
        let server = MockServer::start().await;
        mount_profile(&server, 2, 2).await;
        mount_named(&server, "followers", &[(1, "one"), (3, "three")]).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/following/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("not the JSON this endpoint returns"),
            )
            .mount(&server)
            .await;

        let mut app = app(&server, open_db(tmp.path()));
        let tick = run(&mut app, &Watched::own()).await;

        assert!(
            tick.lists
                .iter()
                .any(|l| l.kind == ListKind::Following && l.skipped.is_some()),
            "the following list has to be the refused one: {:?}",
            tick.lists
        );
        assert!(
            tick.report.changes().renamed.is_empty(),
            "the renamed account is not in the list this run could read"
        );
    }

    // Run three: the counters match what is stored, so both lists are verified
    // without a walk — and the rename the refused list holds is finally
    // reportable, which it can only be if the cursor stayed where it was.
    {
        let server = MockServer::start().await;
        mount_profile(&server, 2, 1).await;

        let mut app = app(&server, open_db(tmp.path()));
        let tick = run(&mut app, &Watched::own()).await;

        let renamed = tick.report.changes().renamed;
        assert_eq!(
            renamed.len(),
            1,
            "a rename in a list that was refused is owed, not lost: {renamed:?}"
        );
        assert_eq!(renamed[0].pk, 2);
        assert_eq!(renamed[0].from, "two");
        assert_eq!(renamed[0].to, "two_renamed");
    }
}

/// A cooldown on the second list does not throw the first one's news away.
///
/// The cancel branch records a refusal and carries on; an `Err` did not, so a
/// completed followers walk whose diff names a departure was discarded with the
/// second list's failure, and no `watch_runs` row was written either. Nothing
/// was lost permanently — the capture is stored and the mark did not move — but
/// on `--every 24h` that is a day late with "somebody left".
///
/// The classification is the point: only a cooldown is scoped to what could be
/// read now. A consent refusal, a session that has gone or a challenge must
/// still stop the run rather than become "one list was skipped".
#[tokio::test]
async fn a_cooldown_on_the_second_list_keeps_the_first_ones_news() {
    use snob_core::budget::RateBudgetError;

    /// Free until the run has spent a couple of requests, then standing — the
    /// shape of another process writing a cooldown while this one is between
    /// its two lists. Counted on `reserve`, which is charged once per request
    /// that really goes out.
    struct CooldownAfterTheFirstList(std::sync::atomic::AtomicUsize);
    impl RateBudget for CooldownAfterTheFirstList {
        fn reserve(&self) -> Result<std::time::Duration, RateBudgetError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(std::time::Duration::ZERO)
        }
        fn reserve_write(&self) -> Result<std::time::Duration, RateBudgetError> {
            self.reserve()
        }
        fn cooldown(&self) -> Result<Option<i64>, RateBudgetError> {
            let spent = self.0.load(std::sync::atomic::Ordering::Relaxed);
            Ok((spent >= 2).then(|| snob_core::clock::now_ms() + 7_200_000))
        }
        fn start_cooldown(&self, _: &str, _: std::time::Duration) -> Result<i64, RateBudgetError> {
            Ok(0)
        }
    }

    let tmp = tempfile::tempdir().unwrap();

    // Run one: a baseline for followers only. Following never gets a capture,
    // so when the cooldown lands there is nothing stored to serve from it —
    // which is what makes `engine::list` answer `Err` rather than a refusal.
    {
        let server = MockServer::start().await;
        mount_profile(&server, 2, 1).await;
        mount_list(&server, "followers", &[1, 2]).await;
        // No route for `following` on purpose: its walk fails, so it ends this
        // run with no capture at all. That is what makes the cooldown below
        // answer `Err` — `cooldown::serve` has nothing stored to hand back —
        // rather than quietly serving a stale list.

        let mut app = app(&server, open_db(tmp.path()));
        run(&mut app, &Watched::own()).await;
        assert!(
            snob_store::store::snapshots::latest_complete(app.db().conn(), 42, ListKind::Following)
                .unwrap()
                .is_none(),
            "the fixture depends on following having nothing stored"
        );
    }

    // Run two: followers loses somebody and is walked, and the cooldown lands
    // once that is paid for.
    let server = MockServer::start().await;
    mount_profile(&server, 1, 1).await;
    mount_list(&server, "followers", &[1]).await;
    mount_list(&server, "following", &[9]).await;

    let budget = Arc::new(CooldownAfterTheFirstList(Default::default()));
    let mut app = app_with(&server, open_db(tmp.path()), budget);
    let tick = watch::tick(&mut app, &Watched::own())
        .await
        .expect("the first list's news must survive the second list's cooldown");

    assert_eq!(
        tick.report.changes().followers.lost.len(),
        1,
        "the departure the first list found is what this run is for: {:?}",
        tick.report.changes()
    );
    assert!(
        tick.lists
            .iter()
            .any(|l| l.kind == ListKind::Following && l.skipped.is_some()),
        "and the list that could not be read says so: {:?}",
        tick.lists
    );
}

/// A rename in the list that *was* read is not announced again next run.
///
/// The other half of the same cursor, and it pulls the opposite way. Whether a
/// rename is reported is gated on there being any verified list; whether the
/// cursor may move is gated on *every* list being accounted for. So a tick with
/// one list refused announces what it can see and files nothing saying it did,
/// and the next tick re-reads the identical window against the identical
/// capture — under a fresh `run_id`, which is the value receivers are told to
/// deduplicate on. No permanent wall is needed: one refused list is enough.
#[tokio::test]
async fn a_rename_in_the_list_that_was_read_is_not_announced_again_next_run() {
    let tmp = tempfile::tempdir().unwrap();

    // Run one: baselines for both lists.
    {
        let server = MockServer::start().await;
        mount_profile(&server, 1, 1).await;
        mount_named(&server, "followers", &[(1, "one")]).await;
        mount_named(&server, "following", &[(2, "two")]).await;

        let mut app = app(&server, open_db(tmp.path()));
        run(&mut app, &Watched::own()).await;

        // A rename of somebody in the list that will be readable.
        snob_store::store::users::upsert(
            app.db().conn(),
            &snob_core::model::User {
                pk: 1,
                username: "one_renamed".into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            },
        )
        .unwrap();
    }

    // Run two: followers answers and is compared, following is refused. The
    // rename is announced, and the cursor may not move.
    {
        let server = MockServer::start().await;
        mount_profile(&server, 2, 2).await;
        mount_named(&server, "followers", &[(1, "one_renamed"), (3, "three")]).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/following/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("not the JSON this endpoint returns"),
            )
            .mount(&server)
            .await;

        let mut app = app(&server, open_db(tmp.path()));
        let tick = run(&mut app, &Watched::own()).await;
        let renamed = tick.report.changes().renamed;
        assert_eq!(renamed.len(), 1, "announced once: {renamed:?}");
        assert_eq!(renamed[0].pk, 1);
    }

    // Run three: both counters match what is stored, so both lists are verified
    // without a walk and the same window is read again.
    {
        let server = MockServer::start().await;
        mount_profile(&server, 2, 1).await;

        let mut app = app(&server, open_db(tmp.path()));
        let tick = run(&mut app, &Watched::own()).await;

        assert!(
            tick.report.changes().renamed.is_empty(),
            "it was announced last run; a second run_id for one event is what a \
             receiver cannot deduplicate: {:?}",
            tick.report.changes().renamed
        );
    }
}

/// A rename seen by a walk that stopped short is not stepped over.
///
/// `every_list_accounted_for` asks `latest_complete`, which is true of members
/// and false of `username_history`: a walk that ends `Truncated` has already run
/// `save_page` and `users::upsert`, so it raised `history_head` — while
/// `renames_since` joins the members of a *verified* capture, so those rows are
/// invisible. The cursor then closes over them, and recovery is shut: when that
/// list finally completes it is a `Baseline`, which `verified()` excludes.
#[tokio::test]
async fn a_rename_seen_by_a_walk_that_stopped_short_is_not_stepped_over() {
    let tmp = tempfile::tempdir().unwrap();

    // Run one: followers completes. Following declares a hundred and serves
    // one, so it hits the truncation wall — a walk that saved a page and has no
    // complete capture at all. `save_page` filed @two into `users` on the way.
    {
        let server = MockServer::start().await;
        mount_profile(&server, 1, 100).await;
        mount_named(&server, "followers", &[(1, "one")]).await;
        mount_named(&server, "following", &[(2, "two")]).await;

        let mut app = app(&server, open_db(tmp.path()));
        let tick = run(&mut app, &Watched::own()).await;
        assert!(
            tick.lists
                .iter()
                .any(|l| l.kind == ListKind::Following && l.skipped.is_some()),
            "the following list has to be the walled one: {:?}",
            tick.lists
        );

        // Somebody the walled walk saw renames.
        snob_store::store::users::upsert(
            app.db().conn(),
            &snob_core::model::User {
                pk: 2,
                username: "two_renamed".into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            },
        )
        .unwrap();
    }

    // Run two: followers compares again, following is still walled. The rename
    // is not found — @two is in no capture this run may read — and the cursor
    // must not close over it.
    {
        let server = MockServer::start().await;
        mount_profile(&server, 2, 100).await;
        mount_named(&server, "followers", &[(1, "one"), (3, "three")]).await;
        mount_named(&server, "following", &[(2, "two_renamed")]).await;

        let mut app = app(&server, open_db(tmp.path()));
        let tick = run(&mut app, &Watched::own()).await;
        assert!(tick.report.changes().renamed.is_empty());
    }

    // Run three onwards: the wall lifts, following completes and then settles
    // into `Unchanged`. The rename is owed, and it has to arrive exactly once.
    let mut announced = 0;
    for _ in 0..3 {
        let server = MockServer::start().await;
        mount_profile(&server, 2, 1).await;
        mount_named(&server, "followers", &[(1, "one"), (3, "three")]).await;
        mount_named(&server, "following", &[(2, "two_renamed")]).await;

        let mut app = app(&server, open_db(tmp.path()));
        let tick = run(&mut app, &Watched::own()).await;
        announced += tick.report.changes().renamed.len();
    }

    assert_eq!(
        announced, 1,
        "a rename the tool saw is reported once: not twice, and not never"
    );
}

/// The first run seeds the rename cursor, so the second does not announce
/// history from before the monitor existed.
///
/// `covered` was set only for a list this run *verified*, and `verified()`
/// excludes a baseline — so after a first run that laid one down the cursor was
/// still zero. The next run's lists are `Unchanged`, which does count, so it
/// read the window from the beginning of time: every `username_history` row
/// written by every ordinary `snob followers` since the first release,
/// announced as news on the monitor's second run. Excluding the baseline
/// deferred the very window it was written to suppress by exactly one run.
#[tokio::test]
async fn a_baseline_run_seeds_the_rename_cursor() {
    let tmp = tempfile::tempdir().unwrap();

    // History from before the monitor: this account was walked by hand and
    // somebody in it changed their name.
    {
        let db = open_db(tmp.path());
        snob_store::store::users::ensure(db.conn(), 42).unwrap();
        snob_store::store::accounts::upsert(db.conn(), 42, true).unwrap();
        for name in ["before", "after"] {
            snob_store::store::users::upsert(
                db.conn(),
                &snob_core::model::User {
                    pk: 1,
                    username: name.into(),
                    full_name: None,
                    is_private: None,
                    is_verified: None,
                    pfp_url: None,
                },
            )
            .unwrap();
        }
        assert_eq!(
            snob_store::store::watch::history_head(db.conn()).unwrap(),
            1,
            "there is a rename on record before the monitor ever runs"
        );
    }

    let server = MockServer::start().await;
    mount_profile(&server, 1, 1).await;
    mount_named(&server, "followers", &[(1, "after")]).await;
    mount_named(&server, "following", &[(1, "after")]).await;

    // The first run: baselines, and it must say nothing.
    let mut app = app(&server, open_db(tmp.path()));
    let first = run(&mut app, &Watched::own()).await;
    assert!(
        first.report.changes().renamed.is_empty(),
        "a baseline has nothing to compare against"
    );

    // And the second, with both counters unchanged, must not announce the
    // rename that was already on record before any of this started.
    let second = run(&mut app, &Watched::own()).await;
    assert!(
        second.report.changes().renamed.is_empty(),
        "run two announced history from before the monitor: {:?}",
        second.report.changes().renamed
    );
}

/// `snob watch check` spends nothing while the account is in cooldown.
///
/// It is built to be polled, and it was the one request path in the tool with
/// no cooldown gate: `Pacer::clear_to_send` charges the budget but never
/// reads the `cooldowns` table, so nothing below it would have caught this. One `validate` plus one `web_profile_info` per
/// configured account, on whatever interval a monitoring system polls at,
/// knocking on a door Instagram had just closed.
///
/// Reported rather than skipped in silence: a cooldown is exactly what somebody
/// running `check` wants to be told, and it lifts on its own, so it is a
/// warning and not a failure.
#[tokio::test]
async fn watch_check_spends_nothing_during_a_cooldown() {
    use snob_cli::engine::check::{CheckReport, Verdict, with_a_session};

    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let _schema = Store::open(&paths).unwrap();
    let budget = Arc::new(SqliteRateBudget::open(&paths).unwrap());

    let server = MockServer::start().await;
    mount_profile(&server, 2, 2).await;

    budget
        .start_cooldown("rate_limit", std::time::Duration::from_secs(2 * 3600))
        .unwrap();

    let app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());
    let secrets = snob_store::secrets::SecretStore::new(paths.clone(), false)
        .with_service(&format!("snob-ig-test-check-{}", std::process::id()));

    let mut report = CheckReport::default();
    with_a_session(&app, &secrets, &[Watched::own()], &mut report).await;

    assert_eq!(
        requests(&server).await,
        0,
        "nothing may be spent during a cooldown, and this is a request path like any other"
    );
    assert_eq!(
        report.verdict(),
        Verdict::Warned,
        "a cooldown lifts on its own, so it is not a failure"
    );
    assert!(
        report
            .checked
            .iter()
            .all(|c| c.problem.as_deref().is_some_and(|p| p.contains("cooldown"))),
        "and every line has to say why it was not checked: {:?}",
        report.checked
    );
}

/// One account earning a cooldown stops the ones after it being asked about.
///
/// The gate above answers for the moment `check` started, and `account_of`
/// folds a 429 into a `Failed` line rather than propagating — so the loop
/// walked on and knocked again. What that costs is not the extra requests, it
/// is the escalation ladder: `start_cooldown` doubles whenever the previous one
/// was set inside a day, so one `check` over three accounts turns a two-hour
/// throttle into eight.
#[tokio::test]
async fn check_stops_asking_once_an_account_earns_a_cooldown() {
    use snob_cli::engine::check::{CheckReport, Verdict, with_a_session};

    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let _schema = Store::open(&paths).unwrap();
    let budget = Arc::new(SqliteRateBudget::open(&paths).unwrap());

    let server = MockServer::start().await;
    // `validate()` asks a different endpoint, and it answers.
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/following/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
        .mount(&server)
        .await;
    // Every profile poll is thrown back.
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"message":"","spam":true}"#))
        .mount(&server)
        .await;

    let app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());
    let secrets = snob_store::secrets::SecretStore::new(paths.clone(), false)
        .with_service(&format!("snob-ig-test-throttle-{}", std::process::id()));

    let watched = [
        Watched::consented("one".into(), snob_cli::engine::watch::Consent),
        Watched::consented("two".into(), snob_cli::engine::watch::Consent),
        Watched::consented("three".into(), snob_cli::engine::watch::Consent),
    ];

    let mut report = CheckReport::default();
    with_a_session(&app, &secrets, &watched, &mut report).await;

    let polls = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/v1/users/web_profile_info/")
        .count();
    assert_eq!(
        polls, 1,
        "the first account earned the cooldown; the rest must not knock again"
    );
    assert_eq!(report.verdict(), Verdict::Failed);
    assert!(
        report
            .checked
            .iter()
            .filter(|c| c.problem.as_deref().is_some_and(|p| p.contains("cooldown")))
            .count()
            >= 2,
        "and the ones that were not asked about say why: {:?}",
        report.checked
    );
}

/// A cooldown is not a reason to call a monitor that cannot start healthy.
///
/// Whether an unattended run may read an account is a fact about the file. No
/// cooldown affects it, and `commands::watch` refuses to start without it — so
/// reporting it as a warning because a cooldown happened to be standing had
/// `check` exit 0 about a monitor that cannot run at all.
#[tokio::test]
async fn a_cooldown_does_not_downgrade_a_missing_consent() {
    use snob_cli::engine::check::{CheckReport, Verdict, with_a_session};

    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let _schema = Store::open(&paths).unwrap();
    let budget = Arc::new(SqliteRateBudget::open(&paths).unwrap());
    budget
        .start_cooldown("rate_limit", std::time::Duration::from_secs(2 * 3600))
        .unwrap();

    let server = MockServer::start().await;
    let app = app_with(&server, Store::open(&paths).unwrap(), budget.clone());
    let secrets = snob_store::secrets::SecretStore::new(paths.clone(), false)
        .with_service(&format!("snob-ig-test-consent-{}", std::process::id()));

    let mut report = CheckReport::default();
    with_a_session(
        &app,
        &secrets,
        &[Watched::asking("stranger".into())],
        &mut report,
    )
    .await;

    assert_eq!(requests(&server).await, 0, "still nothing is spent");
    assert_eq!(
        report.verdict(),
        Verdict::Failed,
        "a scheduled run would refuse to start, and the probe has to say so: {:?}",
        report.checked
    );
}

/// A session that has never learned its own name is resolved, not waved through.
///
/// The arm that handled it returned `Ok` under a comment saying resolving was
/// "what `validate` above has just done for free". It had not: `validate`
/// requests `/api/v1/friendships/{id}/following/?count=1`, which names no
/// account and takes `&self`, so it could not have stored one. Two checks were
/// reported as passed without being made -- the account, and the baseline, which
/// `with_a_session` only asks about when the account line carries a pk.
///
/// Not a corner case. `snob login --paste` during a cooldown stores the session
/// without validating it, so the name stays empty, and only `whoami` ever fills
/// it in -- which nothing on a headless machine runs.
#[tokio::test]
async fn a_session_with_no_stored_username_is_resolved_rather_than_waved_through() {
    use snob_cli::engine::check::{CheckReport, What, with_a_session};

    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let _schema = Store::open(&paths).unwrap();

    let server = MockServer::start().await;
    // `validate()`: the session works.
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/following/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
        .mount(&server)
        .await;
    // The one request this arm has to make, and did not.
    Mock::given(method("GET"))
        .and(path("/api/v1/users/42/info/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"user":{"username":"me"}}"#))
        .mount(&server)
        .await;
    mount_profile(&server, 7, 3).await;

    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::new(Arc::new(UnlimitedRateBudget)))
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());
    let app = App::for_test(
        client,
        Store::open(&paths).unwrap(),
        Viewer {
            pk: 42,
            username: None,
        },
    );
    let secrets = snob_store::secrets::SecretStore::new(paths.clone(), false)
        .with_service(&format!("snob-ig-test-noname-{}", std::process::id()));

    let mut report = CheckReport::default();
    with_a_session(&app, &secrets, &[Watched::own()], &mut report).await;

    let account = report
        .checked
        .iter()
        .find_map(|c| match &c.what {
            What::Account {
                pk,
                followers,
                following,
                ..
            } => Some((*pk, *followers, *following)),
            _ => None,
        })
        .expect("the account is checked");
    assert_eq!(
        account,
        (Some(42), Some(7), Some(3)),
        "the account line has to carry what was really checked: {:?}",
        report.checked
    );
    assert!(
        report
            .checked
            .iter()
            .any(|c| matches!(c.what, What::Baseline { .. })),
        "and the baseline is only asked about when the account line carries a pk: {:?}",
        report.checked
    );
}

/// And the number the help gives is the number that is really spent.
///
/// A sentence about cost is only worth having if something breaks when it stops
/// being true. One `validate` for the session and one `web_profile_info` per
/// configured account, every invocation, against a budget of roughly two
/// thousand requests a day that the walks are also drawing on.
///
/// Twice rather than once, because a per-invocation cost is exactly what a probe
/// multiplies: the question is not what one poll costs, it is what a thousand of
/// them cost.
#[tokio::test]
async fn checking_twice_spends_twice() {
    use snob_cli::engine::check::{CheckReport, with_a_session};

    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let _schema = Store::open(&paths).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/42/following/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
        .mount(&server)
        .await;
    mount_profile(&server, 2, 2).await;

    let app = app(&server, Store::open(&paths).unwrap());
    let secrets = snob_store::secrets::SecretStore::new(paths.clone(), false)
        .with_service(&format!("snob-ig-test-cost-{}", std::process::id()));
    let watched = [Watched::own()];

    let before = app.client().pacer().spent();
    let mut first = CheckReport::default();
    with_a_session(&app, &secrets, &watched, &mut first).await;
    let after_one = app.client().pacer().spent();

    let mut second = CheckReport::default();
    with_a_session(&app, &secrets, &watched, &mut second).await;
    let after_two = app.client().pacer().spent();

    assert_eq!(
        after_one - before,
        2,
        "one for the session and one per configured account, which is what the help says"
    );
    assert_eq!(
        after_two - after_one,
        2,
        "and a probe pays it again every time it is polled"
    );
}
