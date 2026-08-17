//! What the monitor reports out of storage, over a real database.
//!
//! The unit tests cover the rules one at a time. These drive the whole path —
//! walk, mark, walk again, compare — because the defect this feature can have
//! is not in any one rule but in the order they run in: a mark moved at the
//! wrong moment reports the right change twice, or never.
//!
//! Nothing here touches the network. `snob watch diff` spends no request, and a
//! client is built only because an `App` needs one; if any of this ever reached
//! for it, the mock server it points at has nothing mounted and would say so.

use snob_core::model::{ListKind, StopReason, User};
use snob_core::session::{Session, SessionOrigin};
use snob_core::store::{Store, accounts, snapshots, users};
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use url::Url;
use wiremock::MockServer;

use snob_cli::app::{App, Viewer};
use snob_cli::engine::watch;

mod common;
use common::{SID, UA};

const ME: u64 = 42;

fn user(pk: u64, name: &str) -> User {
    User {
        pk,
        username: name.into(),
        full_name: None,
        is_private: None,
        is_verified: None,
        pfp_url: None,
    }
}

fn users(names: &[(u64, &str)]) -> Vec<User> {
    names.iter().map(|&(pk, n)| user(pk, n)).collect()
}

/// An app over this database. The server has nothing mounted, so any request
/// would fail loudly rather than quietly succeeding.
fn app(server: &MockServer, db: Store) -> App {
    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());
    App::for_test(
        client,
        db,
        Viewer {
            pk: ME,
            username: Some("me".into()),
        },
    )
}

/// Stores a finished walk of `kind` holding `members`, the way a real one does.
fn walked(db: &mut Store, kind: ListKind, members: &[User]) -> i64 {
    users::ensure(db.conn(), ME).unwrap();
    accounts::upsert(db.conn(), ME, true).unwrap();
    let opened = snapshots::begin(db.conn(), ME, kind, Some(members.len() as u64)).unwrap();
    snapshots::save_page(db, opened.id, members, None).unwrap();
    snapshots::close(db.conn(), opened.id, StopReason::Completed).unwrap();
    opened.id
}

/// The same, once the app owns the database. `parts()` is what hands out the
/// mutable store; going through it keeps the tests from needing an accessor
/// that only tests would ever call.
fn walked_in(app: &mut App, kind: ListKind, members: &[User]) -> i64 {
    walked(app.parts().1, kind, members)
}

fn cut_short_in(app: &mut App, kind: ListKind, members: &[User]) -> i64 {
    cut_short(app.parts().1, kind, members)
}

/// A walk that stopped short, which must never become a basis for comparison.
fn cut_short(db: &mut Store, kind: ListKind, members: &[User]) -> i64 {
    users::ensure(db.conn(), ME).unwrap();
    accounts::upsert(db.conn(), ME, true).unwrap();
    let opened = snapshots::begin(db.conn(), ME, kind, Some(9_000)).unwrap();
    snapshots::save_page(db, opened.id, members, Some("cursor")).unwrap();
    snapshots::close(db.conn(), opened.id, StopReason::Truncated).unwrap();
    opened.id
}

/// The worst thing this feature could do. Somebody with fourteen hundred
/// followers running it for the first time must not be told that fourteen
/// hundred people just arrived.
#[tokio::test]
async fn a_first_look_reports_nothing_at_all() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "a"), (2, "b")]));

    let app = app(&server, db);
    let report = watch::from_store(&app, None, true).unwrap();

    assert!(
        report.changes().is_empty(),
        "a first look has nothing to compare against"
    );
    assert!(matches!(
        report.followers.unwrap().basis,
        snob_core::watch::Basis::Baseline { .. }
    ));
}

/// And having said nothing, it has to remember that it looked — otherwise
/// every run is a first run and the monitor never reports anything, ever.
#[tokio::test]
async fn a_first_look_leaves_a_mark_so_the_next_one_can_speak() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "a"), (2, "b")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    // Somebody new turns up and somebody else leaves.
    walked_in(&mut app, ListKind::Followers, &users(&[(1, "a"), (3, "c")]));

    let report = watch::from_store(&app, None, true).unwrap();
    let changes = report.changes();

    assert_eq!(changes.followers.gained.len(), 1, "one arrived");
    assert_eq!(changes.followers.gained[0].username, "c");
    assert_eq!(changes.followers.lost.len(), 1, "one left");
    assert_eq!(changes.followers.lost[0].username, "b");
}

/// Looking is not reporting. `snob watch diff` must be askable twice.
#[tokio::test]
async fn asking_without_advancing_gives_the_same_answer_twice() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "a")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();
    walked_in(&mut app, ListKind::Followers, &users(&[(1, "a"), (2, "b")]));

    let first = watch::from_store(&app, None, false).unwrap();
    let second = watch::from_store(&app, None, false).unwrap();

    assert_eq!(first.changes().len(), 1);
    assert_eq!(
        second.changes().len(),
        first.changes().len(),
        "asking a question must not change its answer"
    );
}

/// Once it has been reported, it is not news again. This is the failure a mark
/// exists to prevent: without one, every run would re-announce the same arrival
/// forever.
#[tokio::test]
async fn a_change_is_reported_once_and_not_again() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "a")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();
    walked_in(&mut app, ListKind::Followers, &users(&[(1, "a"), (2, "b")]));

    assert_eq!(
        watch::from_store(&app, None, true).unwrap().changes().len(),
        1
    );
    assert!(
        watch::from_store(&app, None, true)
            .unwrap()
            .changes()
            .is_empty(),
        "the same arrival must not be announced twice"
    );
}

/// Nothing walked since the last report is told apart from a comparison that
/// found nothing: one of them never had to read a member row.
#[tokio::test]
async fn a_list_nothing_has_touched_reads_as_unchanged() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "a")]));

    let app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    let report = watch::from_store(&app, None, true).unwrap();
    assert!(matches!(
        report.followers.unwrap().basis,
        snob_core::watch::Basis::Unchanged { .. }
    ));
}

/// The guard the whole comparison rests on. A walk that stopped early is
/// missing accounts, and comparing against it would report every one of them as
/// somebody who left.
#[tokio::test]
async fn a_walk_that_stopped_short_is_never_compared_against() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(
        &mut db,
        ListKind::Followers,
        &users(&[(1, "a"), (2, "b"), (3, "c")]),
    );

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    // A later walk that only got one page before Instagram stopped serving.
    cut_short_in(&mut app, ListKind::Followers, &users(&[(1, "a")]));

    let report = watch::from_store(&app, None, true).unwrap();
    assert!(
        report.changes().is_empty(),
        "two accounts would have been reported as departures because a walk was cut short"
    );
}

/// The two lists are marked apart, so reporting one must not silence the other.
#[tokio::test]
async fn reporting_the_followers_does_not_silence_the_following() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "a")]));
    walked(&mut db, ListKind::Following, &users(&[(5, "e")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    walked_in(&mut app, ListKind::Following, &users(&[(5, "e"), (6, "f")]));

    let report = watch::from_store(&app, None, true).unwrap();
    assert_eq!(report.changes().following.gained.len(), 1);
    assert_eq!(report.changes().following.gained[0].username, "f");
}

/// A command the user typed between two runs leaves a newer capture behind.
/// The diff still has to cover everything since the last **report**, not since
/// that capture — otherwise running `snob followers` by hand quietly swallows
/// whatever changed before it.
#[tokio::test]
async fn a_manual_run_between_reports_does_not_swallow_the_changes() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "a")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    // Somebody arrives, and the user happens to run `snob followers` themselves.
    walked_in(&mut app, ListKind::Followers, &users(&[(1, "a"), (2, "b")]));
    // Then somebody else arrives, and the monitor finally looks.
    walked_in(
        &mut app,
        ListKind::Followers,
        &users(&[(1, "a"), (2, "b"), (3, "c")]),
    );

    let report = watch::from_store(&app, None, true).unwrap();
    let changes = report.changes();
    let gained: Vec<&str> = changes
        .followers
        .gained
        .iter()
        .map(|u| u.username.as_str())
        .collect();

    assert_eq!(
        gained,
        vec!["b", "c"],
        "both arrivals happened since the last report, so both are news"
    );
}

/// A rename is reported once, from the list, and does not also show up as
/// somebody leaving and somebody else arriving.
#[tokio::test]
async fn a_rename_is_reported_as_a_rename_and_nothing_else() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "before")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    walked_in(&mut app, ListKind::Followers, &users(&[(1, "after")]));

    let report = watch::from_store(&app, None, true).unwrap();
    let changes = report.changes();

    assert!(
        changes.followers.is_empty(),
        "the same person did not leave and arrive"
    );
    assert_eq!(changes.renamed.len(), 1);
    assert_eq!(changes.renamed[0].from, "before");
    assert_eq!(changes.renamed[0].to, "after");
}

/// The defect that made renames useless for exactly the people this tool is
/// about.
///
/// Somebody you follow who does not follow you back is in `following` and not
/// in `followers` — the `unfollowers` set, the whole point of the program. The
/// rename window used to be read from the followers capture alone, so their
/// rename was never found, and the cursor moved past it anyway: not reported
/// then, and not reported ever.
#[tokio::test]
async fn a_rename_of_somebody_only_in_the_following_list_is_reported() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "a_follower")]));
    walked(&mut db, ListKind::Following, &users(&[(2, "before")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    walked_in(&mut app, ListKind::Followers, &users(&[(1, "a_follower")]));
    walked_in(&mut app, ListKind::Following, &users(&[(2, "after")]));

    let report = watch::from_store(&app, None, true).unwrap();
    let changes = report.changes();

    assert_eq!(changes.renamed.len(), 1, "they are in a list this watches");
    assert_eq!(changes.renamed[0].from, "before");
    assert_eq!(changes.renamed[0].to, "after");
}

/// The same defect from the other side: with followers unchanged and only the
/// following list walked, no rename was reported at all.
#[tokio::test]
async fn a_rename_is_reported_when_only_the_following_list_moved() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "a_follower")]));
    walked(&mut db, ListKind::Following, &users(&[(2, "before")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    // Only the following list is walked again — what happens when the followers
    // counter has not moved.
    walked_in(&mut app, ListKind::Following, &users(&[(2, "after")]));

    let report = watch::from_store(&app, None, true).unwrap();
    assert_eq!(
        report.changes().renamed.len(),
        1,
        "the list that was walked had a rename in it"
    );
}

/// And the third: a rename must not be announced twice because one list was
/// refused and its cursor stayed behind.
#[tokio::test]
async fn a_rename_is_not_repeated_when_one_list_was_refused() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "before")]));
    walked(&mut db, ListKind::Following, &users(&[(1, "before")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    // A rename, seen by a followers walk that finished and a following walk
    // that did not — so only the followers mark moves.
    walked_in(&mut app, ListKind::Followers, &users(&[(1, "after")]));
    cut_short_in(&mut app, ListKind::Following, &users(&[(1, "after")]));

    assert_eq!(
        watch::from_store(&app, None, true)
            .unwrap()
            .changes()
            .renamed
            .len(),
        1,
        "reported once"
    );

    walked_in(&mut app, ListKind::Following, &users(&[(1, "after")]));
    assert!(
        watch::from_store(&app, None, true)
            .unwrap()
            .changes()
            .renamed
            .is_empty(),
        "and not again once the other list catches up"
    );
}

/// A rename filed while both lists sit unchanged is reported, not stepped over.
///
/// Two unmoved counters is the documented common case -- "a run with nothing to
/// report costs one request" -- and such a run used to read no rename window at
/// all, because the window was only read for lists that had been *compared*. It
/// advanced the cursor to the global head of `username_history` regardless, so
/// anything another walk had filed in between was marked as reported without
/// having been, and could never be reported by anything afterwards, `snob watch
/// diff` included.
///
/// A rename moves nobody in or out of a list, so an unchanged capture is exactly
/// as good a set of members to look among as one that was walked.
#[tokio::test]
async fn a_rename_filed_while_the_lists_sat_still_is_still_reported() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "before")]));
    walked(&mut db, ListKind::Following, &users(&[(1, "before")]));

    let app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    // Filed by something else -- another account's walk that happens to include
    // them, or a `snob pfp` -- so neither list has a newer capture and both read
    // as unchanged.
    users::upsert(app.db().conn(), &user(1, "after")).unwrap();

    let changes = watch::from_store(&app, None, true).unwrap().changes();
    assert_eq!(changes.renamed.len(), 1, "the rename was stepped over");
    assert_eq!(changes.renamed[0].from, "before");

    assert!(
        watch::from_store(&app, None, true)
            .unwrap()
            .changes()
            .renamed
            .is_empty(),
        "and it is not announced a second time"
    );
}

/// A list that has never been reported does not drag the rename window back.
///
/// The cursor was a column on `watch_marks`, so an account had one per list, and
/// the window started at the older of the two -- necessarily, or the gap between
/// them would be skipped for anybody in only one list. So the first run in which
/// a second list appeared read from that list's zero and re-announced every
/// rename already sent, under a new `run_id`, which is the value receivers are
/// told to deduplicate on. There is one cursor per account now, and no pair to
/// pick between.
#[tokio::test]
async fn a_list_reported_for_the_first_time_does_not_replay_old_renames() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "before")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    walked_in(&mut app, ListKind::Followers, &users(&[(1, "after")]));
    assert_eq!(
        watch::from_store(&app, None, true)
            .unwrap()
            .changes()
            .renamed
            .len(),
        1,
        "reported once, from the only list there is"
    );

    // Now the following list exists for the first time, with the same person in
    // it. It has never been reported, so it lays a baseline -- and it must not
    // reopen a window that has already been sent.
    walked_in(&mut app, ListKind::Following, &users(&[(1, "after")]));
    assert!(
        watch::from_store(&app, None, true)
            .unwrap()
            .changes()
            .renamed
            .is_empty(),
        "the rename was announced twice"
    );
}

/// Somebody in both lists is one person. Asking each list separately would file
/// their rename twice.
#[tokio::test]
async fn a_friend_who_renamed_themselves_is_reported_once() {
    let server = MockServer::start().await;
    let mut db = Store::in_memory().unwrap();
    walked(&mut db, ListKind::Followers, &users(&[(1, "before")]));
    walked(&mut db, ListKind::Following, &users(&[(1, "before")]));

    let mut app = app(&server, db);
    watch::from_store(&app, None, true).unwrap();

    walked_in(&mut app, ListKind::Followers, &users(&[(1, "after")]));
    walked_in(&mut app, ListKind::Following, &users(&[(1, "after")]));

    let report = watch::from_store(&app, None, true).unwrap();
    assert_eq!(
        report.changes().renamed.len(),
        1,
        "one person changed their name once"
    );
}

/// Nothing walked at all is a different answer from nothing changed, and the
/// caller has to be able to tell them apart to say something useful.
#[tokio::test]
async fn an_account_with_nothing_walked_reports_nothing_stored() {
    let server = MockServer::start().await;
    let db = Store::in_memory().unwrap();
    let app = app(&server, db);

    let report = watch::from_store(&app, None, true).unwrap();
    assert!(!report.has_anything_stored());
    assert!(report.changes().is_empty());
}

/// An account nobody has ever walked cannot be resolved without the network,
/// and the refusal has to say what to do rather than name a flag this command
/// does not have.
#[tokio::test]
async fn an_unknown_account_is_refused_with_something_to_do_about_it() {
    let server = MockServer::start().await;
    let db = Store::in_memory().unwrap();
    let app = app(&server, db);

    let error =
        watch::from_store(&app, Some("stranger"), false).expect_err("nothing is stored about them");
    let text = error.to_string();

    assert!(text.contains("stranger"), "{text}");
    assert!(text.contains("snob followers"), "{text}");
    assert!(
        !text.contains("--cache"),
        "this command has no --cache: {text}"
    );
}
