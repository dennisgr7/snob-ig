//! What the tests of every file here build their reports out of.
//!
//! Gathered in one place rather than left loose in one test module, because
//! every file here has a test module of its own and a fixture private to a
//! sibling is a fixture that gets written twice. `pub(in ...watch)` and no
//! further: they are shapes for tests, and nothing outside this module has any
//! business with them.

use snob_core::Pk;
use snob_core::model::{ListKind, User};
use snob_core::watch::{Basis, ListDiff, Rename};
use snob_store::config::{self, WatchConfig};
use snob_store::store::deliveries;
use url::Url;

use crate::engine::watch::{ListReport, WatchReport};
use crate::watch::webhook::{Webhook, WebhookClient};

use super::delivery::Delivery;

pub(in crate::commands::watch) const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
pub(in crate::commands::watch) const SID: &str = "42%3AAbCdEfGh%3A20";

pub(in crate::commands::watch) fn user(pk: Pk, name: &str) -> User {
    User {
        pk,
        username: name.into(),
        full_name: None,
        is_private: None,
        is_verified: None,
        pfp_url: None,
    }
}

pub(in crate::commands::watch) fn report_with(
    followers: Option<ListReport>,
    renamed: Vec<Rename>,
) -> WatchReport {
    WatchReport {
        account_pk: Pk::new(42),
        username: Some("me".into()),
        is_self: true,
        followers,
        following: None,
        renamed,
    }
}

pub(in crate::commands::watch) fn list(
    basis: Basis,
    diff: ListDiff,
    since: Option<i64>,
) -> ListReport {
    ListReport {
        kind: ListKind::Followers,
        basis,
        since,
        until: 2_000,
        diff,
        total: 10,
    }
}

/// An app and a webhook pointed at the same mock server.
pub(in crate::commands::watch) fn app_posting_to(
    server: &wiremock::MockServer,
) -> (crate::app::App, Delivery) {
    let session = snob_core::session::Session::from_sessionid(
        SID,
        UA,
        snob_core::session::SessionOrigin::Paste,
    )
    .unwrap();
    let client = snob_ig::client::IgClient::new(session, snob_ig::pace::Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());

    let db = snob_store::store::Store::in_memory().unwrap();
    snob_store::store::users::upsert(db.conn(), &user(Pk::new(42), "me")).unwrap();
    snob_store::store::accounts::upsert(db.conn(), Pk::new(42), true).unwrap();

    let app = crate::app::App::for_test(
        client,
        db,
        crate::app::Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        },
    );

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let delivery = Delivery {
        destination: super::delivery::destination_of(&url),
        signed: false,
        client: WebhookClient::new(Webhook {
            url,
            headers: vec![],
            key: None,
        })
        .unwrap(),
        heartbeat: false,
    };
    (app, delivery)
}

/// A `watch.toml` as the tool would read one.
pub(in crate::commands::watch) fn watch_toml(body: &str) -> WatchConfig {
    config::parse(body, std::path::Path::new("watch.toml")).expect("the fixture parses")
}

/// A report queued longer ago than it can be news for, in a store at
/// `paths`. Nothing but a settle can move it: `due` will not hand back an
/// over-age row, and only a failed attempt expires one.
pub(in crate::commands::watch) fn owed_long_ago(
    paths: &snob_store::paths::AppPaths,
    now: i64,
) -> i64 {
    let db = snob_store::store::Store::open(paths).unwrap();
    snob_store::store::users::upsert(db.conn(), &user(Pk::new(42), "me")).unwrap();
    snob_store::store::accounts::upsert(db.conn(), Pk::new(42), true).unwrap();
    deliveries::enqueue(
        db.conn(),
        "run-old",
        Pk::new(42),
        "{}",
        now - deliveries::MAX_AGE_SECS - 1,
        Some("https://receiver.example"),
    )
    .unwrap()
}
