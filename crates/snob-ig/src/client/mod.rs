//! HTTP client against Instagram's web API.
//!
//! It uses the `www.instagram.com` endpoints rather than the `i.instagram.com`
//! ones, to stay consistent with a session that originated in a desktop
//! browser. That consistency between cookie, User-Agent and endpoint is what
//! Instagram evaluates, and breaking it is what produces `useragent mismatch`.
//!
//! One `IgClient`, six files, because the type does six separate things. What
//! is here is the client itself: how it is built, what it holds, where it is
//! pointed, and the two steps every answer goes through whichever half of the
//! API produced it -- [`IgClient::decode`] and
//! [`IgClient::classify_and_record`]. The other five add to the same `impl`,
//! which is what lets each of them be read on its own.
//!
//! - [`transport`] -- how a request goes out and how the answer comes back:
//!   the origin rule, the redirect loop that pays for every hop it follows,
//!   the ceilings on what will be read, and the races against the cancel
//!   token.
//! - [`headers`] -- what a request says about itself. [`headers::Surface`] is
//!   the difference between the app talking to the API and the browser
//!   navigating to a page, and it decides most of the set.
//! - [`read`] -- the endpoints this tool reads, one method per URL.
//! - [`write`] -- the follow and the unfollow, and nothing else. One file, so
//!   that the rule about which function may send a method other than GET can
//!   be checked by opening it.
//! - [`media`] -- the CDN half: where a picture is allowed to come from, and
//!   the download that carries nothing identifying.

mod headers;
mod media;
mod read;
mod transport;
mod write;

#[cfg(test)]
mod harness;

pub use self::read::Direction;

use std::sync::{Mutex, OnceLock};

use serde::de::DeserializeOwned;
use snob_core::session::Session;
use url::Url;

use crate::BASE_URL;
use crate::client_hints::ClientHints;
use crate::error::{IgError, classify, declares_failure};
use crate::pace::Pacer;

use self::media::cdn_policy;
use self::transport::{Answer, api_policy, build_client, same_origin};

pub struct IgClient {
    /// Talks to Instagram. Carries the session, and follows a redirect only
    /// while it stays on the origin it started from.
    api: reqwest::Client,
    /// Talks to the CDN. Carries nothing that identifies the account, and
    /// every hop has to be somewhere pictures come from.
    ///
    /// Two clients rather than one because the two have opposite rules: the
    /// API request must not leave instagram.com, and the asset request has to
    /// be allowed to move between CDN hosts. One client can only have one
    /// redirect policy, so sharing it meant the looser of the two governed the
    /// requests carrying the credentials.
    ///
    /// Built on first use, which is `snob pfp` and nothing else. A
    /// `reqwest::Client` is a connection pool and a TLS configuration — the
    /// platform trust store is read to assemble one — and every other command
    /// paid for that on the startup path to never send a request through it.
    cdn: OnceLock<reqwest::Client>,
    /// Sends the two writes, and follows nothing.
    ///
    /// A third client, for a third rule. [`api_policy`] allows up to three
    /// same-origin hops, which is right for a read and wrong for a write:
    /// following a redirect on a POST means asking Instagram to do the thing
    /// again, and "again" is a follow or an unfollow that nobody confirmed.
    /// reqwest converts a 307 or 308 into a repeat of the same method and body,
    /// so this is not theoretical — it is what would happen.
    ///
    /// Built on first use like [`Self::cdn`], and for the same reason: every
    /// command that never writes would otherwise assemble a TLS configuration
    /// it has no use for.
    writer: OnceLock<reqwest::Client>,
    base: Url,
    session: Session,
    /// Reserving budget lives here rather than in each caller, so a request
    /// that is never paid for cannot be written.
    pacer: Pacer,
    hints: ClientHints,
    /// Instagram's session-continuity token. See [`IgClient::claim`].
    claim: Mutex<String>,
}

/// What a browser sends before the server has told it anything.
const INITIAL_CLAIM: &str = "0";

/// Where every client built after this call points, in a testing build.
///
/// **Compiled out of a release build entirely**, feature and all: a shipped
/// binary has neither this function nor the flag that calls it, so there is no
/// way to point it at another server and nothing to disable. That is what the
/// whole Cargo feature buys over a hidden flag, and it is the reason it is a
/// feature.
///
/// A process-global rather than an argument, which is normally the wrong answer
/// and is the right one here. Three separate places build a client — `App`,
/// `whoami`, and `login::validate`, which is in this crate and takes no path
/// from the binary at all — so an argument would have to be threaded through
/// five signatures that exist in the release build, to carry a value that never
/// exists in it. It is set once from `main` before any client exists, and never
/// again: [`OnceLock::set`] returns the value back on a second call rather than
/// replacing it, so a run cannot be redirected halfway through.
///
/// It sets the **base URL**, not a "skip the pace" switch, which is the
/// difference that matters. [`IgClient::is_live`] answers by address, so a
/// client pointed here is genuinely not Instagram and turning the pace off is
/// telling the truth. The failure that shape avoids is the one a loopback-only
/// escape hatch would have created: a proxy on `127.0.0.1` forwarding to
/// Instagram is a test server by address and Instagram by content, and the walk
/// it produces is a real account read with no waits between pages.
///
/// The binary refuses `--ig-base-url` unless `--sandbox-root` is given too, so
/// the session a redirected client carries comes out of a store inside that
/// root. The stored session of the person running it is not reachable from
/// here.
#[cfg(feature = "testing")]
static SANDBOX_BASE: std::sync::OnceLock<Url> = std::sync::OnceLock::new();

/// Points every client built from now on somewhere other than Instagram.
///
/// See [`SANDBOX_BASE`]. `Err` carries the base already set, on a second call.
#[cfg(feature = "testing")]
pub fn point_every_client_at(base: Url) -> Result<(), Url> {
    SANDBOX_BASE.set(base)
}

impl IgClient {
    pub fn new(session: Session, pacer: Pacer) -> Result<Self, IgError> {
        // Read here rather than at each call site, because `login::validate`
        // builds a client inside this crate and never sees the binary's
        // arguments. In a release build this line does not exist.
        #[cfg(feature = "testing")]
        if let Some(base) = SANDBOX_BASE.get() {
            return Self::pointed_at(session, pacer, base.clone());
        }
        Self::pointed_at(session, pacer, Url::parse(BASE_URL)?)
    }

    fn pointed_at(session: Session, pacer: Pacer, base: Url) -> Result<Self, IgError> {
        Ok(Self {
            hints: ClientHints::from_user_agent(&session.user_agent),
            api: build_client(&session.user_agent, api_policy())?,
            cdn: OnceLock::new(),
            writer: OnceLock::new(),
            base,
            session,
            pacer,
            claim: Mutex::new(INITIAL_CLAIM.to_string()),
        })
    }

    /// The CDN client, built the first time a picture is downloaded.
    ///
    /// Everything the policy needs is already a field, so nothing has to be
    /// captured at construction time to make this work — which is what lets the
    /// two-policy boundary stay exactly as it was while only `pfp` pays for it.
    pub(in crate::client) fn cdn(&self) -> Result<&reqwest::Client, IgError> {
        if let Some(cdn) = self.cdn.get() {
            return Ok(cdn);
        }
        let built = build_client(&self.session.user_agent, cdn_policy(self.base.clone()))?;
        Ok(self.cdn.get_or_init(|| built))
    }

    /// The write client, built the first time a follow or an unfollow is sent.
    ///
    /// See [`Self::writer`] for why it does not share the read client.
    pub(in crate::client) fn writer(&self) -> Result<&reqwest::Client, IgError> {
        if let Some(writer) = self.writer.get() {
            return Ok(writer);
        }
        let built = build_client(&self.session.user_agent, reqwest::redirect::Policy::none())?;
        Ok(self.writer.get_or_init(|| built))
    }

    /// Reads Instagram's answer and, if it was a push-back, records the
    /// cooldown before handing the error on.
    ///
    /// It lives here for the same reason the budget does: every caller needed
    /// the same reaction and only two of them remembered it, so a 429 outside a
    /// walk left no mark at all and the next run knocked on the same door
    /// straight away. Making the request is what earns the cooldown, so the
    /// place that makes requests is the place that records it.
    pub(in crate::client) fn classify_and_record(&self, status: u16, body: &str) -> IgError {
        let error = classify(status, body);
        if let Some((reason, minimum)) = crate::error::cooldown_for(&error) {
            // A cooldown that cannot be written is not worth losing the real
            // error over: the caller still gets told what Instagram said.
            if let Err(e) = self.pacer.start_cooldown(reason, minimum) {
                tracing::warn!(error = %e, "could not record the cooldown");
            }
        }
        error
    }

    /// Points the client at a different server. Tests only.
    ///
    /// Rebuilds rather than assigns: both redirect policies are decided from
    /// the base URL when the client is made, so moving the field alone would
    /// leave them judging every hop against the wrong server.
    ///
    /// **This is also what turns the pace off**, through [`IgClient::is_live`],
    /// so a consumer of this crate that pointed a client at a proxy would walk
    /// Instagram with no waits between pages. `snob-cli` is the only consumer,
    /// and it only does this in tests.
    #[doc(hidden)]
    #[must_use]
    pub fn with_base_url(self, base: Url) -> Self {
        Self::pointed_at(self.session, self.pacer, base)
            .expect("rebuilding a client that already exists cannot fail")
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn pacer(&self) -> &Pacer {
        &self.pacer
    }

    /// Whether this client is pointed at Instagram itself.
    ///
    /// What decides whether the pace is real: a walk against the live host pays
    /// every wait between pages, and a walk against a mock server pays none.
    ///
    /// Asked of the base URL rather than set by a caller, and that is the whole
    /// point. It used to be `ListWalker::without_sleeping()`, a `#[doc(hidden)]`
    /// method — so "walk Instagram with no waits" was a thing anybody could ask
    /// for, and the rule that a real account is never walked without the
    /// limiter rested on nobody asking. A test server cannot be Instagram, and
    /// Instagram cannot be a test server, so the question answers itself.
    pub(crate) fn is_live(&self) -> bool {
        static LIVE: std::sync::LazyLock<Url> =
            std::sync::LazyLock::new(|| Url::parse(BASE_URL).expect("BASE_URL parses"));
        same_origin(&self.base, &LIVE)
    }

    /// Turns what Instagram said into either the value asked for or an error,
    /// recording a cooldown on the way if the answer earned one.
    ///
    /// Shared by the read and the write path so that "a 200 can still be a
    /// failure" is one rule rather than two. It says so because it is now true:
    /// this comment was written when `post` was split out and `get` was left
    /// re-implementing every line of it, so for a while the rule was two
    /// copies and the doc was the only place they looked like one. It was inline in `get` when `get`
    /// was the only caller.
    pub(in crate::client) fn decode<T: DeserializeOwned>(
        &self,
        answer: &Answer,
    ) -> Result<T, IgError> {
        let body = answer.body.as_str();
        // A 200 can still be an error: Instagram returns `{"status":"fail"}`
        // with a 200 in some cases, and a GraphQL refusal is *always* a 200
        // with the reason in an `errors` array.
        if !answer.is_success() || declares_failure(body) {
            return Err(self.refuse(answer));
        }

        serde_json::from_str(body).map_err(|e| {
            // The same excerpt every other error gets. This one had a copy of
            // its own that took 200 raw characters: unfiltered, though it is
            // printed to a terminal, and with no idea that a body starting with
            // `<` is a captive portal rather than the API — which is exactly
            // what a body that will not parse usually is.
            IgError::Decode(format!(
                "{e} - response: {}",
                crate::error::body_excerpt(body)
            ))
        })
    }

    /// Writes down what a push-back looked like, and changes nothing.
    ///
    /// **This is a measurement, not a mechanism.** Reading `Retry-After` would
    /// make snob the only tool of its class that does, and nobody has
    /// established whether these endpoints send it at all; the honest first
    /// step is to log it on every push-back so that a real run answers the
    /// question. Until it has, inventing behavior on the assumption that the
    /// header arrives is guessing with somebody's account.
    ///
    /// **When it is implemented it is a floor and never a ceiling.** A server
    /// naming thirty seconds must not shorten a local cooldown that is longer:
    /// the cooldown lengths here are about how long an account is left alone
    /// after Instagram has objected, which is a different question from how
    /// soon the endpoint will answer again. Written here because this is where
    /// somebody will come looking when they add it.
    ///
    /// `debug` rather than `warn`: on a run that is going badly this fires
    /// once per push-back, and the user already gets told what happened.
    ///
    /// The value is logged as it arrived. A header value cannot carry a byte
    /// below 0x20 — the HTTP parser refuses one before we ever see it — so
    /// there is nothing here that a terminal would act on, which is the same
    /// argument the `Referer` above rests on.
    /// The one way a push-back leaves the client: noted, then classified.
    ///
    /// One function rather than a pair of calls at each site, and the history
    /// says why. The pair used to be written out by hand -- in `decode`, in
    /// `page`, and in the transport's unreadable-body arm -- in an order
    /// nothing enforced, and before that the noting lived in `get` only,
    /// which left the write path (the one that earns the twelve-hour
    /// cooldown, and the one where a push-back is most worth seeing) as the
    /// single class of request never measured, while AGENTS.md promised every
    /// one of them was. Now "every push-back is measured" is checkable by
    /// reading one function.
    pub(in crate::client) fn refuse(&self, answer: &Answer) -> IgError {
        self.note_push_back(answer);
        self.classify_and_record(answer.status, &answer.body)
    }

    pub(in crate::client) fn note_push_back(&self, answer: &Answer) {
        tracing::debug!(
            status = answer.status,
            retry_after = answer.retry_after.as_deref().unwrap_or("<absent>"),
            load = answer.load.as_deref().unwrap_or("<absent>"),
            "Instagram pushed back"
        );
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::harness::{answering, client, watching};

    /// Every push-back is written down, with the length its own cause earns.
    ///
    /// `cooldown_for` is the table and it is tested on its own, but a table
    /// nobody reads is not a rule. This drives the four causes through a real
    /// request and asks the budget what arrived.
    #[tokio::test]
    async fn every_push_back_puts_the_account_in_cooldown() {
        use crate::error::cooldown_for;

        let cases = [
            (429, "", IgError::RateLimited),
            (
                400,
                r#"{"message":"","spam":true,"status":"fail"}"#,
                IgError::RateLimited,
            ),
            (
                400,
                r#"{"message":"feedback_required","status":"fail"}"#,
                IgError::FeedbackRequired,
            ),
            (
                400,
                r#"{"message":"challenge_required","status":"fail"}"#,
                IgError::Challenge { url: None },
            ),
        ];

        for (status, body, expected) in cases {
            let server = answering(status, body).await;
            let (client, budget) = watching(&server.uri());
            client.validate().await.unwrap_err();

            let (reason, minimum) = cooldown_for(&expected).expect("this cause earns a cooldown");
            assert_eq!(
                budget.calls(),
                vec![(reason.to_string(), minimum)],
                "status {status} with body {body:?} should record one cooldown"
            );
        }
    }

    /// A dead session is not push-back, and must not put the account in
    /// cooldown: logging in again is what fixes it, and a cooldown would refuse
    /// the very command that fixes it.
    #[tokio::test]
    async fn a_dead_session_records_nothing() {
        let server = answering(403, r#"{"message":"login_required","status":"fail"}"#).await;
        let (client, budget) = watching(&server.uri());

        let error = client.validate().await.unwrap_err();
        assert!(matches!(error, IgError::SessionExpired));
        assert!(budget.calls().is_empty());
    }

    /// A 200 whose body will not parse gets the same excerpt as every other
    /// error.
    ///
    /// It had a copy of its own that took 200 raw characters. The filtering and
    /// the "this is an HTML page" recognition both lived in `body_excerpt`,
    /// which only the non-success path called — so an escape sequence arriving
    /// with a 500 was filtered and the identical body arriving with a 200 was
    /// printed to the terminal verbatim. A captive portal answering 200 with a
    /// login page is the ordinary way to reach this.
    #[tokio::test]
    async fn a_body_that_will_not_parse_is_excerpted_like_every_other_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/following/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "<html><body>{esc}[2K{esc}[A Sign in to the network</body></html>",
                esc = '\x1b'
            )))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .validate()
            .await
            .expect_err("that is not the API's JSON");
        let message = error.to_string();

        assert!(!message.contains('\x1b'), "{message:?}");
        assert!(message.contains("an HTML page"), "{message}");
    }

    #[tokio::test]
    async fn an_expired_session_is_recognized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string(r#"{"message":"login_required","status":"fail"}"#),
            )
            .mount(&server)
            .await;

        let error = client(&server).await.validate().await.unwrap_err();
        assert!(matches!(error, IgError::SessionExpired));
    }

    #[tokio::test]
    async fn a_failure_carrying_a_200_is_caught_too() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"message":"","spam":true,"status":"fail"}"#),
            )
            .mount(&server)
            .await;

        let error = client(&server).await.validate().await.unwrap_err();
        assert!(matches!(error, IgError::RateLimited));
    }

    #[tokio::test]
    async fn an_unreadable_response_gives_a_decode_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>oops</html>"))
            .mount(&server)
            .await;

        let error = client(&server).await.validate().await.unwrap_err();
        assert!(matches!(error, IgError::Decode(_)));
    }
}
