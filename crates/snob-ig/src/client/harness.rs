//! What the tests in this module's files build their clients out of.
//!
//! These were one copy at the bottom of the file when all of this was one file,
//! and the tests that need them are now spread over five of them. They are here
//! rather than written out five times. Anything only one concern's tests reach
//! for stayed with those tests.

use std::sync::Arc;

use snob_core::budget::{RateBudget, RateBudgetError};
use snob_core::session::{Session, SessionOrigin};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::IgClient;
use crate::graphql;

pub(super) const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
pub(super) const SID: &str = "42%3AAbCdEfGh%3A20";

pub(super) async fn client(server: &MockServer) -> IgClient {
    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    IgClient::new(session, crate::pace::Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap())
}

/// A budget that remembers what it was told to write down.
///
/// The rule that a push-back puts the account in cooldown had no observer
/// anywhere in the workspace: every test that drove a throttling body
/// through a real client hung `Pacer::unlimited()` off it, whose
/// `start_cooldown` answers `Ok(0)` and forgets. The whole recording half of
/// [`IgClient::classify_and_record`] could be deleted and the suite stayed
/// green — on the one rule that decides whether the next run walks back into
/// an account Instagram has just flagged.
#[derive(Default)]
pub(super) struct Recording {
    started: std::sync::Mutex<Vec<(String, std::time::Duration)>>,
}

impl Recording {
    pub(super) fn calls(&self) -> Vec<(String, std::time::Duration)> {
        self.started.lock().unwrap().clone()
    }
}

impl snob_core::budget::RateBudget for Recording {
    fn reserve(&self) -> Result<std::time::Duration, RateBudgetError> {
        Ok(std::time::Duration::ZERO)
    }
    fn reserve_write(&self) -> Result<std::time::Duration, RateBudgetError> {
        self.reserve()
    }
    fn cooldown(&self) -> Result<Option<i64>, RateBudgetError> {
        Ok(None)
    }
    fn start_cooldown(
        &self,
        reason: &str,
        minimum: std::time::Duration,
    ) -> Result<i64, RateBudgetError> {
        self.started
            .lock()
            .unwrap()
            .push((reason.to_string(), minimum));
        Ok(0)
    }
}

/// A client whose budget can be asked afterwards what it was told.
pub(super) fn watching(base: &str) -> (IgClient, Arc<Recording>) {
    watching_as(base, false)
}

/// The same, with the option of a session that can write. Kept as one
/// helper so the two paths are driven through identical wiring and any
/// difference in what gets recorded is the code's rather than the test's.
pub(super) fn watching_as(base: &str, can_write: bool) -> (IgClient, Arc<Recording>) {
    let budget = Arc::new(Recording::default());
    let mut session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    if can_write {
        session.csrftoken = Some("TOKEN".into());
    }
    let client = IgClient::new(
        session,
        crate::pace::Pacer::new(Arc::clone(&budget) as Arc<dyn RateBudget>),
    )
    .unwrap()
    .with_base_url(Url::parse(base).unwrap());
    (client, budget)
}

pub(super) async fn answering(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    server
}

/// A session with a CSRF token, which is what `login --browser` produces
/// and what the write path requires.
pub(super) async fn writer(server: &MockServer) -> IgClient {
    let mut session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    session.csrftoken = Some("TOKEN".into());
    IgClient::new(session, crate::pace::Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap())
}

/// A page that hands out both tokens, which is what a logged-in one does.
pub(super) const LOGGED_IN_PAGE: &str = r#"<html><script>
    {"define":[["DTSGInitData",[],{"token":"DTSG-TOKEN"},258],
               ["LSD",[],{"token":"LSD-TOKEN"},323]]}
    </script></html>"#;

/// A cache that already knows the ids.
///
/// **Every write test uses this, and that is a decision worth naming.**
/// Discovery walks `static.cdninstagram.com`, and the host is fixed in
/// `graphql::bundles_in` rather than taken from the document — which is the
/// property that makes walking a page's URLs safe, and which therefore
/// cannot be pointed at a mock server. Weakening it so a test could reach it
/// would be trading the guard for the coverage. The walk's two halves are
/// pure functions and are tested directly in `graphql`; what is exercised
/// here is everything around them.
pub(super) struct Known;

impl graphql::DocIds for Known {
    fn get(&self, name: &str) -> Option<String> {
        Some(match name {
            "usePolarisFollowMutation" => "26508036048874888".into(),
            _ => "27789106940691111".into(),
        })
    }
    fn put(&self, _: &str, _: &str) {}
}

/// A server that answers the page and the mutation, which is the pair every
/// write needs.
pub(super) async fn instagram_that_takes_a_write(body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    server
}

/// What the mutation answers.
pub(super) const FOLLOWED: &str = r#"{"result":"following","status":"ok"}"#;
