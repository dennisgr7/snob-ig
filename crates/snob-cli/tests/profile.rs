//! `snob profile` against a mock server: what is asked, what is not, and what
//! the answer says about each.
//!
//! Every case here is one the August 2026 capture showed a real account in —
//! a private account the viewer does not follow, whose tray comes back
//! empty; an account with nobody in common, where there is nothing to page;
//! one with more mutuals than a page holds; and the viewer's own, where half
//! the questions have no answer.

mod common;

use common::{SID, UA};
use snob_cli::commands::profile::{Visibility, fetch};
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

/// A profile answer with the fields `profile` reads, and room for the few
/// that vary between cases.
fn profile_body(id: u64, private: bool, followed: bool, mutual: u64, names: &[&str]) -> String {
    let edges: Vec<String> = names
        .iter()
        .map(|n| format!(r#"{{"node":{{"username":"{n}"}}}}"#))
        .collect();
    format!(
        r#"{{"data":{{"user":{{"id":"{id}","username":"someone","full_name":"Some One",
        "biography":"hi","external_url":"","is_private":{private},"is_verified":false,
        "followed_by_viewer":{followed},"follows_viewer":true,"requested_by_viewer":false,
        "has_requested_viewer":false,"edge_followed_by":{{"count":244}},
        "edge_follow":{{"count":319}},"highlight_reel_count":1,
        "edge_mutual_followed_by":{{"count":{mutual},"edges":[{edges}]}},
        "is_business_account":true,"category_name":"",
        "edge_owner_to_timeline_media":{{"count":6}}}}}},"status":"ok"}}"#,
        edges = edges.join(",")
    )
}

async fn mount_profile(server: &MockServer, body: String) {
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

async fn mount_tray(server: &MockServer, pk: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/highlights/{pk}/highlights_tray/")))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tray":[{"id":"highlight:1","title":"trip","media_count":5}],"status":"ok"}"#,
        ))
        .mount(server)
        .await;
}

async fn mount_stories(server: &MockServer, items: usize) {
    let items: Vec<String> = (0..items)
        .map(|i| format!(r#"{{"pk":"{i}","media_type":1,"taken_at":1752774358}}"#))
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"reels_media":[{{"items":[{}]}}],"status":"ok"}}"#,
            items.join(",")
        )))
        .mount(server)
        .await;
}

fn paths(requests: &[wiremock::Request]) -> Vec<String> {
    requests.iter().map(|r| r.url.path().to_string()).collect()
}

/// A private account the viewer does not follow serves neither its tray nor
/// its stories, so neither is asked for — and the answer says they are kept
/// back rather than that there are none.
#[tokio::test]
async fn a_private_account_you_do_not_follow_is_not_asked_for_what_it_hides() {
    let server = MockServer::start().await;
    mount_profile(&server, profile_body(7, true, false, 2, &["ana", "luis"])).await;
    // One page of mutuals, with no cursor: the list ends here.
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/7/mutual_followers/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"users":[{"pk":1,"username":"ana"},{"pk":2,"username":"luis"}],"status":"ok"}"#,
        ))
        .mount(&server)
        .await;

    let profile = fetch(&client(&server), "someone", VIEWER).await.unwrap();

    assert_eq!(profile.highlights, Visibility::Hidden);
    assert_eq!(profile.stories_up, Visibility::Hidden);
    assert!(profile.is_private);
    let relation = profile.relation.expect("somebody else's account");
    assert!(!relation.you_follow && relation.follows_you);
    let mutual = profile.mutual.expect("somebody else's account");
    assert_eq!(mutual.count, 2);
    assert_eq!(mutual.people.len(), 2);
    assert!(mutual.complete);
    // Empty strings are absences, not blank lines.
    assert_eq!(profile.external_url, None);
    assert_eq!(profile.category, None);

    let asked = paths(&server.received_requests().await.unwrap());
    assert_eq!(
        asked,
        [
            "/api/v1/users/web_profile_info/",
            "/api/v1/friendships/7/mutual_followers/"
        ],
        "two requests, and neither reel"
    );
}

/// Nobody in common means nothing to page, and the page's count is zero
/// rather than a request spent to learn it.
#[tokio::test]
async fn an_account_with_nobody_in_common_costs_no_mutual_request() {
    let server = MockServer::start().await;
    mount_profile(&server, profile_body(7, false, false, 0, &[])).await;
    mount_tray(&server, 7).await;
    mount_stories(&server, 0).await;

    let profile = fetch(&client(&server), "someone", VIEWER).await.unwrap();

    let mutual = profile.mutual.expect("somebody else's account");
    assert_eq!(mutual.count, 0);
    assert!(mutual.people.is_empty() && mutual.complete);
    assert_eq!(profile.stories_up, Visibility::Shown(0));
    match &profile.highlights {
        Visibility::Shown(list) => {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].title, "trip");
            assert_eq!(list[0].items, Some(5));
        }
        Visibility::Hidden => panic!("a public account hides nothing"),
    }

    let asked = paths(&server.received_requests().await.unwrap());
    assert_eq!(asked.len(), 3, "{asked:?}");
    assert!(
        !asked.iter().any(|p| p.contains("mutual_followers")),
        "{asked:?}"
    );
}

/// The mutual list is walked page by page, with the cursor the last page
/// handed over, until a page comes with none.
#[tokio::test]
async fn the_mutual_list_follows_the_cursor_to_the_end() {
    let server = MockServer::start().await;
    mount_profile(&server, profile_body(7, false, true, 14, &["a", "b", "c"])).await;
    mount_tray(&server, 7).await;
    mount_stories(&server, 2).await;

    let first: Vec<String> = (0..12)
        .map(|i| format!(r#"{{"pk":{i},"username":"u{i}"}}"#))
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/7/mutual_followers/"))
        .and(query_param("page_size", "12"))
        .and(wiremock::matchers::query_param_is_missing("max_id"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"users":[{}],"next_max_id":"12","big_list":true,"status":"ok"}}"#,
            first.join(",")
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/friendships/7/mutual_followers/"))
        .and(query_param("max_id", "12"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"users":[{"pk":12,"username":"u12"},{"pk":13,"username":"u13"}],"status":"ok"}"#,
        ))
        .mount(&server)
        .await;

    let profile = fetch(&client(&server), "someone", VIEWER).await.unwrap();

    let mutual = profile.mutual.expect("somebody else's account");
    assert_eq!(mutual.count, 14);
    assert_eq!(mutual.people.len(), 14);
    assert!(mutual.complete);
    assert_eq!(mutual.people[0].username, "u0");
    assert_eq!(mutual.people[13].username, "u13");
    assert_eq!(profile.stories_up, Visibility::Shown(2));

    let asked = paths(&server.received_requests().await.unwrap());
    assert_eq!(
        asked
            .iter()
            .filter(|p| p.contains("mutual_followers"))
            .count(),
        2
    );
}

/// The viewer's own account: no relation to state and no mutuals to list,
/// and the rest is read like anybody else's.
#[tokio::test]
async fn your_own_account_has_no_relation_and_no_mutuals() {
    let server = MockServer::start().await;
    mount_profile(&server, profile_body(VIEWER.get(), true, false, 0, &[])).await;
    mount_tray(&server, VIEWER.get()).await;
    mount_stories(&server, 1).await;

    let profile = fetch(&client(&server), "someone", VIEWER).await.unwrap();

    assert!(profile.relation.is_none());
    assert!(profile.mutual.is_none());
    // Private, and yours: the reels are yours to see.
    assert!(matches!(profile.highlights, Visibility::Shown(ref l) if l.len() == 1));
    assert_eq!(profile.stories_up, Visibility::Shown(1));
    assert_eq!(profile.followers, Some(244));
    assert_eq!(profile.posts, Some(6));

    let asked = paths(&server.received_requests().await.unwrap());
    assert_eq!(asked.len(), 3, "{asked:?}");
}
