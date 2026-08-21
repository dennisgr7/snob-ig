//! The people you both know, end to end.
//!
//! Checked against a real database rather than in isolation because the whole
//! point of the feature is where the answer comes from: your own stored
//! `following` list, with no request spent. A unit test over two vectors would
//! prove the intersection and miss that entirely.

use snob_core::model::{ListKind, StopReason, User};
use snob_core::session::{Session, SessionOrigin};
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use snob_store::store::{Store, accounts, snapshots, users};
use url::Url;
use wiremock::MockServer;

use snob_cli::app::{App, Viewer};
use snob_cli::engine::people;

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

/// Stores a complete list for an account, the way a finished walk would.
fn store_list(db: &mut Store, account: u64, kind: ListKind, members: &[User]) {
    users::upsert(db.conn(), &user(account, &format!("account{account}"))).unwrap();
    accounts::upsert(db.conn(), account, account == ME).unwrap();

    let id = snapshots::begin(db.conn(), account, kind, Some(members.len() as u64))
        .unwrap()
        .id;
    snapshots::save_page(db, id, members, None).unwrap();
    snapshots::close(db.conn(), id, StopReason::Completed).unwrap();
}

/// Stores a list and leaves it half-finished, which is what an interrupted
/// walk produces.
fn store_partial(db: &mut Store, account: u64, kind: ListKind, members: &[User]) {
    users::upsert(db.conn(), &user(account, &format!("account{account}"))).unwrap();
    accounts::upsert(db.conn(), account, account == ME).unwrap();

    let id = snapshots::begin(db.conn(), account, kind, Some(999))
        .unwrap()
        .id;
    snapshots::save_page(db, id, members, Some("more")).unwrap();
    snapshots::close(db.conn(), id, StopReason::Canceled).unwrap();
}

async fn app_over(db: Store) -> (MockServer, App) {
    let server = MockServer::start().await;
    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());

    let app = App::for_test(
        client,
        db,
        Viewer {
            pk: ME,
            username: Some("me".into()),
        },
    );
    (server, app)
}

fn names(people: &[User]) -> Vec<&str> {
    people.iter().map(|u| u.username.as_str()).collect()
}

/// The answer is the accounts you follow who are also among theirs, and it
/// costs nothing: the mock server is mounted with no routes at all, so any
/// request would fail the test rather than pass it quietly.
#[tokio::test]
async fn it_names_the_accounts_you_follow_who_follow_them_too() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Store::open_at(&tmp.path().join("test.db")).unwrap();

    store_list(
        &mut db,
        ME,
        ListKind::Following,
        &[user(1, "ana"), user(2, "luis"), user(3, "eva")],
    );

    let (server, app) = app_over(db).await;
    // Their followers: two people I follow, one I do not.
    let theirs = vec![user(2, "luis"), user(9, "stranger"), user(1, "ana")];

    let found = people::in_common(&app, &theirs).unwrap().unwrap();

    // Ordered by my own following list, not by theirs: these are names I am
    // meant to recognize, and that is the list I know.
    assert_eq!(names(&found.people), vec!["ana", "luis"]);
    assert!(
        found.taken_at > 0,
        "the answer says which capture it was worked out from"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

/// "Nobody" and "we could not look" are different answers and the caller
/// prints them differently, so they must not collapse into one here.
#[tokio::test]
async fn nobody_in_common_is_not_the_same_as_nothing_stored() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Store::open_at(&tmp.path().join("test.db")).unwrap();
    store_list(&mut db, ME, ListKind::Following, &[user(1, "ana")]);

    let (_server, app) = app_over(db).await;
    let strangers = vec![user(9, "stranger")];
    let found = people::in_common(&app, &strangers).unwrap();
    assert!(
        found.is_some_and(|f| f.people.is_empty()),
        "an empty overlap is an answer"
    );

    // A second database, with no list of my own in it at all.
    let empty = tempfile::tempdir().unwrap();
    let (_server, app) = app_over(Store::open_at(&empty.path().join("test.db")).unwrap()).await;
    assert!(
        people::in_common(&app, &strangers).unwrap().is_none(),
        "with nothing stored there is no answer to give"
    );
}

/// A partial list of my own following would leave people out of the overlap,
/// and naming two acquaintances when there are nine is worse than naming none.
#[tokio::test]
async fn a_half_finished_list_of_my_own_is_not_used() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Store::open_at(&tmp.path().join("test.db")).unwrap();
    store_partial(&mut db, ME, ListKind::Following, &[user(1, "ana")]);

    let (_server, app) = app_over(db).await;
    let theirs = vec![user(1, "ana")];

    assert!(
        people::in_common(&app, &theirs).unwrap().is_none(),
        "an incomplete list of my own would understate the overlap"
    );
}

/// My followers are not my following. Crossing the wrong one would answer
/// "people who follow me and follow them", which is a different question.
#[tokio::test]
async fn the_wrong_list_of_mine_does_not_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Store::open_at(&tmp.path().join("test.db")).unwrap();
    store_list(&mut db, ME, ListKind::Followers, &[user(1, "ana")]);

    let (_server, app) = app_over(db).await;
    assert!(
        people::in_common(&app, &[user(1, "ana")])
            .unwrap()
            .is_none(),
        "only my own following list can answer this"
    );
}
