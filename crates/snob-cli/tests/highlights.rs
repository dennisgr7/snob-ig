//! `snob highlights` against a mock server: what is asked, what is not, and
//! how the tray becomes the numbered listing the command acts on.
//!
//! The binary-level behavior — the file names, the refusals by number, the
//! GET-only property — is in `sandbox.rs` beside the story tests it mirrors.
//! What is here is the library half: the visibility rule, the mapping from
//! the wire tray to the listing, and the prefixed reel id the items are
//! fetched with.

mod common;

use common::{SID, UA};
use snob_cli::commands::highlights::{Fetched, fetch_tray, items_of_entry};
use snob_core::Pk;
use snob_core::session::{Session, SessionOrigin};
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const VIEWER: Pk = Pk::new(42);

fn client(server: &MockServer) -> IgClient {
    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    IgClient::new(session, Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap())
}

/// The fields `fetch_tray` reads from the profile, and nothing else.
fn profile_body(id: u64, private: bool, followed: bool) -> String {
    format!(
        r#"{{"data":{{"user":{{"id":"{id}","username":"someone",
        "is_private":{private},"followed_by_viewer":{followed},
        "edge_followed_by":{{"count":244}},"edge_follow":{{"count":319}}}}}},"status":"ok"}}"#
    )
}

async fn mount_profile(server: &MockServer, body: String) {
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

fn paths(requests: &[wiremock::Request]) -> Vec<String> {
    requests.iter().map(|r| r.url.path().to_string()).collect()
}

/// A private account the viewer does not follow is answered from the profile
/// alone: the tray is never asked for, and the answer says "kept back", not
/// "none". The tray endpoint answers both cases with an empty list, so the
/// distinction can only come from not asking.
#[tokio::test]
async fn a_private_account_you_do_not_follow_is_not_asked_for_its_tray() {
    let server = MockServer::start().await;
    mount_profile(&server, profile_body(7, true, false)).await;

    let fetched = fetch_tray(&client(&server), "someone", VIEWER)
        .await
        .unwrap();

    let Fetched::Hidden { username } = fetched else {
        panic!("a hidden tray must be its own case, not an empty one");
    };
    assert_eq!(username, "someone");
    let asked = paths(&server.received_requests().await.unwrap());
    assert!(
        !asked.iter().any(|p| p.contains("highlights_tray")),
        "the tray was asked for although the account hides it: {asked:?}"
    );
}

/// A private account the viewer follows serves its tray like a public one,
/// and the wire fields land where the listing reads them — the title
/// defaulted to empty, the count and dates carried through.
#[tokio::test]
async fn a_followed_private_account_serves_its_tray() {
    let server = MockServer::start().await;
    mount_profile(&server, profile_body(7, true, true)).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/highlights/7/highlights_tray/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tray":[
                {"id":"highlight:100","title":"trip","media_count":5,
                 "created_at":1600000000,"updated_timestamp":1700000000},
                {"id":"highlight:200","media_count":1}
            ],"status":"ok"}"#,
        ))
        .mount(&server)
        .await;

    let fetched = fetch_tray(&client(&server), "someone", VIEWER)
        .await
        .unwrap();

    let Fetched::Tray(tray) = fetched else {
        panic!("a followed private account shows its tray");
    };
    assert_eq!(tray.username, "someone");
    assert_eq!(tray.entries.len(), 2);
    assert_eq!(tray.entries[0].id, "highlight:100");
    assert_eq!(tray.entries[0].title, "trip");
    assert_eq!(tray.entries[0].declared_items, Some(5));
    assert_eq!(
        tray.entries[1].title, "",
        "an absent title is empty, not a crash and not a placeholder"
    );
}

/// The items are fetched through the reel endpoint with the tray's own
/// spelling of the id, prefix included — that prefix is how `reels_media`
/// tells a highlight from an account.
#[tokio::test]
async fn the_items_are_asked_for_under_the_prefixed_id() {
    let server = MockServer::start().await;
    mount_profile(&server, profile_body(7, false, false)).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/highlights/7/highlights_tray/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tray":[{"id":"highlight:100","title":"trip","media_count":2}],"status":"ok"}"#,
        ))
        .mount(&server)
        .await;
    // Matched on the query, so a request under a bare or wrongly spelled id
    // finds no mock and fails the test as an unexpected 404.
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .and(query_param("reel_ids", "highlight:100"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"reels_media":[{"items":[
                {"pk":"1","media_type":1,"taken_at":1600000000,
                 "image_versions2":{"candidates":[
                    {"url":"https://scontent.cdninstagram.com/a.jpg","width":1080,"height":1920}]}}
            ]}],"status":"ok"}"#,
        ))
        .mount(&server)
        .await;

    let client = client(&server);
    let Fetched::Tray(tray) = fetch_tray(&client, "someone", VIEWER).await.unwrap() else {
        panic!("visible");
    };
    let items = items_of_entry(&client, &tray.entries[0]).await.unwrap();

    assert_eq!(items.len(), 1);
    assert!(
        items[0].expiring_at.is_none(),
        "a highlight item does not expire, and must not claim to"
    );
    assert_eq!(
        items[0].url.as_deref(),
        Some("https://scontent.cdninstagram.com/a.jpg")
    );
}

/// An id the reel no longer answers for is an empty listing, not an error:
/// the highlight was deleted between the tray and the click, and "empty" is
/// what there is to say about it.
#[tokio::test]
async fn a_deleted_highlight_answers_empty_rather_than_failing() {
    let server = MockServer::start().await;
    mount_profile(&server, profile_body(7, false, false)).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/highlights/7/highlights_tray/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tray":[{"id":"highlight:100","title":"gone","media_count":2}],"status":"ok"}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"reels_media":[],"status":"ok"}"#),
        )
        .mount(&server)
        .await;

    let client = client(&server);
    let Fetched::Tray(tray) = fetch_tray(&client, "someone", VIEWER).await.unwrap() else {
        panic!("visible");
    };
    let items = items_of_entry(&client, &tray.entries[0]).await.unwrap();
    assert!(items.is_empty());
}
