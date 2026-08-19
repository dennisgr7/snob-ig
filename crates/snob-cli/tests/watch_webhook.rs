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

/// The header carries whatever event it is given, so a receiver can route
/// without parsing the body.
///
/// This pins the client's plumbing and nothing else: the name arrives as an
/// argument. That the name *agrees with the body* -- the defect where the
/// header said `watch.changes` over every heartbeat -- is decided by
/// `commands::watch::event_of`, and is tested there.
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

/// A configured header snob also sends replaces snob's, rather than travelling
/// beside it.
///
/// Two rounds of getting this wrong. `RequestBuilder::header` appends, so a
/// configured `Authorization` and the keyring token both went out; building the
/// user's list into a map with `insert` fixed that among the user's own headers
/// and then replayed the map with `header` again, so a configured
/// `Content-Type` still went out twice. A receiver reading the ordinary
/// single-value accessor sees the first, so the override silently did nothing —
/// and a strict one answers 400, which is a refusal rather than a retry.
#[tokio::test]
async fn a_configured_header_replaces_the_one_snob_would_have_sent() {
    let server = MockServer::start().await;
    mount_hook(&server, 200).await;

    client_for(
        &server,
        None,
        vec![(
            "Content-Type".into(),
            "application/json; charset=utf-8".into(),
        )],
    )
    .post(BODY, "watch.changes", "7", 1)
    .await;

    let requests = server.received_requests().await.unwrap();
    let sent: Vec<_> = last(&requests)
        .headers
        .get_all("content-type")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(sent, ["application/json; charset=utf-8"]);
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

/// Every answer the far end can give is worth another try — a 4xx included.
///
/// A 4xx used to come back as `Refused`, which the outbox expires with **zero**
/// retries, and the mark has already moved by then: one 404 threw away the only
/// copy of a set of arrivals and departures, and the line printed said to check
/// the address. And the 4xx a webhook actually gives are mostly transient — n8n
/// answers 404 for a workflow that is not currently registered, a reverse proxy
/// answers 403 while it reloads, an expired token answers 401. The attempt and
/// age bounds are what stop the retrying, not a guess about the status.
#[tokio::test]
async fn every_answer_from_the_far_end_is_worth_another_try() {
    for status in [500, 502, 503, 408, 429, 400, 401, 403, 404, 410, 422] {
        let server = MockServer::start().await;
        mount_hook(&server, status).await;

        let outcome = client_for(&server, None, vec![])
            .post(BODY, "watch.changes", "7", 1)
            .await;
        assert!(
            matches!(outcome, Attempt::Failed { .. }),
            "{status} was not queued for another try: {outcome:?}"
        );
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
    // A moment the report is still young at. The queue refuses to hand back a
    // report older than `MAX_AGE_SECS` — news about last Tuesday is not news —
    // so a synthetic timestamp from 1970 would simply never be due.
    let queued_at = 1_000_000;
    let id = deliveries::enqueue(app.db().conn(), "run-1", 42, BODY, queued_at, None).unwrap();

    // First attempt: the receiver is down.
    let down = MockServer::start().await;
    mount_hook(&down, 503).await;
    let outcome = client_for(&down, None, vec![])
        .post(BODY, "watch.changes", &id.to_string(), 1)
        .await;
    assert!(matches!(outcome, Attempt::Failed { .. }));
    deliveries::failed(app.db().conn(), id, Some(503), "busy", false, queued_at).unwrap();

    // Still owed, and the bytes are the ones that were signed.
    let owed = deliveries::due(
        app.db().conn(),
        queued_at + 3_600,
        10,
        "https://receiver.example",
    )
    .unwrap();
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
    deliveries::delivered(app.db().conn(), id, 200, queued_at + 3_600).unwrap();

    assert_eq!(deliveries::pending(app.db().conn()).unwrap(), 0);
    let requests = up.received_requests().await.unwrap();
    assert_eq!(std::str::from_utf8(&last(&requests).body).unwrap(), BODY);
    assert_eq!(last(&requests).headers.get("x-snob-attempt").unwrap(), "2");
}

/// The preflight really posts, carrying everything a report would carry.
///
/// A parsed URL says nothing about whether the host resolves, the certificate
/// verifies, the path is registered or the token is the one the receiver wants
/// — and every one of those turns into a queued report and a retry schedule
/// hours later, found from `status` if anybody looks. So `snob watch check`
/// uses the address rather than inspecting it, with the configured headers and
/// the configured signature: a preflight that skipped either would be checking
/// a request nobody makes.
///
/// It is announced as `watch.preflight` so a receiver can branch on it the way
/// it branches on the rest, and nothing about it is queued — there is no report
/// here to retry or deduplicate.
#[tokio::test]
async fn the_preflight_posts_a_signed_message_and_reports_the_answer() {
    use snob_cli::engine::check::{Verdict, What};

    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&receiver)
        .await;

    let url = Url::parse(&format!("{}/hook", receiver.uri())).unwrap();
    let key = Secret::from("shared-secret".to_string());
    let client = WebhookClient::new(Webhook {
        url,
        headers: vec![("X-Api-Key".to_string(), "team".to_string())],
        key: Some(key.clone()),
    })
    .unwrap();

    let body = r#"{"event":"watch.preflight"}"#;
    let checked = snob_cli::engine::check::webhook_of(
        &client,
        "https://receiver.example".to_string(),
        true,
        "preflight-1",
        body,
    )
    .await;

    assert_eq!(checked.verdict, Verdict::Ok, "{:?}", checked.problem);
    assert!(matches!(
        checked.what,
        What::Webhook {
            status: Some(204),
            signed: true,
            ..
        }
    ));

    let sent = &receiver.received_requests().await.unwrap()[0];
    let header = |name: &str| {
        sent.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(header("X-Snob-Event"), "watch.preflight");
    assert_eq!(header("X-Api-Key"), "team", "the configured headers travel");
    assert_eq!(
        header("X-Snob-Signature"),
        sign::sign(std::str::from_utf8(&sent.body).unwrap(), &key),
        "and so does the signature, over the bytes that were sent"
    );
}

/// A receiver that is not there is a failure worth a red line, not a warning.
#[tokio::test]
async fn a_preflight_that_cannot_be_delivered_fails_the_check() {
    use snob_cli::engine::check::Verdict;

    // Port 1, which nothing binds and which is outside the ephemeral range.
    //
    // This used to start a `MockServer` and drop it, on the reasoning that the
    // port was then closed. It is closed for about as long as it takes another
    // test in the same binary to be given it: the suite runs in parallel, every
    // other test here starts a server on an ephemeral port, and when one landed
    // on this one the connection succeeded and the verdict came back `Ok`. A
    // test that fails once a week teaches people to re-run it.
    let address = "http://127.0.0.1:1".to_string();

    let client = WebhookClient::new(Webhook {
        url: Url::parse(&format!("{address}/hook")).unwrap(),
        headers: vec![],
        key: None,
    })
    .unwrap();

    let checked = snob_cli::engine::check::webhook_of(
        &client,
        address,
        false,
        "preflight-2",
        r#"{"event":"watch.preflight"}"#,
    )
    .await;

    assert_eq!(checked.verdict, Verdict::Failed);
    assert!(checked.problem.is_some(), "it has to say what went wrong");
}
