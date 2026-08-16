//! The early refusal of private accounts the viewer does not follow.
//!
//! Checked end to end because what matters is *when* it happens: before any
//! walk, so no request is spent on lists Instagram would never serve.

use snob_core::model::ListKind;
use snob_core::session::{Session, SessionOrigin};
use snob_core::store::Store;
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use snob_cli::app::{App, Viewer};
use snob_cli::cli::ListArgs;
use snob_cli::engine::{self, ListOutcome};

mod common;
use common::{SID, UA};

/// The account every test here asks about.
fn args() -> ListArgs {
    common::args_for("@ghost")
}

/// The profile endpoint answering with whatever user object each test needs.
async fn mount_profile(server: &MockServer, user: &str) {
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!(r#"{{"data":{{"user":{user}}}}}"#)),
        )
        .mount(server)
        .await;
}

async fn mount_list(server: &MockServer, pk: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/friendships/{pk}/followers/")))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"users":[{"pk":1,"username":"u1"}]}"#),
        )
        .mount(server)
        .await;
}

async fn execute_with(
    server: &MockServer,
    db: Store,
    args: &ListArgs,
) -> anyhow::Result<(Vec<snob_core::model::User>, ListOutcome)> {
    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());

    let mut app = App::for_test(
        client,
        db,
        Viewer {
            pk: 42,
            username: Some("me".into()),
        },
    );
    engine::list(&mut app, args, ListKind::Followers).await
}

async fn execute(
    server: &MockServer,
) -> anyhow::Result<(Vec<snob_core::model::User>, ListOutcome)> {
    let tmp = tempfile::tempdir().unwrap();
    let db = Store::open_at(&tmp.path().join("test.db")).unwrap();
    execute_with(server, db, &args()).await
}

/// How many requests the server has received so far.
async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn a_private_account_you_do_not_follow_fails_before_any_walk() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":99,"username":"ghost","is_private":true,"followed_by_viewer":false}"#,
    )
    .await;

    let error = execute(&server).await.unwrap_err().to_string();
    assert!(
        error.contains("private account you do not follow"),
        "{error}"
    );
    assert_eq!(
        requests(&server).await,
        1,
        "nothing beyond the profile lookup may be spent"
    );
}

#[tokio::test]
async fn a_pending_request_gets_its_own_message() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":99,"username":"ghost","is_private":true,"followed_by_viewer":false,"requested_by_viewer":true}"#,
    )
    .await;

    let error = execute(&server).await.unwrap_err().to_string();
    assert!(
        error.contains("follow request has not been accepted"),
        "{error}"
    );
}

/// The fail-open contract: this API has no promises, and a missing field must
/// never turn into a refusal.
#[tokio::test]
async fn an_unknown_relationship_does_not_block() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":99,"username":"ghost","is_private":true,
            "edge_followed_by":{"count":1},"edge_follow":{"count":1}}"#,
    )
    .await;
    mount_list(&server, 99).await;

    let (found, _) = execute(&server).await.unwrap();
    assert_eq!(found.len(), 1);
}

#[tokio::test]
async fn a_private_account_you_follow_is_walked() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":99,"username":"ghost","is_private":true,"followed_by_viewer":true,
            "edge_followed_by":{"count":1},"edge_follow":{"count":1}}"#,
    )
    .await;
    mount_list(&server, 99).await;

    let (found, _) = execute(&server).await.unwrap();
    assert_eq!(found.len(), 1);
}

/// With --cache no walk would be spent, so there is nothing to protect: the
/// stored snapshot is served even if the account has since gone private.
#[tokio::test]
async fn with_cache_the_stored_snapshot_is_still_served() {
    let tmp = tempfile::tempdir().unwrap();
    let db = || Store::open_at(&tmp.path().join("test.db")).unwrap();

    // Seed the snapshot while the account was public.
    let public = MockServer::start().await;
    mount_profile(
        &public,
        r#"{"id":99,"username":"ghost","edge_followed_by":{"count":1},"edge_follow":{"count":1}}"#,
    )
    .await;
    mount_list(&public, 99).await;
    execute_with(&public, db(), &args()).await.unwrap();

    // Now the account is private and unfollowed, and only stored data is
    // asked for.
    let private = MockServer::start().await;
    mount_profile(
        &private,
        r#"{"id":99,"username":"ghost","is_private":true,"followed_by_viewer":false}"#,
    )
    .await;
    let mut cached = args();
    cached.cache = true;
    let (found, outcome) = execute_with(&private, db(), &cached).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(outcome.requests, 0);
}

#[tokio::test]
async fn your_own_account_is_never_blocked() {
    let server = MockServer::start().await;
    // A target that resolves to the logged-in account, private or not.
    mount_profile(
        &server,
        r#"{"id":42,"username":"ghost","is_private":true,"followed_by_viewer":false,
            "edge_followed_by":{"count":1},"edge_follow":{"count":1}}"#,
    )
    .await;
    mount_list(&server, 42).await;

    let (found, _) = execute(&server).await.unwrap();
    assert_eq!(found.len(), 1);
}

/// The refusal names the account, and that name came off Instagram.
///
/// Every sibling that prints this field filters it — `walk.rs`, `pfp.rs`,
/// `people::name_a_few` — and this one did not, so a private account whose
/// username carried `\x1b[2K\x1b[A` erased the line the tool had just printed.
/// It reaches the terminal through `report::print_error`, which is where the
/// whole chain is written out.
#[tokio::test]
async fn the_refusal_names_a_hostile_account_without_obeying_it() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":99,"username":"gh\u001b[2K\u001b[Aost","is_private":true,
            "followed_by_viewer":false}"#,
    )
    .await;

    let error = execute(&server)
        .await
        .expect_err("a private account you do not follow is refused");
    let printed = format!("{error:#}");

    assert!(!printed.contains('\x1b'), "{printed:?}");
    assert!(printed.contains("private"), "{printed}");
}
