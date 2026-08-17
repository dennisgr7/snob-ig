//! What actually reaches the address the user chose.
//!
//! Two servers: one pretending to be Instagram, one pretending to be the
//! receiver. That separation is the point of several of these — a request that
//! went to the wrong one would show up as a count on a server that was not
//! meant to get it.

use std::sync::Arc;

use snob_core::secret::Secret;
use snob_core::session::{Session, SessionOrigin};
use snob_core::store::rate_budget::UnlimitedRateBudget;
use snob_core::store::{Store, deliveries};
use snob_core::watch::sign;
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use snob_cli::app::{App, Viewer};
use snob_cli::watch::webhook::{Attempt, Webhook, WebhookClient};

mod common;
use common::{SID, UA};

fn open_db(root: &std::path::Path) -> Store {
    Store::open_at(&root.join("test.db")).unwrap()
}

fn app(server: &MockServer, db: Store) -> App {
    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::new(Arc::new(UnlimitedRateBudget)))
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

fn client_for(
    server: &MockServer,
    key: Option<&str>,
    headers: Vec<(String, String)>,
) -> WebhookClient {
    WebhookClient::new(Webhook {
        url: Url::parse(&format!("{}/hook", server.uri())).unwrap(),
        headers,
        key: key.map(|k| Secret::from(k.to_string())),
    })
    .unwrap()
}

async fn mount_hook(server: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

fn last(server: &[Request]) -> &Request {
    server.last().expect("something was posted")
}

const BODY: &str = r#"{"schema":1,"event":"watch.changes"}"#;

#[tokio::test]
async fn a_report_that_is_accepted_is_delivered() {
    let server = MockServer::start().await;
    mount_hook(&server, 200).await;

    let client = client_for(&server, None, vec![]);
    assert_eq!(
        client.post(BODY, "watch.changes", "7", 1).await,
        Attempt::Delivered { status: 200 }
    );
}

/// The body is posted verbatim, not re-rendered. The signature covers bytes, so
/// a second rendering that spaced the JSON differently would be rejected after
/// the first attempt was accepted.
#[tokio::test]
async fn the_body_arrives_byte_for_byte() {
    let server = MockServer::start().await;
    mount_hook(&server, 200).await;

    client_for(&server, None, vec![])
        .post(BODY, "watch.changes", "7", 1)
        .await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(std::str::from_utf8(&last(&requests).body).unwrap(), BODY);
    assert_eq!(
        last(&requests).headers.get("content-type").unwrap(),
        "application/json"
    );
}

/// The receiver has to be able to verify what it got, so the test verifies it
/// the way a receiver would: recompute the HMAC over the bytes that arrived.
#[tokio::test]
async fn the_signature_checks_out_against_the_bytes_that_arrived() {
    let server = MockServer::start().await;
    mount_hook(&server, 200).await;

    client_for(&server, Some("a shared secret"), vec![])
        .post(BODY, "watch.changes", "7", 1)
        .await;

    let requests = server.received_requests().await.unwrap();
    let sent = last(&requests);
    let arrived = std::str::from_utf8(&sent.body).unwrap();

    assert_eq!(
        sent.headers.get("x-snob-signature").unwrap(),
        &sign::sign(arrived, &Secret::from("a shared secret".to_string()))
    );
}

#[tokio::test]
async fn without_a_secret_nothing_is_signed() {
    let server = MockServer::start().await;
    mount_hook(&server, 200).await;

    client_for(&server, None, vec![])
        .post(BODY, "watch.changes", "7", 1)
        .await;

    let requests = server.received_requests().await.unwrap();
    assert!(last(&requests).headers.get("x-snob-signature").is_none());
}

/// The delivery id is what a receiver deduplicates on, and it has to be told
/// which attempt this is — the queue is at-least-once by design.
#[tokio::test]
async fn the_receiver_is_told_what_this_is_and_which_attempt() {
    let server = MockServer::start().await;
    mount_hook(&server, 200).await;

    client_for(&server, None, vec![])
        .post(BODY, "watch.changes", "41", 3)
        .await;

    let requests = server.received_requests().await.unwrap();
    let sent = last(&requests);
    assert_eq!(sent.headers.get("x-snob-delivery").unwrap(), "41");
    assert_eq!(sent.headers.get("x-snob-attempt").unwrap(), "3");
    assert_eq!(sent.headers.get("x-snob-event").unwrap(), "watch.changes");
}

/// The header exists so a receiver can route without parsing the body, which
/// means it has to agree with the body. It said `watch.changes` over every
/// heartbeat, so exactly the receiver the header is for would have treated
/// every one of them as a report of changes.
#[tokio::test]
async fn a_heartbeat_says_so_in_the_header_too() {
    let server = MockServer::start().await;
    mount_hook(&server, 200).await;

    client_for(&server, None, vec![])
        .post(
            r#"{"schema":1,"event":"watch.heartbeat"}"#,
            "watch.heartbeat",
            "7",
            1,
        )
        .await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        last(&requests).headers.get("x-snob-event").unwrap(),
        "watch.heartbeat"
    );
}

#[tokio::test]
async fn a_configured_header_is_sent() {
    let server = MockServer::start().await;
    mount_hook(&server, 200).await;

    client_for(
        &server,
        None,
        vec![("Authorization".into(), "Bearer secret".into())],
    )
    .post(BODY, "watch.changes", "7", 1)
    .await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        last(&requests).headers.get("authorization").unwrap(),
        "Bearer secret"
    );
}

/// The guard the whole module is arranged around: this client cannot carry the
/// session, and it does not claim to be a browser at somebody else's server.
#[tokio::test]
async fn the_session_never_reaches_the_webhook() {
    let server = MockServer::start().await;
    mount_hook(&server, 200).await;

    client_for(&server, None, vec![])
        .post(BODY, "watch.changes", "7", 1)
        .await;

    let requests = server.received_requests().await.unwrap();
    let sent = last(&requests);
    assert!(sent.headers.get("cookie").is_none());
    assert!(
        !sent
            .headers
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Mozilla"),
        "the browser User-Agent belongs to the Instagram session, not here"
    );
}

/// A server that was restarting comes back. A token that is wrong does not.
#[tokio::test]
async fn a_server_error_is_worth_retrying_and_a_refusal_is_not() {
    for (status, retryable) in [
        (500, true),
        (502, true),
        (503, true),
        (408, true),
        (429, true),
        (401, false),
        (403, false),
        (404, false),
        (422, false),
    ] {
        let server = MockServer::start().await;
        mount_hook(&server, status).await;

        let outcome = client_for(&server, None, vec![])
            .post(BODY, "watch.changes", "7", 1)
            .await;
        match (&outcome, retryable) {
            (Attempt::Failed { .. }, true) | (Attempt::Refused { .. }, false) => {}
            _ => panic!(
                "{status} should {}be retried: {outcome:?}",
                if retryable { "" } else { "not " }
            ),
        }
    }
}

/// A 3xx is how a body carrying account names ends up at a host nobody named.
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let elsewhere = MockServer::start().await;
    mount_hook(&elsewhere, 200).await;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/hook", elsewhere.uri())),
        )
        .mount(&server)
        .await;

    client_for(&server, None, vec![])
        .post(BODY, "watch.changes", "7", 1)
        .await;

    assert!(
        elsewhere.received_requests().await.unwrap().is_empty(),
        "the report must not travel to wherever a redirect points"
    );
}

/// An address that is not listening is a failure that comes back, not one to
/// give up on.
#[tokio::test]
async fn an_unreachable_address_is_a_temporary_failure() {
    // Port 1 on loopback: privileged, so nothing of the test's own can be
    // listening, and the connection is refused at once rather than timing out.
    // Dropping a `MockServer` and reusing its address would be the obvious way
    // to write this and is not reliable — the port can be taken again between
    // the drop and the request.
    let client = WebhookClient::new(Webhook {
        url: Url::parse("http://127.0.0.1:1/hook").unwrap(),
        headers: vec![],
        key: None,
    })
    .unwrap();

    assert!(matches!(
        client.post(BODY, "watch.changes", "7", 1).await,
        Attempt::Failed { status: None, .. }
    ));
}

/// End to end over a real database: a failed send leaves the report queued, and
/// a later run sends the same bytes again.
#[tokio::test]
async fn a_failed_report_is_queued_and_the_same_bytes_go_out_next_time() {
    let tmp = tempfile::tempdir().unwrap();
    let ig = MockServer::start().await;
    let db = open_db(tmp.path());
    let app = app(&ig, db);

    snob_core::store::users::ensure(app.db().conn(), 42).unwrap();
    snob_core::store::accounts::upsert(app.db().conn(), 42, true).unwrap();
    let id = deliveries::enqueue(app.db().conn(), "run-1", 42, BODY, 1_000).unwrap();

    // First attempt: the receiver is down.
    let down = MockServer::start().await;
    mount_hook(&down, 503).await;
    let outcome = client_for(&down, None, vec![])
        .post(BODY, "watch.changes", &id.to_string(), 1)
        .await;
    assert!(matches!(outcome, Attempt::Failed { .. }));
    deliveries::failed(app.db().conn(), id, Some(503), "busy", false, 1_000).unwrap();

    // Still owed, and the bytes are the ones that were signed.
    let owed = deliveries::due(app.db().conn(), 999_999, 10).unwrap();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].body, BODY);

    // Second attempt: it is back.
    let up = MockServer::start().await;
    mount_hook(&up, 200).await;
    let outcome = client_for(&up, None, vec![])
        .post(
            &owed[0].body,
            "watch.changes",
            &id.to_string(),
            owed[0].attempts + 1,
        )
        .await;
    assert_eq!(outcome, Attempt::Delivered { status: 200 });
    deliveries::delivered(app.db().conn(), id, 200, 2_000).unwrap();

    assert_eq!(deliveries::pending(app.db().conn()).unwrap(), 0);
    let requests = up.received_requests().await.unwrap();
    assert_eq!(std::str::from_utf8(&last(&requests).body).unwrap(), BODY);
    assert_eq!(last(&requests).headers.get("x-snob-attempt").unwrap(), "2");
}
