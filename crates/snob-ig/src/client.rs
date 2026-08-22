//! HTTP client against Instagram's web API.
//!
//! It uses the `www.instagram.com` endpoints rather than the `i.instagram.com`
//! ones, to stay consistent with a session that originated in a desktop
//! browser. That consistency between cookie, User-Agent and endpoint is what
//! Instagram evaluates, and breaking it is what produces `useragent mismatch`.

use std::sync::{Mutex, OnceLock};

use serde::de::DeserializeOwned;
use snob_core::Pk;
use snob_core::session::Session;
use url::Url;

use crate::client_hints::{self, ClientHints};
use crate::error::{IgError, classify, declares_failure};
use crate::graphql;
use crate::model::{
    FriendshipResult, FriendshipStatus, FriendshipsPage, Identity, Reel, ReelsMedia, SearchUser,
    TopSearch, UserInfo, UserInfoEnvelope, WebProfileInfo, WebProfileInfoEnvelope,
};
use crate::pace::Pacer;
use crate::{BASE_URL, IG_APP_ID};

/// Ceiling on a downloaded asset. A profile picture tops out at 1080x1080 and
/// lands far below this; the cap exists so that a redirect to something else
/// cannot make us read until memory runs out.
const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;

/// Ceiling on an API response.
///
/// A page of fifty accounts is tens of kilobytes; this is orders of magnitude
/// above anything real. It exists because the body is read into memory whole,
/// so without a limit a hostile or broken answer decides how much memory this
/// process uses.
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// How long a single request may take end to end, and how long the connection
/// itself may take to come up. Generous: the walk's own pacing is what keeps
/// requests apart, and these are only here so that nothing waits forever.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How far a redirect chain may go before it is a loop by another name.
const MAX_HOPS: usize = 3;

/// Same scheme, same host, same port. Not "the same host", which is what this
/// used to be in one place and is the reason the rest of this section exists.
fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Whether a URL is somewhere a profile picture actually comes from.
///
/// HTTPS, and one of the two hosts Instagram serves media from. The leading
/// dot the suffix is built with is what stops `evilcdninstagram.com` matching.
///
/// The one exception is the server this client was pointed at, which is how a
/// test serves an asset over plain HTTP from localhost. It is matched on
/// scheme, host **and** port together, and that matters in production rather
/// than in tests: on host alone the exception is live against the real base
/// URL, so a `profile_pic_url` of `http://www.instagram.com:8080/x` was
/// accepted and fetched in the clear.
fn serves_pictures(base: &Url, url: &Url) -> bool {
    const CDN_HOSTS: [&str; 2] = ["cdninstagram.com", "fbcdn.net"];

    if same_origin(base, url) {
        return true;
    }
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    CDN_HOSTS
        .iter()
        .any(|cdn| host == *cdn || host.ends_with(&format!(".{cdn}")))
}

/// Redirects for the API: none, because following one is a request and a
/// request has to be paid for.
///
/// `IgClient::get_body` follows them itself, charging `Pacer::clear_to_send`
/// per hop and holding every hop to the same origin. Leaving it to the HTTP
/// client meant the hops went out unpaced and uncounted, which is what this
/// exists to stop — the reasoning is on `get_body`, next to the loop that does
/// it.
fn api_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::none()
}

/// Redirects for an asset: every hop held to the same rule as the first.
///
/// The picture URL comes out of Instagram's own answer, so a redirect chain is
/// the one place where a response gets to choose where the next request goes.
/// Checking only the address as written left hops two and three judged by
/// scheme alone, which is a weaker rule than the one the module documents.
fn cdn_policy(base: Url) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_HOPS {
            attempt.error("too many redirects")
        } else if serves_pictures(&base, attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("a redirect tried to leave the CDN")
        }
    })
}

/// One HTTP client under a given redirect policy.
///
/// The timeouts are not optional. Without them, a server that accepts the
/// connection and then says nothing hangs the process for good: neither the
/// cancel token nor any deadline above reaches a socket that is simply waiting.
fn build_client(
    user_agent: &str,
    redirect: reqwest::redirect::Policy,
) -> Result<reqwest::Client, IgError> {
    // The trust store is read here rather than passed in, for the reason
    // `http::TRUST` gives: `login::validate` builds a client inside this crate
    // and never sees the binary's arguments.
    Ok(crate::http::builder(
        user_agent,
        redirect,
        CONNECT_TIMEOUT,
        REQUEST_TIMEOUT,
        &crate::http::chosen_trust(),
    )?
    .build()?)
}

/// Reads a response body, refusing one that will not fit.
///
/// `text()` would buffer whatever arrives, which lets the far end decide how
/// much memory this process uses. Read in chunks so that a response with no
/// `Content-Length` — which is most of them — is bounded too.
async fn read_capped(response: reqwest::Response, cap: u64) -> Result<String, IgError> {
    // Lossy rather than strict: a body that is not valid UTF-8 is not JSON
    // either, and saying "could not parse" is more use than "invalid encoding".
    Ok(String::from_utf8_lossy(&read_capped_bytes(response, cap).await?).into_owned())
}

/// The ceiling itself: refuse a declared length over it, and read in chunks so
/// that a response without a `Content-Length` — which is most of them — is
/// bounded too.
///
/// One copy for both callers. The API body and the CDN download had the same
/// declared-length check, the same chunked read and the same `TooLarge` written
/// out separately, and the CDN is the wrong one to leave behind at the next
/// tightening: its URL comes out of Instagram's own answer, so it is the one
/// place a response chooses where the next request goes.
///
/// `http::read_capped` stays where it is. Its doc names why it truncates rather
/// than refusing, which is a different rule for a different reader.
async fn read_capped_bytes(mut response: reqwest::Response, cap: u64) -> Result<Vec<u8>, IgError> {
    let too_large = || IgError::TooLarge {
        limit: cap as usize,
    };

    if let Some(declared) = response.content_length()
        && declared > cap
    {
        return Err(too_large());
    }

    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() as u64 + chunk.len() as u64 > cap {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// `Retry-After`, if the answer carried one.
///
/// A string rather than a parsed duration on purpose: the header has two legal
/// forms, seconds and an HTTP date, and until it is known which of them these
/// endpoints send — if either — turning it into a number would be deciding the
/// answer to the question the logging exists to ask.
/// The load headers, if Instagram volunteered any, as one string to log.
///
/// Absent from a CDN answer and from anything that is not the API, so `None` is
/// ordinary rather than notable.
fn load_of(response: &reqwest::Response) -> Option<String> {
    let headers = response.headers();
    let named = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(|value| format!("{name}={value}"))
    };
    let found: Vec<String> = ["x-ig-capacity-level", "x-ig-peak-time"]
        .into_iter()
        .filter_map(named)
        .collect();
    (!found.is_empty()).then(|| found.join(" "))
}

fn retry_after(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// One answer from Instagram: what it said, and the one header worth keeping
/// hold of.
///
/// `classify` is a pure function of a status and a body and does not receive
/// headers, which is why `Retry-After` and the load below travel separately
/// rather than being read where the decision is made. **Nothing decides
/// anything from either of them**, deliberately — see
/// [`IgClient::note_push_back`].
struct Answer {
    status: u16,
    body: String,
    retry_after: Option<String>,
    /// What Instagram said about its own load while answering.
    ///
    /// `x-ig-capacity-level` and `x-ig-peak-time`, joined. Not decided on, and
    /// the reason is with the rest of the pacing reasoning in [`crate::pace`]:
    /// they describe a datacenter's headroom, which is the same for everyone in
    /// that region, and what the pace is managing is a checkpoint on one
    /// account. Carried so that the run which is finally refused can say what
    /// the load was at that moment, which is the observation nobody has.
    load: Option<String>,
}

/// Which side of the relationship is being requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Followers,
    Following,
}

impl Direction {
    fn segment(self) -> &'static str {
        match self {
            Self::Followers => "followers",
            Self::Following => "following",
        }
    }
}

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

/// Whether a refused mutation is worth spending a discovery walk on.
///
/// **Narrow on purpose.** A rotated identifier and a throttled account both
/// come back as a refusal, and walking megabytes of JavaScript at an account
/// Instagram has just said no to is the opposite of what the pacing rules are
/// for. So everything that means "stop asking" is excluded: a push-back, an
/// action block, a challenge, a dead session, a cancellation. What is left is
/// the shapes a bad `doc_id` actually takes — a 400 with a body that did not
/// parse, or one that did and said nothing useful.
/// **`NotFound` is in here and is not in `worth_a_second_route`**, twenty lines
/// away, and the two are not contradicting each other. There, a 404 means the
/// account does not exist and a second lookup would be a second request spent
/// confirming it. Here, a 404 is one of the ways Instagram refuses an operation
/// it no longer serves under that identifier, and the discovery walk costs it
/// nothing: it reads the CDN, not the API, so a wrong guess is bytes rather
/// than a request against the account's budget.
///
/// **And narrow in a second direction, which is the one that matters for a
/// write.** The walk ends in a second `mutate`, and a second mutation is only
/// safe when the first is known not to have happened. A 4xx and a 404 say
/// so; a 200 carrying `errors` says so -- that is a GraphQL refusal, and the
/// shape a stale identifier actually arrives in. A 5xx does not: an edge
/// that answers 502 may have forwarded the request before it failed. Nor
/// does the redirect `post` reports as an `Unexpected` 3xx, nor a 200 whose
/// body would not decode, which is a write that very probably happened and
/// an answer this program could not read. Any of those used to pass, and
/// the recovery then sent the follow again on a budget paid once and a
/// confirmation given once -- the replay the redirect policy exists to
/// prevent, arriving by another road.
fn worth_rediscovering(error: &IgError) -> bool {
    match error {
        IgError::NotFound { .. } => true,
        IgError::Unexpected { status, .. } => {
            (200..300).contains(status) || (400..500).contains(status)
        }
        _ => false,
    }
}

/// Which of the two things a browser does on instagram.com a request is.
///
/// **They do not carry the same headers, and the difference is not cosmetic —
/// it changes which handler answers.** Learned the hard way while finding the
/// write path: `POST /web/friendships/{pk}/follow/` sent with the app's headers
/// answers 404, which reads exactly like the route having been removed. It had
/// been, as it turned out, but that took a third attempt to establish rather
/// than the second.
///
/// - [`Surface::App`] is the single-page application talking to `/api/v1/` and
///   `/api/graphql`. It announces itself with `X-IG-App-ID`, `X-ASBD-ID`,
///   `X-IG-WWW-Claim` and `X-Requested-With`, and without the first of those
///   the API routes answer 403 even with a good session. Every read this tool
///   makes is one of these, and so is the write.
/// - [`Surface::Document`] is the browser **navigating** to a page, which is
///   how the two tokens a mutation needs are obtained. It is not an XHR at all:
///   it asks for HTML, it says `Sec-Fetch-Mode: navigate` and
///   `Sec-Fetch-Dest: document`, and it announces none of the app headers,
///   because at that moment there is no app yet — the page is what loads it.
///
/// So coherence here is per request rather than per client. One superset of
/// headers sent everywhere is a shape no browser produces anywhere.
#[derive(Debug, Clone, Copy)]
enum Surface<'a> {
    App,
    Document,
    /// The app again, but talking to Relay rather than to `/api/v1/`.
    ///
    /// It carries values rather than being a third marker, because the two
    /// headers that tell it apart are per request: the operation's name, and
    /// the page-scoped `lsd`. Both are already in hand at the call site and
    /// both already travel in the body, so putting them in the headers too is
    /// saying the same true thing in the second place the site says it -- not
    /// a disguise, which is what the header rule in `AGENTS.md` forbids.
    Relay {
        friendly_name: &'a str,
        lsd: &'a str,
    },
}

impl Surface<'_> {
    /// What this kind of request says it will accept.
    fn accept(self) -> &'static str {
        match self {
            // `*/*`, not `application/json`: that is what `fetch()` sends when
            // the page does not set one, and no browser sends the latter here.
            Self::App | Self::Relay { .. } => "*/*",
            // **Byte for byte what Chrome 151 sent on all six navigations in
            // the August 2026 capture**, `application/signed-exchange` and all.
            // It had been carrying a seventeen-space run in the middle of the
            // value, from a line continuation lost in an edit, and it went out
            // that way on the request that fetches the tokens before every
            // write. No
            // browser sends that, and the test nearest to it asserted only
            // `starts_with("text/html")` -- a fixture that could not see the
            // defect it was standing next to.
            // **`concat!`, and that is not a style choice.** This value was
            // carrying a seventeen-space run in the middle of it, and it went
            // out that way on the request that fetches the tokens before
            // every write.
            // The cause is `cargo fmt`: given a `\`-continued string literal
            // it joins the lines back together and materializes the
            // indentation as spaces inside the value. So the continuation form
            // cannot be used for a header, and neither can a single long line
            // without going past the width. Separate literals are the form
            // that survives formatting.
            //
            // Byte for byte what Chrome 151 sent on all six navigations in the
            // August 2026 capture, `application/signed-exchange` and all.
            Self::Document => concat!(
                "text/html,application/xhtml+xml,application/xml;q=0.9,",
                "image/avif,image/webp,image/apng,*/*;q=0.8,",
                "application/signed-exchange;v=b3;q=0.7"
            ),
        }
    }

    /// The `Sec-Fetch-Mode` and `Sec-Fetch-Dest` pair, which Instagram answers
    /// what a browser sends -- see the note on `Vary` in `dressed`, which the
    /// capture measured and which does not name these.
    fn fetch_mode(self) -> (&'static str, &'static str) {
        match self {
            Self::App | Self::Relay { .. } => ("cors", "empty"),
            Self::Document => ("navigate", "document"),
        }
    }

    /// Whether there is an app behind this request to announce.
    ///
    /// False for a navigation, and that is the whole of the distinction: at the
    /// moment a page is fetched there is no app yet, because the page is what
    /// loads it.
    fn announces_the_app(self) -> bool {
        !matches!(self, Self::Document)
    }

    /// How urgently the browser wants it.
    ///
    /// A navigation is `u=0`: nothing on the page can start until the document
    /// lands. Everything else is a fetch at the default urgency. This was one
    /// header applied to both until the surfaces were separated -- and the
    /// constant's own name said `FETCH` while it went out on the page fetch,
    /// which is a navigation.
    fn priority(self) -> &'static str {
        match self {
            Self::Document => client_hints::NAVIGATION_PRIORITY,
            _ => client_hints::FETCH_PRIORITY,
        }
    }
}

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
    fn cdn(&self) -> Result<&reqwest::Client, IgError> {
        if let Some(cdn) = self.cdn.get() {
            return Ok(cdn);
        }
        let built = build_client(&self.session.user_agent, cdn_policy(self.base.clone()))?;
        Ok(self.cdn.get_or_init(|| built))
    }

    /// The write client, built the first time a follow or an unfollow is sent.
    ///
    /// See [`Self::writer`] for why it does not share the read client.
    fn writer(&self) -> Result<&reqwest::Client, IgError> {
        if let Some(writer) = self.writer.get() {
            return Ok(writer);
        }
        let built = build_client(&self.session.user_agent, reqwest::redirect::Policy::none())?;
        Ok(self.writer.get_or_init(|| built))
    }

    /// The current `X-IG-WWW-Claim`.
    ///
    /// Instagram's session-continuity token. The server hands one back in
    /// `x-ig-set-www-claim` and expects it echoed on every request after that;
    /// the literal `0` is what a client sends **once**, on its first request of
    /// the session, to say it has not been given one yet.
    ///
    /// Following that cycle is just implementing the protocol as specified.
    /// Ignoring the response header would leave the client sending `0` forever
    /// — announcing on every request that it is making its first — which is a
    /// false statement about our own session and costs one line to avoid.
    fn claim(&self) -> String {
        self.claim.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Reads Instagram's answer and, if it was a push-back, records the
    /// cooldown before handing the error on.
    ///
    /// It lives here for the same reason the budget does: every caller needed
    /// the same reaction and only two of them remembered it, so a 429 outside a
    /// walk left no mark at all and the next run knocked on the same door
    /// straight away. Making the request is what earns the cooldown, so the
    /// place that makes requests is the place that records it.
    fn classify_and_record(&self, status: u16, body: &str) -> IgError {
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

    fn remember_claim(&self, response: &reqwest::Response) {
        let Some(fresh) = response
            .headers()
            .get("x-ig-set-www-claim")
            .and_then(|v| v.to_str().ok())
        else {
            return;
        };
        *self.claim.lock().unwrap_or_else(|e| e.into_inner()) = fresh.to_string();
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

    /// Checks the session works, with the cheapest request that exercises the
    /// very endpoint the walk will depend on.
    pub async fn validate(&self) -> Result<(), IgError> {
        let _: FriendshipsPage = self
            .get(
                &format!(
                    "/api/v1/friendships/{}/{}/",
                    self.session.ds_user_id,
                    Direction::Following.segment()
                ),
                &[("count", "1")],
                // The account's own page: the only one we can name before the
                // username has been resolved.
                "",
            )
            .await?;
        Ok(())
    }

    /// Identity of the authenticated account. The id comes from the cookie
    /// itself; the username is only requested if we do not have it already.
    pub async fn whoami(&self) -> Result<Identity, IgError> {
        let pk = self.session.ds_user_id;
        if let Some(username) = &self.session.username {
            self.validate().await?;
            return Ok(Identity {
                pk,
                username: Some(username.clone()),
            });
        }
        let username = self.resolve_username(pk).await?;
        Ok(Identity { pk, username })
    }

    /// Resolves a username from an id. Returns `None` if Instagram answers but
    /// does not carry the field: it is cosmetic information and must never make
    /// a login fail.
    pub async fn resolve_username(&self, pk: Pk) -> Result<Option<String>, IgError> {
        Ok(self.user_info(pk).await?.map(|u| u.username))
    }

    /// Everything `/api/v1/users/{pk}/info/` says about an account. One request.
    ///
    /// Worth going to separately for the full-size profile picture, which no
    /// other endpoint offers.
    pub async fn user_info(&self, pk: Pk) -> Result<Option<UserInfo>, IgError> {
        let envelope: UserInfoEnvelope = self
            .get(&format!("/api/v1/users/{pk}/info/"), &[], "")
            .await?;
        Ok(envelope.user)
    }

    /// Public profile data, including the follower and following counters and
    /// the high-resolution picture.
    ///
    /// **One request in the ordinary case, and two only when the first fails in
    /// one particular way.** Nothing that works today costs anything extra:
    /// [`IgClient::profile_by_name`] is tried first and its answer is returned
    /// as it always was.
    ///
    /// The fallback exists because this endpoint answers **400** for certain
    /// business accounts, with
    /// `Asset asset://laser.provider/ig_business_category_subvertical has been
    /// deleted. You cannot use this schema` — Instagram failing to serialize
    /// its own reply, reproducible, and nothing to do with the request. It
    /// takes down every command that names an account, because they all start
    /// by turning a username into an id. Verified against the live API in
    /// August 2026: 400 for `elrubiuswtf`, 200 for an ordinary account, which is
    /// why the second route is reached only from the failure and never
    /// replaces the first.
    ///
    /// [`IgError::worth_a_second_route`] is what keeps this from becoming a
    /// retry loop. A 429, an action block, a challenge, an expired session, a
    /// cancel and a 404 all answer `false` there, so the one rule that matters
    /// — when a service says no, stop asking — is not weakened by having a
    /// second route at all.
    ///
    /// What comes back from search is **less**: an identity and the two
    /// friendship flags, and no counters. It is not padded out.
    /// [`WebProfileInfo::counters_are_knowable`] is how a caller tells the
    /// difference, and `engine::target` says so out loud, because a walk with
    /// no declared size is a walk `pager::verify_completion` cannot check for
    /// truncation.
    pub async fn web_profile_info(&self, username: &str) -> Result<WebProfileInfo, IgError> {
        let failure = match self.profile_by_name(username).await {
            Ok(profile) => return Ok(profile),
            Err(e) => e,
        };
        if !failure.worth_a_second_route() {
            return Err(failure);
        }

        tracing::debug!(
            error = %failure,
            "the profile endpoint would not answer; trying search"
        );

        match self.search_user_id(username).await {
            Ok(Some(user)) => {
                tracing::debug!(pk = user.pk, "search resolved the account the profile lost");
                Ok(WebProfileInfo::from_search(user))
            }
            // Search answered and knows no such account. The original failure
            // is still the truthful thing to report: this route not finding it
            // is not evidence that the name is free, and a 400 reported as
            // "no such account" sends somebody hunting for a typo that is not
            // there.
            Ok(None) => {
                tracing::debug!("search knows no such account either");
                Err(failure)
            }
            // The fallback's own failure must not replace the real one --
            // **except** when it is one the user has to act on. A cooldown or a
            // dead session recorded on this request is a fact about the account
            // that would otherwise be swallowed by an error about serialization.
            Err(second) => {
                if crate::error::cooldown_for(&second).is_some() || second.invalidates_session() {
                    Err(second)
                } else {
                    Err(failure)
                }
            }
        }
    }

    /// The profile endpoint on its own, with no fallback behind it.
    async fn profile_by_name(&self, username: &str) -> Result<WebProfileInfo, IgError> {
        let missing = || IgError::NotFound {
            what: Some(username.to_string()),
        };
        // Instagram says the same thing two ways: a 404 for the page, or a 200
        // whose envelope carries no user. Both mean the name is not taken, and
        // the person asking should read one answer, not two.
        let envelope: WebProfileInfoEnvelope = self
            .get(
                "/api/v1/users/web_profile_info/",
                &[("username", username)],
                &format!("{}/", snob_core::model::in_a_path(username)),
            )
            .await
            .map_err(|e| match e {
                IgError::NotFound { .. } => missing(),
                other => other,
            })?;
        envelope.data.user.ok_or_else(missing)
    }

    /// Resolves a username to an id through the web client's search box.
    ///
    /// **A fallback, never a first choice.** It exists because
    /// `web_profile_info` answers 400 for certain business accounts with a
    /// serialization failure of Instagram's own -- see `resolve_id` -- and it
    /// is used only after that has happened.
    ///
    /// Search matches loosely, so the answer is filtered to an exact,
    /// case-insensitive match on the name asked for. Without that, asking about
    /// a name that does not exist hands back whatever the search box would have
    /// suggested instead, and the run then walks a stranger's followers under
    /// the name that was typed. That is the failure this whole route could
    /// introduce, and it is the only reason the comparison is here rather than
    /// left to the caller.
    ///
    /// `None` means search knows no such account, which is what a caller should
    /// report as "no such account" rather than as a failure of the fallback.
    pub async fn search_user_id(&self, username: &str) -> Result<Option<SearchUser>, IgError> {
        let found: TopSearch = self
            .get(
                "/web/search/topsearch/",
                &[("context", "blended"), ("query", username), ("count", "1")],
                // In a browser this is called from whatever page the search box
                // is open on. The site's own address is the truthful one.
                "",
            )
            .await?;

        Ok(found
            .users
            .into_iter()
            .map(|hit| hit.user)
            .find(|user| user.username.eq_ignore_ascii_case(username)))
    }

    /// One page of followers or following. Pagination is the caller's job.
    ///
    /// `username` is only used to name the page a browser would have made this
    /// call from. It is allowed to be empty — the walk still works — but the
    /// caller knows it, so it may as well say it.
    pub async fn friendships_page(
        &self,
        pk: Pk,
        username: &str,
        direction: Direction,
        count: u32,
        cursor: Option<&str>,
    ) -> Result<FriendshipsPage, IgError> {
        let count = count.to_string();
        let mut query: Vec<(&str, &str)> = vec![("count", &count)];
        if let Some(c) = cursor {
            query.push(("max_id", c));
        }
        let segment = direction.segment();
        self.get(
            &format!("/api/v1/friendships/{pk}/{segment}/"),
            &query,
            // In a browser this call only ever comes from the modal that opens
            // over the account's own page.
            &if username.is_empty() {
                String::new()
            } else {
                format!("{}/{segment}/", snob_core::model::in_a_path(username))
            },
        )
        .await
    }

    /// A page, as HTML, exactly as a browser navigating to it would get it.
    ///
    /// The one reader here that does not want JSON. It exists because the two
    /// tokens a mutation needs — `fb_dtsg` and `lsd` — are only ever handed out
    /// inside a rendered page; see [`crate::graphql`].
    ///
    /// It costs a request like everything else, and it is a **large** one: a
    /// profile page is around six hundred kilobytes of bootstrapped Relay
    /// state. That is why the caller caches what it finds rather than reading
    /// the page per write.
    pub async fn page(&self, path: &str) -> Result<String, IgError> {
        let answer = self.get_body(path, &[], "", Surface::Document).await?;
        if !(200..300).contains(&answer.status) {
            self.note_push_back(&answer);
            return Err(self.classify_and_record(answer.status, &answer.body));
        }
        Ok(answer.body)
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
    fn decode<T: DeserializeOwned>(&self, answer: &Answer) -> Result<T, IgError> {
        let body = answer.body.as_str();
        // A 200 can still be an error: Instagram returns `{"status":"fail"}`
        // with a 200 in some cases, and a GraphQL refusal is *always* a 200
        // with the reason in an `errors` array.
        if !(200..300).contains(&answer.status) || declares_failure(body) {
            // Here rather than in each caller. It used to be in `get` only,
            // which meant the write path -- the one that earns the twelve-hour
            // cooldown, and the one where a push-back is most worth seeing --
            // was the single class of request never measured, while
            // `AGENTS.md` promised every one of them was.
            self.note_push_back(answer);
            return Err(self.classify_and_record(answer.status, body));
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

    /// **The only function in this workspace that sends anything other than a
    /// GET to Instagram.** Everything the write rule in `AGENTS.md` promises is
    /// enforced on the way through here.
    ///
    /// Four things it does that [`IgClient::get`] does not, each of them there
    /// because a write is not a read:
    ///
    /// - **It pays the write budget**, not the read one, and like `get` it pays
    ///   before it sends. There is no argument that selects between the two:
    ///   `get` calls `clear_to_send` and this calls `clear_to_send_write`, so
    ///   the choice is made by which function the caller reached rather than by
    ///   a value it passed.
    /// - **It refuses without a CSRF token instead of finding out.** Instagram
    ///   would answer 403, and that 403 would cost a request, a slot of write
    ///   budget and — because `classify` reads a 403 as a dead session — a
    ///   message telling the user to log in again when their session is fine.
    ///   A `snob login --paste` session has no `csrftoken` unless one was given,
    ///   so this is the common case rather than the odd one.
    /// - **It sends `Origin`**, which the Fetch standard requires on a POST even
    ///   when the request is same-origin. See the comment in
    ///   [`IgClient::dressed`] for the half of that rule which lives on
    ///   the read side.
    /// - **It follows no redirect at all**, through [`IgClient::writer`].
    /// - **It is not abandoned once it is in flight**, and that is the one
    ///   place this program does not do what `AGENTS.md` says about Ctrl+C.
    ///   Every read races the cancel token against the socket, because giving
    ///   up on a read costs nothing: the answer was going to be thrown away.
    ///   Giving up on a write costs the one thing worth having, which is
    ///   knowing whether it happened — the request has already gone, Instagram
    ///   may well act on it, and reporting "canceled" to somebody who is now
    ///   following an account is worse than making them wait out the timeout.
    ///
    ///   The boundary is exact rather than convenient: `clear_to_send_write`
    ///   reads the token before it reserves and again inside the wait, so a
    ///   write is cancelable up to the moment it is sent and not after it. The
    ///   uncancelable part is the part that must not be abandoned.
    /// - **It cannot be pointed anywhere.** It takes a
    ///   [`graphql::Mutation`], not a path, so the set of things this program
    ///   can write is the set of variants that enum has. It used to take a
    ///   `&str`, and then "there is no third write" rested on nobody typing a
    ///   third string -- which a source-reading guard cannot check, because the
    ///   identifier Instagram acts on is a `doc_id` and no list of words
    ///   contains a number.
    ///
    /// The two doc links below name [`IgClient::dressed`], which is where the
    /// shared header list actually lives; they said `browser_headers` for a
    /// while after it was renamed.
    ///
    /// `referer` is the profile page the button would have been clicked on, in
    /// the same spelling `get` wants: a path with no leading slash.
    async fn post<T: DeserializeOwned>(
        &self,
        write: graphql::Mutation,
        form: &[(&str, &str)],
        referer: &str,
        style: Surface<'_>,
    ) -> Result<T, IgError> {
        // Before the budget is charged, so that a session which cannot write
        // does not spend a slot discovering it. The token itself is put on the
        // request by `dressed`, which adds it whenever the session has
        // one; this guard is what makes "whenever" mean "always" on this path.
        if self.session.csrftoken.is_none() {
            return Err(IgError::NoCsrfToken);
        }

        let url = self.base.join(write.path())?;
        tracing::debug!(%url, operation = write.friendly_name(), "POST");

        self.pacer.clear_to_send_write().await?;

        let request = self
            .dressed(self.writer()?.post(url), referer, style)
            .header("Origin", self.base.as_str().trim_end_matches('/'))
            .form(form);

        let response = request.send().await?;
        self.remember_claim(&response);

        let status = response.status();
        // Read before the body, for the same reason `get_body` reads it there:
        // the header is on the response, and a body that will not read must not
        // take the one measurement worth having away with it.
        let retry_after = retry_after(&response);
        let load = load_of(&response);

        // A redirect reaches here as a status rather than as a new request,
        // because the policy is `none()`. It is not success and it is not
        // something to replay, so it is reported as what it is.
        if status.is_redirection() {
            return Err(IgError::Unexpected {
                status: status.as_u16(),
                body: "Instagram redirected a write, which is never followed".into(),
            });
        }

        let body = match read_capped(response, MAX_BODY_BYTES).await {
            Ok(body) => body,
            Err(_) if !status.is_success() => String::new(),
            Err(e) => return Err(e),
        };

        self.decode(&Answer {
            status: status.as_u16(),
            body,
            retry_after,
            load,
        })
    }
    /// The stories an account has up right now. One request.
    ///
    /// **This does not tell anybody you looked.** Instagram registers a view
    /// through a separate call, which this project does not implement and which
    /// `crates/snob-core/tests/no_seen.rs` checks has not appeared. Fetching the
    /// reel is a read like any other.
    ///
    /// An account with nothing up answers 200 with an empty envelope rather
    /// than 404, so `None` means there are no stories and not that the account
    /// is missing — the caller resolved it before getting here.
    pub async fn stories(&self, pk: Pk, username: &str) -> Result<Option<Reel>, IgError> {
        let ids = pk.to_string();
        let envelope: ReelsMedia = self
            .get(
                "/api/v1/feed/reels_media/",
                &[("reel_ids", ids.as_str())],
                // In a browser this call comes from the story viewer, which
                // opens over the account's own page.
                &if username.is_empty() {
                    String::new()
                } else {
                    format!("{}/", snob_core::model::in_a_path(username))
                },
            )
            .await?;
        Ok(envelope.reel())
    }

    /// Follows an account. **A write.** See [`IgClient::post`].
    pub async fn follow(
        &self,
        pk: Pk,
        username: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<FriendshipStatus, IgError> {
        self.friendship(graphql::Mutation::Follow, pk, username, ids)
            .await
    }

    /// Unfollows an account. **A write.** See [`IgClient::post`].
    pub async fn unfollow(
        &self,
        pk: Pk,
        username: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<FriendshipStatus, IgError> {
        self.friendship(graphql::Mutation::Unfollow, pk, username, ids)
            .await
    }

    /// The `doc_id` of a mutation, out of the cache or out of the page's own
    /// JavaScript.
    ///
    /// The walk goes through the **CDN client**, which is the right one twice
    /// over: those bundles really are on the CDN, and that client carries no
    /// cookie and no app id -- a public script has no business being fetched
    /// with a session attached. It also means they are not charged against
    /// Instagram's request budget, for the reason [`IgClient::download`]
    /// already gives about pictures.
    ///
    /// `html` is the page already fetched for the tokens, so the list of
    /// bundles costs nothing extra, and its order is the page's own -- which
    /// puts the chunk its route needs near the front.
    async fn doc_id_for(
        &self,
        mutation: graphql::Mutation,
        html: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<String, IgError> {
        /// One bundle. Instagram's largest is around five megabytes; twelve is
        /// a ceiling rather than a target, and it is here for the reason every
        /// other ceiling in this file is.
        const MAX_BUNDLE_BYTES: usize = 12 * 1024 * 1024;
        /// How many to open before giving up.
        ///
        /// **A page names around four hundred and forty of these**, so this is
        /// a real bound rather than a formality: the walk is bytes off the CDN
        /// and minutes of wall clock, and it happens once per rotation because
        /// the answer is cached. Sixty is roughly seven megabytes, measured.
        /// Past that the answer is more likely to be that the shape changed
        /// than that the right chunk is one further along.
        const MOST_BUNDLES: usize = 60;

        let name = mutation.friendly_name();

        for url in graphql::bundles_in(html).into_iter().take(MOST_BUNDLES) {
            let Ok(bytes) = self.download_capped(&url, MAX_BUNDLE_BYTES).await else {
                // A bundle that will not come down is not the end of the
                // search: there are others, and the next may hold it.
                continue;
            };
            if let Some(id) = graphql::doc_id_in(&String::from_utf8_lossy(&bytes), name) {
                tracing::debug!(name, %url, "found the mutation id");
                ids.put(name, &id);
                return Ok(id);
            }
        }

        Err(IgError::MutationNotFound { name })
    }

    /// The body of both, because the two differ by one word.
    ///
    /// # Which request, and why it took three attempts to find out
    ///
    /// `POST /api/graphql`, naming the mutation. Settled by capturing the real
    /// web client in August 2026 -- see [`crate::graphql`], which carries the
    /// finding and the two routes that were sent live first and changed
    /// nothing.
    ///
    /// Two requests, and both are paid for: the page that hands out the tokens,
    /// and the mutation. The page is the expensive one at around six hundred
    /// kilobytes, and it is why there is no bulk mode to be tempted by even if
    /// the rule allowed one.
    ///
    /// The profile fetched is **the target's**, which is the page a browser
    /// would have been on when the button was pressed. Any logged-in page would
    /// hand out the same tokens, so this is coherence rather than necessity --
    /// but a request whose `Referer` names a page nobody loaded is the kind of
    /// small incoherence the rest of this module exists to avoid.
    async fn friendship(
        &self,
        mutation: graphql::Mutation,
        pk: Pk,
        username: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<FriendshipStatus, IgError> {
        // **Before the page.** `post` refuses without a CSRF token too, but by
        // then the expensive half has been spent: a profile page is around six
        // hundred kilobytes and a paid-for request. A session that cannot write
        // is the common case — every `snob login --paste` without `--csrftoken`
        // produces one — so this is the ordinary path, not the odd one.
        if self.session.csrftoken.is_none() {
            return Err(IgError::NoCsrfToken);
        }

        let referer = if username.is_empty() {
            String::new()
        } else {
            format!("{}/", snob_core::model::in_a_path(username))
        };

        let html = self.page(&format!("/{referer}")).await?;
        // A logged-out page renders too, and it has no tokens in it. Saying so
        // here is better than sending a mutation that cannot be authorized and
        // reading whatever Instagram says about it.
        let tokens = graphql::extract_tokens(&html).ok_or(IgError::SessionExpired)?;

        // What is known, and otherwise what was captured.
        // [`graphql::Mutation::seed_doc_id`] explains why there is a captured
        // value at all, and why discovery is the recovery rather than the way in.
        let name = mutation.friendly_name();
        let doc_id = ids
            .get(name)
            .unwrap_or_else(|| mutation.seed_doc_id().to_string());

        match self.mutate(mutation, &tokens, &doc_id, pk, &referer).await {
            Ok(status) => Ok(status),
            // **The recovery path.** A rotated identifier is refused, and the
            // refusal looks like several other things — so rather than trying
            // to read Instagram's mind, the walk runs and is worth something
            // only if it comes back with a *different* answer. It costs
            // megabytes off the CDN, which is why it is here and not on the way
            // in, and the original error is what the user hears if it fails.
            Err(first) if worth_rediscovering(&first) => {
                let Ok(found) = self.doc_id_for(mutation, &html, ids).await else {
                    return Err(first);
                };
                if found == doc_id {
                    return Err(first);
                }
                tracing::debug!(name, "the stored identifier was stale; trying the new one");
                self.mutate(mutation, &tokens, &found, pk, &referer).await
            }
            Err(other) => Err(other),
        }
    }

    /// One attempt at the mutation.
    async fn mutate(
        &self,
        mutation: graphql::Mutation,
        tokens: &graphql::PageTokens,
        doc_id: &str,
        pk: Pk,
        referer: &str,
    ) -> Result<FriendshipStatus, IgError> {
        let body = graphql::mutation_body(tokens, mutation, doc_id, pk);
        let form: Vec<(&str, &str)> = body.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        let answer: FriendshipResult = self
            .post(
                mutation,
                &form,
                referer,
                Surface::Relay {
                    friendly_name: mutation.friendly_name(),
                    lsd: tokens.lsd.expose(),
                },
            )
            .await?;
        Ok(answer.status())
    }

    /// Downloads a public asset, such as a profile picture.
    ///
    /// These live on the CDN, a different host from the API, so the URL arrives
    /// absolute and this does not go through [`IgClient::get`]. Nothing
    /// identifying travels with it: no session cookie, no app id. The CDN is a
    /// third party and has no business seeing either, and the asset is public
    /// anyway. Reading only, like the rest of this crate.
    pub async fn download(&self, url: &str) -> Result<Vec<u8>, IgError> {
        self.download_capped(url, MAX_ASSET_BYTES).await
    }

    /// The same, with the caller naming the ceiling.
    ///
    /// A story video does not fit under [`MAX_ASSET_BYTES`], which was sized
    /// for a 1080x1080 picture. Rather than raising that constant — and with it
    /// the ceiling on every profile picture, for a reason that has nothing to
    /// do with profile pictures — the caller that needs a different one says
    /// so, and says why where it says it.
    ///
    /// Everything else is identical, [`IgClient::check_downloadable`]
    /// included: the URL still has to point at the CDN, and every redirect hop
    /// after it is held to the same rule.
    pub async fn download_capped_public(&self, url: &str, cap: usize) -> Result<Vec<u8>, IgError> {
        self.download_capped(url, cap).await
    }

    /// Refuses a picture URL that does not go where a picture goes.
    ///
    /// The address of the first hop comes straight out of Instagram's answer,
    /// so it is the one an attacker gets to choose: a `profile_pic_url` of
    /// `http://127.0.0.1:9222/json` or of a cloud metadata address would be
    /// fetched as written. It is held to the same rule [`cdn_policy`] holds
    /// every hop after it to, which is the point of the rule being one
    /// function.
    fn check_downloadable(&self, url: &Url) -> Result<(), IgError> {
        if serves_pictures(&self.base, url) {
            return Ok(());
        }
        Err(IgError::Unexpected {
            status: 0,
            body: format!(
                "the picture URL points somewhere pictures do not come from: {}",
                url.host_str().unwrap_or("nowhere")
            ),
        })
    }

    /// The body of [`IgClient::download`], with the ceiling as an argument so a
    /// test can reach it without moving eight megabytes around.
    async fn download_capped(&self, url: &str, cap: usize) -> Result<Vec<u8>, IgError> {
        let url = Url::parse(url)?;
        self.check_downloadable(&url)?;
        tracing::debug!(%url, "GET asset");

        // Deliberately not paced: the CDN is a different host with its own
        // limits, and charging a picture against Instagram's budget would make
        // the number mean two things at once.
        //
        // `Accept-Encoding` is the browser's, for the same reason it is on the
        // API request and not for a different one. This request sends no header
        // of its own, so reqwest inserted the string it assembles from whichever
        // decoders were compiled in — `zstd,gzip,deflate,br`, which no browser
        // has ever sent — under a User-Agent that says Chrome. Everything else
        // about this client is deliberately unlike the API one; this is not one
        // of those things.
        let response = self
            .send_or_cancel(
                self.cdn()?
                    .get(url)
                    .header("Accept-Encoding", self.hints.accept_encoding),
            )
            .await?;
        let status = response.status();

        // Deliberately not `classify`: that reads Instagram's API vocabulary,
        // and the CDN does not speak it. Its 403 means the signed link has
        // expired, not that the session died, and saying otherwise would send
        // someone to log in again over a stale URL.
        if !status.is_success() {
            return Err(IgError::Unexpected {
                status: status.as_u16(),
                body: "the picture could not be downloaded".into(),
            });
        }

        // Raced against the token like the API read, for the same reason: a
        // CDN that answers with headers and then stalls holds this process for
        // as long as it likes.
        tokio::select! {
            biased;
            () = self.pacer.cancel_token().canceled() => Err(IgError::Canceled),
            bytes = read_capped_bytes(response, cap as u64) => bytes,
        }
    }

    /// One request to Instagram's API, with the headers a browser would send.
    ///
    /// `referer` is the path of the page the call would have come from, without
    /// the leading slash. It is not decoration: the tool asks for a followers
    /// list from a URL that, in a browser, only that account's followers page
    /// ever calls, and a generic referer next to a specific endpoint is an
    /// incoherence that costs nothing to avoid.
    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        referer: &str,
    ) -> Result<T, IgError> {
        let answer = self.get_body(path, query, referer, Surface::App).await?;
        self.decode(&answer)
    }

    /// Sends the request and follows any redirect itself, **paying for every
    /// hop**.
    ///
    /// The redirect policy used to be reqwest's, and that is why this loop
    /// exists. `Pacer::clear_to_send` sits inside this function, so a hop the
    /// HTTP client followed on its own went out without being charged and
    /// without waiting — against the rule that every request is paid for, and
    /// invisibly, because the budget's own count is what the user is shown. A
    /// chain of three therefore reported one request and sent four, at whatever
    /// rate the network allowed rather than at the tool's.
    ///
    /// The origin rule is unchanged and is applied to every hop rather than to
    /// the first: Instagram's JSON endpoints do not redirect off their own
    /// host, so refusing costs nothing — and two of the headers on these
    /// requests are credentials. reqwest drops `Cookie` when a redirect crosses
    /// hosts, but `X-CSRFToken` is not on the list it knows about and would
    /// travel to wherever the response pointed. A boundary that depends on
    /// somebody else's list of header names is not one.
    ///
    /// **A refusal here has to reach [`Reaction::Abort`]**, which is why the
    /// two ways out are variants of their own rather than an `Unexpected`. When
    /// reqwest refused the hop it raised an ordinary `reqwest::Error`, that
    /// landed as `Network`, whose reaction is `Retry`, and the pager sent the
    /// same impossible request three more times with the session on it —
    /// measured at 13 requests on the wire against 5 charged. Moving the
    /// refusal here must not reintroduce the same thing under a new name.
    ///
    /// The query is attached to the first request only. A `Location` carries
    /// whatever query it means to carry, and appending ours to it would send a
    /// parameter the server did not ask to see twice.
    async fn get_body(
        &self,
        path: &str,
        query: &[(&str, &str)],
        referer: &str,
        style: Surface<'_>,
    ) -> Result<Answer, IgError> {
        let mut url = self.base.join(path)?;
        let mut query: Option<&[(&str, &str)]> = Some(query);
        let mut hops: usize = 0;

        loop {
            tracing::debug!(%url, "GET");

            // Paid for before it is sent, and there is no way in that skips
            // this — the hops included, which is the whole point of the loop.
            self.pacer.clear_to_send().await?;

            let response = self
                .send_or_cancel(self.api_request(&url, query, referer, style))
                .await?;
            self.remember_claim(&response);
            let status = response.status();

            if status.is_redirection() {
                let next = self.next_hop(&url, &response)?;
                if hops >= MAX_HOPS {
                    return Err(IgError::TooManyRedirects);
                }
                hops += 1;
                url = next;
                query = None;
                continue;
            }

            // The status is already in hand, and a body that will not read must
            // not take it away. With `?` here, a 429 whose body died mid-stream
            // became `IgError::Network` — whose reaction is `Retry` — so the
            // walker fired three more requests into an endpoint that had just
            // said no, and `classify_and_record`, the only caller of
            // `cooldown_for` there is, never ran: nothing was written down and
            // the next run knocked again. What Instagram said is the status;
            // the body only refines it.
            let retry_after = retry_after(&response);
            let load = load_of(&response);

            let body = match self.read_or_cancel(response, MAX_BODY_BYTES).await {
                Ok(body) => body,
                // A canceled read is the user, not the server, and must not be
                // turned into a push-back that gets written down as one.
                Err(IgError::Canceled) => return Err(IgError::Canceled),
                Err(_) if !status.is_success() => {
                    let answer = Answer {
                        status: status.as_u16(),
                        body: String::new(),
                        retry_after,
                        load,
                    };
                    self.note_push_back(&answer);
                    return Err(self.classify_and_record(answer.status, ""));
                }
                Err(e) => return Err(e),
            };
            return Ok(Answer {
                status: status.as_u16(),
                body,
                retry_after,
                load,
            });
        }
    }

    /// Sends the request, or gives up the moment the user asks it to.
    ///
    /// **Ctrl+C used to wait for the server.** Cancellation was read in
    /// `Pacer::clear_to_send` and in every deliberate wait, which is where about
    /// nine interrupts in ten land — a stop during a budget wait takes about a
    /// second. The tenth lands here, and here nothing was watching: the exit
    /// tracked however long the far end chose to hold the connection, up to
    /// `REQUEST_TIMEOUT`, or 254 seconds on a black-holed connection because
    /// `Network` is retried. That matters most under a service manager, where a
    /// stalled request outlasts the stop grace period and the process is killed
    /// before it can close its snapshot.
    ///
    /// **The cancel branch must answer [`IgError::Canceled`]**, and that is the
    /// part worth guarding. Dropping the future and letting the resulting
    /// `reqwest::Error` fall through would classify as `Network`, whose reaction
    /// is `Retry`, so the pager would answer a Ctrl+C by sending the request
    /// three more times.
    ///
    /// `biased`, so a token that is already set wins against a response that
    /// happens to be ready in the same poll.
    async fn send_or_cancel(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, IgError> {
        tokio::select! {
            biased;
            () = self.pacer.cancel_token().canceled() => Err(IgError::Canceled),
            response = request.send() => Ok(response?),
        }
    }

    /// Reads the body, or gives up the moment the user asks it to.
    ///
    /// The other half of [`IgClient::send_or_cancel`], and not an afterthought:
    /// a server that answers with headers and then stalls mid-body holds the
    /// connection exactly as long, and the read is where those seconds are
    /// spent.
    async fn read_or_cancel(
        &self,
        response: reqwest::Response,
        cap: u64,
    ) -> Result<String, IgError> {
        tokio::select! {
            biased;
            () = self.pacer.cancel_token().canceled() => Err(IgError::Canceled),
            body = read_capped(response, cap) => body,
        }
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
    fn note_push_back(&self, answer: &Answer) {
        tracing::debug!(
            status = answer.status,
            retry_after = answer.retry_after.as_deref().unwrap_or("<absent>"),
            load = answer.load.as_deref().unwrap_or("<absent>"),
            "Instagram pushed back"
        );
    }

    /// Where a redirect points, if it points somewhere this client may go.
    ///
    /// A `Location` is allowed to be relative, so it is resolved against the
    /// URL that produced it rather than parsed on its own — and the result is
    /// held to the same origin rule as the first request. Both refusals are
    /// excerpted like every other error that prints something a server chose:
    /// this string is printed to a terminal and Instagram wrote it.
    fn next_hop(&self, from: &Url, response: &reqwest::Response) -> Result<Url, IgError> {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| IgError::Unexpected {
                status: response.status().as_u16(),
                body: "a redirect arrived with nowhere to go".into(),
            })?;

        let next = from.join(location).map_err(|_| IgError::OffOrigin {
            to: crate::error::body_excerpt(location),
        })?;

        if !same_origin(&self.base, &next) {
            return Err(IgError::OffOrigin {
                to: crate::error::body_excerpt(next.as_str()),
            });
        }
        Ok(next)
    }

    /// The request itself, with the headers a browser would send.
    ///
    /// Split from the sending so that a redirect hop is built the same way the
    /// first request was, rather than by whatever the HTTP client decided to
    /// carry forward.
    fn api_request(
        &self,
        url: &Url,
        query: Option<&[(&str, &str)]>,
        referer: &str,
        style: Surface<'_>,
    ) -> reqwest::RequestBuilder {
        let request = self.api.get(url.clone());
        let request = match query {
            Some(q) => request.query(q),
            None => request,
        };
        self.dressed(request, referer, style)
    }

    /// The headers every request to Instagram carries, whatever its method.
    ///
    /// Split out of [`Self::api_request`] at the merge, because the write path
    /// needs the same set on a POST. Two copies of this list is how the
    /// coherence `client_hints.rs` exists to maintain gets lost: a header added
    /// for a good reason on one and forgotten on the other makes two requests
    /// on one session look like two clients.
    fn dressed(
        &self,
        request: reqwest::RequestBuilder,
        referer: &str,
        style: Surface<'_>,
    ) -> reqwest::RequestBuilder {
        let mut request = request;
        if style.announces_the_app() {
            request = request
                // Without these Instagram answers 403 even with a good session
                // -- on the API routes. A navigation announces neither, because
                // at that moment there is no app yet.
                .header("X-IG-App-ID", IG_APP_ID)
                .header("X-ASBD-ID", client_hints::ASBD_ID);
        }
        // **Only on `/api/v1/`, and this is measured rather than reasoned.** A
        // second browser capture in August 2026 -- the first one recorded the
        // headers a page set rather than the ones that went on the wire, which
        // is why this waited for a second -- put the count beyond argument:
        //
        //     x-ig-www-claim     0/171 on /api/graphql, 97/97 on /api/v1/*
        //     x-requested-with   0/171 on /api/graphql, 97/97 on /api/v1/*
        //
        // Relay does not announce itself as an XHR and does not echo the claim.
        // It sends `X-FB-Friendly-Name` and `X-FB-LSD` instead, which is what
        // the branch below is for, and sending both sets is a shape no browser
        // produces -- the failure [`Surface`] exists to prevent.
        if matches!(style, Surface::App) {
            request = request
                .header("X-IG-WWW-Claim", self.claim())
                .header("X-Requested-With", "XMLHttpRequest");
        }
        // What a Relay request says about itself that an `/api/v1/` one does
        // not. Both values already travel in the body of the same request --
        // `fb_api_req_friendly_name` and `lsd` -- so neither is new state and
        // neither is invented; this is the request agreeing with itself in the
        // second place the site states it. Read off a real browser in August
        // 2026, where every `/api/graphql` call carried both.
        if let Surface::Relay { friendly_name, lsd } = style {
            request = request
                .header("X-FB-Friendly-Name", friendly_name)
                .header("X-FB-LSD", lsd);
        }
        let mut request = request
            // Not a literal: a navigation asks for HTML and an XHR asks for
            // anything. This used to say Instagram answers `Vary` on the two
            // `Sec-Fetch-*` headers below; the August 2026 capture saw `Vary`
            // on `Origin`, on `Accept-Encoding` and on `Accept-Language,
            // Cookie`, and on no `Sec-Fetch-*` at all. **Unverified rather than
            // corrected**: that capture kept no response headers that survive
            // to be re-read, so the honest state of the claim is that nobody
            // has checked it, not that it is false. It changes nothing either
            // way -- the reason to send them is the one two paragraphs up in
            // [`Surface`], that a browser sends them and a request without them
            // is the anomaly.
            .header("Accept", style.accept())
            // Computed from the User-Agent like the rest of the set rather
            // than written out, because Chromium began offering `zstd` in the
            // same release it began sending `Priority`: as a literal, a session
            // created with an older Chrome omitted the one and offered the
            // other. `client_hints::ACCEPT_ENCODING` carries the value, and why
            // it is not left to the HTTP client to invent.
            .header("Accept-Encoding", self.hints.accept_encoding)
            // The one header here that comes from the person rather than from
            // the User-Agent. Every browser sends it on every request.
            .header("Accept-Language", client_hints::accept_language())
            // See the note on `Accept` above for what is and is not known
            // about `Vary` here. `Accept-Language` is the one the capture did
            // confirm: Instagram answered `Vary: Accept-Language, Cookie`, so
            // the value `client_hints::accept_language` computes really does
            // change the reply, which until then was an assumption.
            .header("Sec-Fetch-Site", "same-origin")
            .header("Sec-Fetch-Mode", style.fetch_mode().0)
            .header("Sec-Fetch-Dest", style.fetch_mode().1)
            // Built rather than interpolated. Every caller encodes the name it
            // puts in here, so this cannot fail today — but a header value
            // that will not build is not an error reqwest raises where it is
            // made. It carries it to `send()`, where `?` classifies it as
            // `Network`, whose reaction is `Retry`: the pager would send the
            // same impossible request three more times, each one paid for by a
            // budget that thinks it bought a request. The site's own address
            // is a truthful referer, and a slightly less specific one costs
            // nothing next to that.
            .header(
                "Referer",
                reqwest::header::HeaderValue::try_from(format!("{BASE_URL}/{referer}"))
                    .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(BASE_URL)),
            )
            .header("Cookie", self.session.cookie_header().as_str());

        // Deliberately no `Origin` **here**: the Fetch standard omits it on
        // same-origin GETs, so sending one next to `Sec-Fetch-Site:
        // same-origin` would be two headers contradicting each other. Easy to
        // add by reflex, which is why it is called out rather than left to be
        // noticed.
        //
        // The other half of the same rule, and the reason this now says
        // "here": the standard requires `Origin` on a POST even when it is
        // same-origin. Omitting it there would be the identical incoherence the
        // other way round, so `post` adds it, and only `post`.

        // **Only where an app would send one.** A browser navigating to
        // `instagram.com/nasa/` sends no `X-CSRFToken`; it is a header the page
        // adds to its own XHRs once it is running. This was applied to every
        // request that had a token, which put it on the page fetch in
        // [`IgClient::page`] -- the one request in this program that is a
        // navigation. `a_navigation_does_not_claim_to_be_the_app` exists to
        // catch exactly this and was checking four other names.
        if let Some(csrf) = &self.session.csrftoken
            && style.announces_the_app()
        {
            request = request.header("X-CSRFToken", csrf.expose());
        }

        // Only for browsers that send client hints at all. Inventing them for
        // Firefox would be a mismatch rather than an improvement.
        if let Some(brands) = &self.hints.ua_brands {
            request = request
                .header("Sec-CH-UA", brands)
                .header("Sec-CH-UA-Mobile", self.hints.mobile)
                .header("Sec-CH-UA-Platform", self.hints.platform);
        }

        // Likewise: only the versions that send one. **Which one is the
        // surface's to say, not the browser's** -- the hints answer whether
        // this Chrome sends a `Priority` at all, and the request answers how
        // urgent it is. One value went out on both until the surfaces were
        // separated, and the constant carrying it is called `FETCH_PRIORITY`.
        if self.hints.priority.is_some() {
            request = request.header("Priority", style.priority());
        }

        request
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use snob_core::budget::{RateBudget, RateBudgetError};
    use snob_core::session::{Session, SessionOrigin};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
    const SID: &str = "42%3AAbCdEfGh%3A20";

    async fn client(server: &MockServer) -> IgClient {
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
    struct Recording {
        started: std::sync::Mutex<Vec<(String, std::time::Duration)>>,
    }

    impl Recording {
        fn calls(&self) -> Vec<(String, std::time::Duration)> {
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
    fn watching(base: &str) -> (IgClient, Arc<Recording>) {
        watching_as(base, false)
    }

    /// The same, with the option of a session that can write. Kept as one
    /// helper so the two paths are driven through identical wiring and any
    /// difference in what gets recorded is the code's rather than the test's.
    fn watching_as(base: &str, can_write: bool) -> (IgClient, Arc<Recording>) {
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

    async fn answering(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

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

    /// A 429 whose body dies mid-stream is still a 429.
    ///
    /// The read used to be `read_capped(...).await?`, so the failure to read
    /// replaced a status already in hand with `IgError::Network` — whose
    /// reaction is `Retry`. The walker then fired three more requests into an
    /// endpoint that had just said no, and `classify_and_record` never ran, so
    /// nothing was written down either. Both halves of the rule went at once.
    ///
    /// Served from a raw socket rather than from `wiremock`, because what has to
    /// happen is a body that stops arriving: the headers announce a length and
    /// the connection closes before it is sent.
    #[tokio::test]
    async fn a_throttled_answer_with_a_body_that_dies_is_still_throttled() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());

        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();

            // Drained first, or the close below races the request still being
            // written and the failure lands on the send rather than on the read.
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while socket.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }

            // Announced as 4096 bytes, and then the connection goes away with
            // none of them sent. The pause is what lets the head be delivered
            // and the body read begin before that happens.
            let _ =
                socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 4096\r\n\r\n");
            let _ = socket.flush();
            std::thread::sleep(std::time::Duration::from_millis(150));
        });

        let (client, budget) = watching(&base);
        let error = client.validate().await.unwrap_err();
        server.join().unwrap();

        assert!(
            matches!(error, IgError::RateLimited),
            "a body that would not read must not turn a 429 into a network error: {error:?}"
        );
        assert_eq!(error.reaction(), crate::error::Reaction::Cooldown);
        assert_eq!(
            budget.calls(),
            vec![(
                "rate_limit".to_string(),
                snob_core::budget::rate_limit_cooldown()
            )]
        );
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
    async fn it_sends_the_headers_instagram_requires() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/following/"))
            .and(query_param("count", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
            .mount(&server)
            .await;

        client(&server).await.validate().await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let headers = &requests[0].headers;
        let read = |name: &str| {
            headers
                .get(name)
                .unwrap_or_else(|| panic!("missing header {name}"))
                .to_str()
                .unwrap()
                .to_string()
        };

        assert_eq!(read("x-ig-app-id"), IG_APP_ID);
        assert_eq!(read("user-agent"), UA);
        assert_eq!(
            read("cookie"),
            "sessionid=42%3AAbCdEfGh%3A20; ds_user_id=42"
        );
        assert_eq!(read("x-requested-with"), "XMLHttpRequest");

        // A browser sends these, so a request without them is the anomaly.
        // (It used to say Instagram answers `Vary` on them. It does not; see
        // `client_hints`'s header for the count.)
        assert_eq!(read("sec-fetch-site"), "same-origin");
        assert_eq!(read("sec-fetch-mode"), "cors");
        assert_eq!(read("sec-fetch-dest"), "empty");

        // What `fetch()` sends when the page sets nothing. `application/json`
        // is a value no browser produces here.
        assert_eq!(read("accept"), "*/*");

        // Chrome's own string, not the one the HTTP client assembles from
        // whichever decoders happen to be compiled in.
        assert_eq!(read("accept-encoding"), "gzip, deflate, br, zstd");

        // Present, and shaped like a language preference rather than like a
        // locale. What it says depends on the machine, so that is all this can
        // assert.
        let language = read("accept-language");
        assert!(!language.is_empty());
        assert!(!language.contains('_'), "{language}");

        // Chrome 138 is past the version that started sending this.
        assert_eq!(read("priority"), client_hints::FETCH_PRIORITY);

        // The client hints have to agree with the User-Agent above, which says
        // Chrome 138 on Windows.
        assert!(read("sec-ch-ua").contains(r#""Google Chrome";v="138""#));
        assert_eq!(read("sec-ch-ua-platform"), "\"Windows\"");
        assert_eq!(read("sec-ch-ua-mobile"), "?0");

        // A browser omits `Origin` on a same-origin GET. Sending one next to
        // `Sec-Fetch-Site: same-origin` is a pairing Chrome cannot produce.
        assert!(
            headers.get("origin").is_none(),
            "Origin must not travel on a same-origin GET"
        );
    }

    /// The token is announced as `0` once and is the server's answer after
    /// that. Staying on `0` forever would claim, on every request, to be
    /// making the first request of the session.
    #[tokio::test]
    async fn the_www_claim_is_echoed_back_after_the_first_answer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"users":[]}"#)
                    .insert_header("x-ig-set-www-claim", "hmac.AR2nhXYZ"),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        client.validate().await.unwrap();
        client.validate().await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let claim = |i: usize| {
            requests[i]
                .headers
                .get("x-ig-www-claim")
                .unwrap()
                .to_str()
                .unwrap()
        };
        assert_eq!(claim(0), "0", "the first one has nothing to echo yet");
        assert_eq!(claim(1), "hmac.AR2nhXYZ");
    }

    /// The referer names the page a browser would have called from. A generic
    /// one next to a followers endpoint is an incoherence that costs nothing
    /// to avoid.
    #[tokio::test]
    async fn the_referer_names_the_page_the_call_would_come_from() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
            .mount(&server)
            .await;

        client(&server)
            .await
            .friendships_page(7, "someone", Direction::Followers, 50, None)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests[0]
                .headers
                .get("referer")
                .unwrap()
                .to_str()
                .unwrap(),
            "https://www.instagram.com/someone/followers/"
        );
    }

    /// A name that cannot go in a header verbatim must still get an answer
    /// about the name.
    ///
    /// `target::clean` strips a leading `@` and nothing else, and `watch.toml`
    /// does not validate a username at all, so the referer is the one
    /// name-in-a-URL in this crate that arrives as typed. Any byte below 0x20
    /// makes the header unbuildable; reqwest holds that failure until `send()`,
    /// where `?` reads it as `Network` — a *retryable* fault, so the pager
    /// sends the same doomed request three more times and the pacer charges
    /// for four requests that never left the machine. The name it was asking
    /// about is never mentioned.
    #[tokio::test]
    async fn a_hostile_name_still_reaches_the_not_found_arm() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{"user":null}}"#))
            .mount(&server)
            .await;

        let hostile = "gh\u{1b}[2K";
        let error = client(&server)
            .await
            .web_profile_info(hostile)
            .await
            .unwrap_err();

        assert!(
            matches!(&error, IgError::NotFound { what: Some(name) } if name == hostile),
            "{error:?}"
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            1,
            "the request the budget paid for went out"
        );
        assert_eq!(
            requests[0]
                .headers
                .get("referer")
                .unwrap()
                .to_str()
                .unwrap(),
            "https://www.instagram.com/gh%1B%5B2K/",
            "encoded, not filtered: a name with a character removed is a different account"
        );
    }

    /// The net under the encoding above, reached the only way it can be: by
    /// handing `get` a referer no caller builds any more.
    ///
    /// It matters because of what the alternative costs. A header that will not
    /// build is not refused where it is written — reqwest carries it to
    /// `send()`, `?` turns it into `Network`, and `Network`'s reaction is
    /// `Retry`. Four requests charged to the budget, none of them sent, and the
    /// answer says the network is at fault.
    #[tokio::test]
    async fn a_referer_that_will_not_build_falls_back_to_the_site() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let _: serde_json::Value = client(&server)
            .await
            .get("/api/v1/users/web_profile_info/", &[], "gh\u{1b}[2K/")
            .await
            .expect("a referer is not worth failing a request over");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests[0]
                .headers
                .get("referer")
                .unwrap()
                .to_str()
                .unwrap(),
            BASE_URL,
            "less specific, and still true"
        );
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
    async fn it_reads_a_page_of_followers() {
        let server = MockServer::start().await;
        let body = r#"{"users":[
            {"pk":"1","username":"one","full_name":"One","is_verified":true,"is_private":false},
            {"pk":2,"username":"two"}
        ],"next_max_id":"QVFB","status":"ok"}"#;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/followers/"))
            .and(query_param("count", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let page = client(&server)
            .await
            .friendships_page(42, "someone", Direction::Followers, 50, None)
            .await
            .unwrap();

        assert_eq!(page.users.len(), 2);
        assert_eq!(page.users[0].username, "one");
        assert_eq!(page.users[0].is_verified, Some(true));
        assert_eq!(page.users[1].full_name, None);
        assert_eq!(page.next_cursor(), Some("QVFB"));
    }

    #[tokio::test]
    async fn the_cursor_is_sent_as_max_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("max_id", "QVFB"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
            .mount(&server)
            .await;

        client(&server)
            .await
            .friendships_page(42, "someone", Direction::Following, 50, Some("QVFB"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn it_resolves_a_username_from_an_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users/42/info/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"user":{"pk":42,"username":"whoever"},"status":"ok"}"#),
            )
            .mount(&server)
            .await;

        let name = client(&server).await.resolve_username(42).await.unwrap();
        assert_eq!(name.as_deref(), Some("whoever"));
    }

    /// The live answer to a name nobody owns: a 404 carrying a web page. What
    /// reaches the terminal must be the account name, not the markup.
    #[tokio::test]
    async fn a_profile_that_does_not_exist_is_named_not_dumped() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users/web_profile_info/"))
            .respond_with(ResponseTemplate::new(404).set_body_string(
                "<!DOCTYPE html><html><head><title>Page Not Found</title></head></html>",
            ))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .web_profile_info("nobody")
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("nobody"), "{message}");
        assert!(!message.contains("DOCTYPE"), "{message}");
    }

    /// The other half of the same answer: a 200 whose envelope has no user.
    /// It must read exactly like the 404 above.
    #[tokio::test]
    async fn an_empty_profile_envelope_reads_like_the_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users/web_profile_info/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{"user":null}}"#))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .web_profile_info("nobody")
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "the account \"nobody\" does not exist");
    }

    /// The whole point of a separate download path: the CDN is someone else's
    /// server, and the session must not reach it.
    #[tokio::test]
    async fn a_download_carries_nothing_identifying() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xD8, 0xFF, 0xE0]))
            .mount(&server)
            .await;

        let bytes = client(&server)
            .await
            .download(&format!("{}/pic.jpg", server.uri()))
            .await
            .unwrap();
        assert_eq!(bytes, vec![0xFF, 0xD8, 0xFF, 0xE0]);

        let requests = server.received_requests().await.unwrap();
        let headers = &requests[0].headers;
        assert!(
            headers.get("cookie").is_none(),
            "the session reached the CDN"
        );
        assert!(headers.get("x-ig-app-id").is_none());
        // The User-Agent does travel: it is on the client, and a mismatched one
        // is what makes a CDN answer differently than the browser would.
        assert_eq!(headers.get("user-agent").unwrap().to_str().unwrap(), UA);
    }

    /// An expired signed URL is a 403 from the CDN. It must not read as a dead
    /// session, which would send someone to log in again for nothing.
    #[tokio::test]
    async fn a_refused_download_is_not_mistaken_for_a_dead_session() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string("expired"))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .download(&format!("{}/pic.jpg", server.uri()))
            .await
            .unwrap_err();

        assert!(matches!(error, IgError::Unexpected { status: 403, .. }));
    }

    /// Something that is not a picture must not be read until memory runs out.
    #[tokio::test]
    async fn a_download_past_the_ceiling_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 64]))
            .mount(&server)
            .await;

        let url = format!("{}/pic.jpg", server.uri());
        let client = client(&server).await;

        let error = client.download_capped(&url, 8).await.unwrap_err();
        assert!(matches!(error, IgError::TooLarge { limit: 8 }));

        // The same body under a ceiling that fits arrives whole.
        assert_eq!(client.download_capped(&url, 64).await.unwrap().len(), 64);
    }

    /// The exception that lets a test serve a picture over plain HTTP used to
    /// match on host alone. Against the real base URL that is
    /// `www.instagram.com`, so it was live in production, and it ran *before*
    /// the https check.
    #[test]
    fn the_test_server_exception_does_not_open_a_hole_in_production() {
        let production = Url::parse(BASE_URL).unwrap();
        for refused in [
            "http://www.instagram.com/pic.jpg",
            "http://www.instagram.com:8080/pic.jpg",
            "https://www.instagram.com:8443/pic.jpg",
        ] {
            assert!(
                !serves_pictures(&production, &Url::parse(refused).unwrap()),
                "{refused} should not be downloadable"
            );
        }
        assert!(serves_pictures(
            &production,
            &Url::parse("https://scontent-mad1-1.cdninstagram.com/v/pic.jpg").unwrap()
        ));
    }

    /// The leading dot is what makes this a suffix rather than a substring.
    #[test]
    fn a_host_that_merely_ends_in_the_cdns_name_is_refused() {
        let production = Url::parse(BASE_URL).unwrap();
        for impostor in [
            "https://evilcdninstagram.com/pic.jpg",
            "https://fbcdn.net.evil.test/pic.jpg",
            "https://cdninstagram.com.evil.test/pic.jpg",
        ] {
            assert!(
                !serves_pictures(&production, &Url::parse(impostor).unwrap()),
                "{impostor} should not be downloadable"
            );
        }
    }

    /// Checking only the address as written left hops two and three judged by
    /// scheme alone, which is a weaker rule than the first hop gets.
    #[tokio::test]
    async fn a_redirect_off_the_cdn_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "https://evil.test/pic.jpg"),
            )
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .download(&format!("{}/pic.jpg", server.uri()))
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::Network(_)), "{error:?}");
    }

    /// `Cookie` is dropped by the HTTP client on a cross-host redirect, but
    /// `X-CSRFToken` is not on its list and would have traveled. Instagram's
    /// API does not redirect off its own host, so refusing costs nothing.
    #[tokio::test]
    async fn an_api_redirect_off_the_origin_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "https://evil.test/collected"),
            )
            .mount(&server)
            .await;

        let error = client(&server).await.validate().await.unwrap_err();
        // `OffOrigin` rather than `Network`: the refusal is this client's now
        // that `get_body` follows the chain itself, and it has to keep landing
        // on `Abort` — see `a_hop_off_the_origin_is_refused_and_not_retried`.
        assert!(matches!(error, IgError::OffOrigin { .. }), "{error:?}");

        // The one request that was made is the one we made on purpose.
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
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
    /// A followed hop is a request, and every request is paid for.
    ///
    /// This is the whole of the change: the hops were followed by reqwest, so
    /// they went out without being charged and without waiting. The budget's
    /// count is what the user is shown and what rate control is built on, so a
    /// chain of two reported one request and sent three.
    #[tokio::test]
    async fn every_redirect_hop_is_charged() {
        let server = MockServer::start().await;
        let body = r#"{"users":[],"next_max_id":null}"#;

        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/1/followers/"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/api/v1/hop-one/"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/hop-one/"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/api/v1/hop-two/"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/hop-two/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let page = client
            .friendships_page(1, "someone", Direction::Followers, 50, None)
            .await
            .expect("the chain ends in an answer");
        assert!(page.users.is_empty());

        assert_eq!(
            client.pacer().spent(),
            3,
            "one request and two hops is three requests, and the budget has to know"
        );
    }

    /// A hop off instagram.com is refused, and refused in a way that stops the
    /// walk rather than making it try again.
    ///
    /// Two of the headers on these requests are credentials. reqwest drops
    /// `Cookie` across hosts but has never heard of `X-CSRFToken`, so a
    /// followed hop would carry it wherever the response pointed.
    ///
    /// The reaction matters as much as the refusal. When this was reqwest's
    /// refusal it arrived as `Network`, whose reaction is `Retry`, and the
    /// pager sent the same impossible request three more times.
    #[tokio::test]
    async fn a_hop_off_the_origin_is_refused_and_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/1/followers/"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "https://example.invalid/collect"),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        let error = client
            .friendships_page(1, "someone", Direction::Followers, 50, None)
            .await
            .expect_err("it must not follow that");

        assert!(
            matches!(&error, IgError::OffOrigin { to } if to.contains("example.invalid")),
            "{error:?}"
        );
        assert_eq!(
            error.reaction(),
            crate::error::Reaction::Abort,
            "retrying a redirect that will be refused again is what cost 13 requests"
        );
        assert_eq!(
            client.pacer().spent(),
            1,
            "the hop was never sent, so it is never charged"
        );
    }

    /// A chain that never ends stops at `MAX_HOPS`, having paid for exactly the
    /// requests it made.
    #[tokio::test]
    async fn a_redirect_loop_stops_and_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/1/followers/"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "/api/v1/friendships/1/followers/"),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        let error = client
            .friendships_page(1, "someone", Direction::Followers, 50, None)
            .await
            .expect_err("a loop is not an answer");

        assert!(matches!(error, IgError::TooManyRedirects), "{error:?}");
        assert_eq!(error.reaction(), crate::error::Reaction::Abort);
        assert_eq!(
            client.pacer().spent(),
            (MAX_HOPS + 1) as u32,
            "the first request and every hop it was allowed"
        );
    }

    /// The query goes on the first request and not on the hops.
    ///
    /// A `Location` carries whatever query it means to carry. Appending ours to
    /// it would send a parameter the server did not ask to see twice, and on an
    /// endpoint that takes a cursor that is a different request from the one
    /// the redirect described.
    #[tokio::test]
    async fn the_query_is_not_reattached_to_a_hop() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/1/followers/"))
            .and(query_param("count", "50"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "/api/v1/landed/?count=7"),
            )
            .mount(&server)
            .await;
        // Mounted with the hop's own count, so it only matches if ours was not
        // added alongside it.
        Mock::given(method("GET"))
            .and(path("/api/v1/landed/"))
            .and(query_param("count", "7"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"users":[],"next_max_id":null}"#),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        client
            .friendships_page(1, "someone", Direction::Followers, 50, None)
            .await
            .expect("the hop's own query is the one that travels");
    }

    /// A redirect with no `Location` is an answer nobody can act on, and it must
    /// not become a silent success or a retry.
    #[tokio::test]
    async fn a_redirect_with_nowhere_to_go_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/1/followers/"))
            .respond_with(ResponseTemplate::new(302))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let error = client
            .friendships_page(1, "someone", Direction::Followers, 50, None)
            .await
            .expect_err("there is nowhere to go");
        assert!(
            matches!(error, IgError::Unexpected { status: 302, .. }),
            "{error:?}"
        );
    }
    /// A stop during a request in flight does not wait for the server.
    ///
    /// This is the tenth interrupt in ten. The other nine land in a budget wait
    /// and take about a second; this one used to track however long the far end
    /// chose to hold the connection — up to `REQUEST_TIMEOUT`, or 254 seconds
    /// on a black-holed connection, because `Network` is retried. Under a
    /// service manager that outlasts the stop grace period and the process is
    /// killed before it can close its snapshot.
    ///
    /// The delay here is thirty seconds so that a passing run cannot be one
    /// that simply waited it out: the assertion is that the call came back in a
    /// fraction of it.
    #[tokio::test]
    async fn canceling_does_not_wait_for_the_server() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(30))
                    .set_body_string(r#"{"users":[],"next_max_id":null}"#),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        let token = client.pacer().cancel_token().clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token.cancel();
        });

        let started = std::time::Instant::now();
        let error = client
            .friendships_page(1, "someone", Direction::Followers, 50, None)
            .await
            .expect_err("the run was canceled");

        // `Canceled`, not `Network`. Letting the dropped request become a
        // network error would give it `Reaction::Retry`, so the pager would
        // answer a Ctrl+C by sending the request three more times.
        assert!(matches!(error, IgError::Canceled), "{error:?}");
        assert_eq!(error.reaction(), crate::error::Reaction::Abort);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "it waited {:?}, which is the server's patience rather than the user's",
            started.elapsed()
        );
    }

    /// A stop is the user's answer, not Instagram's, and nothing is written
    /// down as though it were.
    ///
    /// The endpoint here would classify as a push-back and earn a cooldown if
    /// its answer were ever read. Cancellation has to win first, and win
    /// without the interrupted request leaving a mark: a cooldown recorded
    /// because somebody pressed Ctrl+C would refuse the next run for half an
    /// hour over something Instagram never said.
    #[tokio::test]
    async fn canceling_records_no_cooldown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_delay(std::time::Duration::from_secs(30))
                    .set_body_string(r#"{"message":"feedback_required"}"#),
            )
            .mount(&server)
            .await;

        let budget = Arc::new(Recording::default());
        let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        let client = IgClient::new(session, crate::pace::Pacer::new(budget.clone()))
            .unwrap()
            .with_base_url(Url::parse(&server.uri()).unwrap());

        let token = client.pacer().cancel_token().clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token.cancel();
        });

        let error = client.validate().await.expect_err("the run was canceled");
        assert!(matches!(error, IgError::Canceled), "{error:?}");
        assert!(
            budget.calls().is_empty(),
            "a canceled request was recorded as a push-back: {:?}",
            budget.calls()
        );
    }

    /// Every push-back says what `Retry-After` it carried, and nothing acts on
    /// it.
    ///
    /// `classify` takes a status and a body and never sees a header, so the
    /// question of whether these endpoints send this at all has never been
    /// answerable from a real run. It is now. What must **not** happen is the
    /// header changing anything before somebody has seen one: the cooldown
    /// recorded here is the same one that was recorded before, and a
    /// server-named thirty seconds must never shorten it.
    #[tokio::test]
    async fn a_push_back_says_what_retry_after_it_carried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "30")
                    // What Instagram volunteers about its own load on every
                    // API answer. Carried for the same reason and decided on
                    // for neither: see `note_push_back`.
                    .insert_header("x-ig-capacity-level", "2")
                    .insert_header("x-ig-peak-time", "1")
                    .set_body_string(r#"{"message":"Please wait a few minutes"}"#),
            )
            .mount(&server)
            .await;

        let budget = Arc::new(Recording::default());
        let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        let client = IgClient::new(session, crate::pace::Pacer::new(budget.clone()))
            .unwrap()
            .with_base_url(Url::parse(&server.uri()).unwrap());

        // **The header on the answer, not the line in the log.**
        //
        // This used to install a capturing subscriber and read the debug line
        // back, and it failed about one run in three once the suite grew: a
        // subscriber is thread-local, an async path is polled wherever the
        // runtime likes, and `WithSubscriber` did not close the gap either.
        // What the test is really about is that the header is read off the
        // response and carried, which `Answer` holds — so it is asserted there,
        // where no scheduler can move it. The logging is one line over this
        // value and does not need its own test.
        let answer = client
            .get_body(
                &format!(
                    "/api/v1/friendships/{}/following/",
                    client.session.ds_user_id
                ),
                &[("count", "1")],
                "",
                Surface::App,
            )
            .await
            .expect("a 429 is an answer, not a transport failure");
        assert_eq!(answer.status, 429);
        assert_eq!(answer.retry_after.as_deref(), Some("30"));
        assert_eq!(
            answer.load.as_deref(),
            Some("x-ig-capacity-level=2 x-ig-peak-time=1"),
            "nobody has ever recorded the load Instagram announced while refusing"
        );

        // And the cooldown is untouched by either of them. Thirty seconds is far shorter
        // than the rate-limit cooldown, so a header that had been allowed to
        // shorten anything would show up right here.
        let error = client.validate().await.unwrap_err();
        assert!(matches!(error, IgError::RateLimited), "{error:?}");
        let recorded = budget.calls();
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert_eq!(recorded[0].0, "rate_limit");
        assert_eq!(
            recorded[0].1,
            snob_core::budget::rate_limit_cooldown(),
            "the server's number reached the cooldown, and it must not"
        );
    }

    /// A push-back with no such header still says so, which is the answer the
    /// logging is really after: these endpoints may simply never send one.
    ///
    /// A 200 carrying `spam: true` is a push-back, and one that a check on the
    /// status alone would have walked straight past.
    #[tokio::test]
    async fn a_push_back_without_the_header_is_recorded_as_absent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"status":"fail","spam":true}"#),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        let answer = client
            .get_body("/api/v1/friendships/42/following/", &[], "", Surface::App)
            .await
            .expect("a 200 is an answer");
        assert_eq!(answer.status, 200);
        assert!(
            answer.retry_after.is_none(),
            "nothing sent one, so nothing may be invented"
        );

        let error = client.validate().await.unwrap_err();
        assert!(matches!(error, IgError::RateLimited), "{error:?}");
    }

    /// The body Instagram really sends for the accounts this fallback exists
    /// for. Captured live in August 2026.
    const BROKEN_PROFILE: &str = r#"{"message":"Asset asset://laser.provider/ig_business_category_subvertical has been deleted. You cannot use this schema","status":"fail"}"#;

    /// One search hit, shaped like the live answer: `pk` as a string, no
    /// counters anywhere, and the relationship under `friendship_status`.
    fn search_body(username: &str, pk: &str) -> String {
        format!(
            r#"{{"users":[{{"position":0,"user":{{"pk":"{pk}","username":"{username}",
               "full_name":"Someone","is_private":false,"is_verified":true,
               "profile_pic_url":"https://cdninstagram.com/p.jpg",
               "friendship_status":{{"following":true,"outgoing_request":false,
               "is_private":false}}}}}}],"status":"ok"}}"#
        )
    }

    fn mount_profile(server: &MockServer, response: ResponseTemplate) -> impl Future<Output = ()> {
        Mock::given(method("GET"))
            .and(path("/api/v1/users/web_profile_info/"))
            .respond_with(response)
            .mount(server)
    }

    /// The ordinary account is untouched: one request, the full answer, and the
    /// route it came from says so.
    ///
    /// This is the half that is easy to break while fixing the other one. The
    /// fallback must not cost anybody who does not need it a second request, so
    /// the charge is asserted rather than assumed.
    #[tokio::test]
    async fn an_ordinary_profile_still_costs_one_request() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(200).set_body_string(
                r#"{"data":{"user":{"id":"7","username":"ann",
                   "edge_followed_by":{"count":10},"edge_follow":{"count":4}}}}"#,
            ),
        )
        .await;

        let client = client(&server).await;
        let profile = client.web_profile_info("ann").await.unwrap();

        assert_eq!(profile.id, 7);
        assert_eq!(profile.via, crate::model::Via::Profile);
        assert!(profile.counters_are_knowable());
        assert_eq!(profile.follower_count(), Some(10));
        assert_eq!(client.pacer().spent(), 1, "the fallback was not needed");

        let asked = server.received_requests().await.unwrap();
        assert_eq!(asked.len(), 1, "search was reached on a working account");
    }

    /// Instagram failing to serialize its own reply does not take the account
    /// down with it.
    ///
    /// The 400 here is verbatim from the live API. Every command that names an
    /// account starts by turning the name into an id, so without the fallback
    /// this one body stops `pfp`, `scan` and every set command.
    #[tokio::test]
    async fn a_profile_instagram_cannot_serialize_is_resolved_by_search() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(400).set_body_string(BROKEN_PROFILE),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/web/search/topsearch/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_body("rubius", "1506")))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let profile = client.web_profile_info("rubius").await.unwrap();

        assert_eq!(profile.id, 1506);
        assert_eq!(profile.username, "rubius");
        assert_eq!(profile.via, crate::model::Via::Search);

        // The two facts the private-account refusal turns on survive the
        // change of route, under different names.
        assert_eq!(profile.followed_by_viewer, Some(true));
        assert_eq!(profile.requested_by_viewer, Some(false));

        // And the counters do not. **`None`, never `Some(0)`**: a declared zero
        // is what would make `pager::verify_completion` call every short walk
        // complete.
        assert!(!profile.counters_are_knowable());
        assert_eq!(profile.follower_count(), None);
        assert_eq!(profile.following_count(), None);

        assert_eq!(client.pacer().spent(), 2, "the failure and the fallback");
    }

    /// A push-back is never worked around.
    ///
    /// This is the rule the whole fallback is written around: when a service
    /// says no, the answer is to stop asking. A 429 already carries a cooldown
    /// by the time the fallback would be considered, and sending a second
    /// request into an endpoint that has just refused is exactly how a
    /// momentary limit becomes a lasting one.
    #[tokio::test]
    async fn a_push_back_is_not_worked_around() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(429).set_body_string(r#"{"message":"feedback_required"}"#),
        )
        .await;

        let budget = Arc::new(Recording::default());
        let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        let client = IgClient::new(session, crate::pace::Pacer::new(budget.clone()))
            .unwrap()
            .with_base_url(Url::parse(&server.uri()).unwrap());

        let error = client.web_profile_info("ann").await.unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(
            client.pacer().spent(),
            1,
            "a second request was sent anyway"
        );

        let asked = server.received_requests().await.unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(budget.calls().len(), 1, "the cooldown still gets recorded");
    }

    /// Every answer that means "no" means no, and a 404 is a real answer.
    ///
    /// Asking search about a name nobody owns spends a request to be told the
    /// same thing, and the two session errors and the cancel must not be
    /// retried at all.
    #[test]
    fn only_a_broken_answer_earns_a_second_route() {
        assert!(
            IgError::Unexpected {
                status: 400,
                body: BROKEN_PROFILE.into()
            }
            .worth_a_second_route()
        );

        for refused in [
            IgError::RateLimited,
            IgError::FeedbackRequired,
            IgError::Challenge { url: None },
            IgError::Checkpoint { url: None },
            IgError::SessionExpired,
            IgError::UserAgentMismatch,
            IgError::Canceled,
            IgError::NotFound { what: None },
            IgError::Decode("a captive portal".into()),
            // The server being unwell is what `Reaction::Retry` is for, and a
            // second route there would hide an outage behind a worse answer.
            IgError::Unexpected {
                status: 503,
                body: String::new(),
            },
        ] {
            assert!(
                !refused.worth_a_second_route(),
                "{refused:?} would be worked around"
            );
        }
    }

    /// A second mutation is sent only after an answer that says the first
    /// did not happen.
    ///
    /// The ambiguous ones are the point: a 502 from an edge that may already
    /// have forwarded the write, the redirect `post` refuses to follow, and a
    /// 200 this program could not read. Each used to earn a discovery walk
    /// and then a second `mutate` on the identifier it found.
    #[test]
    fn a_write_is_rediscovered_only_after_a_refusal_that_says_it_did_not_happen() {
        let unexpected = |status: u16| IgError::Unexpected {
            status,
            body: String::new(),
        };
        for refused in [
            unexpected(400),
            unexpected(200),
            IgError::NotFound { what: None },
        ] {
            assert!(worth_rediscovering(&refused), "{refused:?}");
        }
        for ambiguous in [
            unexpected(502),
            unexpected(302),
            unexpected(0),
            IgError::Decode("an answer that would not parse".into()),
            IgError::RateLimited,
            IgError::FeedbackRequired,
            IgError::SessionExpired,
            IgError::Canceled,
            IgError::NoCsrfToken,
        ] {
            assert!(
                !worth_rediscovering(&ambiguous),
                "{ambiguous:?} would have the write sent twice"
            );
        }
    }

    /// Search matches loosely, and a loose match is a different account.
    ///
    /// This is the failure the fallback could introduce and the reason the
    /// exact-name comparison is in `search_user_id` rather than left to a
    /// caller: without it, asking about a name that Instagram would not serve
    /// hands back whatever the search box suggested instead, and the run then
    /// walks a stranger's followers under the name that was typed.
    #[tokio::test]
    async fn search_does_not_hand_back_somebody_else() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(400).set_body_string(BROKEN_PROFILE),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/web/search/topsearch/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(search_body("rubius_fanpage", "999")),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        let error = client.web_profile_info("rubius").await.unwrap_err();

        // The original failure, not a wrong account and not a "no such
        // account": search not finding it is no evidence the name is free.
        assert!(
            matches!(&error, IgError::Unexpected { status: 400, .. }),
            "{error:?}"
        );
    }

    /// Case is not identity, but it is not a different account either.
    #[tokio::test]
    async fn search_matches_the_name_whatever_its_case() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(400).set_body_string(BROKEN_PROFILE),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/web/search/topsearch/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_body("Rubius", "12")))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let profile = client.web_profile_info("rubius").await.unwrap();
        assert_eq!(profile.id, 12);
    }

    /// When the fallback is the one that hits the wall, the wall is what gets
    /// reported.
    ///
    /// A cooldown recorded on the second request is a fact about the account
    /// that the user has to act on, and letting the first failure stand would
    /// bury it under a message about serialization.
    #[tokio::test]
    async fn a_cooldown_on_the_fallback_is_not_swallowed() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(400).set_body_string(BROKEN_PROFILE),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/web/search/topsearch/"))
            .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"message":"spam"}"#))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let error = client.web_profile_info("rubius").await.unwrap_err();
        assert!(matches!(error, IgError::RateLimited), "{error:?}");
    }

    /// A session with a CSRF token, which is what `login --browser` produces
    /// and what the write path requires.
    async fn writer(server: &MockServer) -> IgClient {
        let mut session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        session.csrftoken = Some("TOKEN".into());
        IgClient::new(session, crate::pace::Pacer::unlimited())
            .unwrap()
            .with_base_url(Url::parse(&server.uri()).unwrap())
    }

    /// A page that hands out both tokens, which is what a logged-in one does.
    const LOGGED_IN_PAGE: &str = r#"<html><script>
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
    struct Known;

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
    async fn instagram_that_takes_a_write(body: &str) -> MockServer {
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
    const FOLLOWED: &str = r#"{"result":"following","status":"ok"}"#;

    /// **What the mutation really answers**, captured live. Everything else in
    /// this file was right and this was the last thing wrong: the follow
    /// happened and the command said it had not, because the envelope was not
    /// one of the shapes being read.
    const FOLLOWED_GRAPHQL: &str = r#"{"data":{"xdt_create_friendship":{"friendship_status":{"following":true,"outgoing_request":false}}}}"#;

    /// The mobile shape, which this endpoint does not send and the client reads
    /// anyway. Instagram has been moving the web client onto the `/api/v1/`
    /// routes everywhere else, and the day it moves this one the answer changes
    /// shape without changing meaning.
    const FOLLOWED_OBJECT: &str =
        r#"{"status":"ok","friendship_status":{"following":true,"outgoing_request":false}}"#;

    /// **The request goes to `/api/graphql` and names the mutation**, which is
    /// what the real client does and what three live attempts established — see
    /// [`crate::graphql`] for the two that were sent first and changed nothing.
    #[tokio::test]
    async fn a_follow_names_the_mutation_with_the_id_it_was_given() {
        let server = instagram_that_takes_a_write(FOLLOWED).await;

        let status = writer(&server)
            .await
            .follow(7, "someone", &Known)
            .await
            .unwrap();
        assert!(status.following);

        let requests = server.received_requests().await.unwrap();
        let write = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::POST)
            .expect("the mutation");
        assert_eq!(write.url.path(), "/api/graphql");

        let body = String::from_utf8_lossy(&write.body);
        assert!(
            body.contains("fb_api_req_friendly_name=usePolarisFollowMutation"),
            "{body}"
        );
        assert!(body.contains("doc_id=26508036048874888"), "{body}");
        assert!(body.contains("fb_dtsg=DTSG-TOKEN"), "{body}");
        assert!(body.contains("lsd=LSD-TOKEN"), "{body}");
        // The account, percent-encoded inside the variables object.
        assert!(body.contains("target_user_id"), "{body}");
    }

    /// **A page and a mutation, and both are paid for.** The page is the
    /// expensive half and it is why there is no bulk mode to be tempted by.
    #[tokio::test]
    async fn a_write_costs_a_page_and_a_mutation() {
        let server = instagram_that_takes_a_write(FOLLOWED).await;
        writer(&server)
            .await
            .follow(7, "someone", &Known)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "{requests:?}");
        // And the page asked for is the target's, which is where a browser
        // would have been standing.
        let page = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::GET)
            .expect("the page");
        assert_eq!(page.url.path(), "/someone/");
    }

    /// A page with no tokens is a logged-out page, and saying so beats sending
    /// a mutation that cannot be authorized and reading whatever comes back.
    #[tokio::test]
    async fn a_logged_out_page_is_reported_as_a_dead_session() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>log in</html>"))
            .mount(&server)
            .await;

        let error = writer(&server)
            .await
            .follow(7, "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::SessionExpired), "{error:?}");
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method == wiremock::http::Method::GET),
            "nothing may be sent without the tokens to authorize it"
        );
    }

    /// Either answer shape means the same thing to the caller.
    #[tokio::test]
    async fn both_answer_shapes_read_the_same() {
        for body in [FOLLOWED, FOLLOWED_OBJECT, FOLLOWED_GRAPHQL] {
            let server = instagram_that_takes_a_write(body).await;
            let status = writer(&server)
                .await
                .follow(7, "someone", &Known)
                .await
                .unwrap();
            assert!(status.following, "{body}");
            assert!(!status.outgoing_request, "{body}");
        }
    }

    /// **A navigation is not an XHR, and the headers say so.**
    ///
    /// The page that hands out the tokens is fetched the way a browser fetches
    /// a page: asking for HTML, saying `navigate`/`document`, and announcing
    /// none of the four headers that mean "I am the single-page app" — because
    /// at that moment there is no app yet, the page is what loads it. The API
    /// requests must keep all four, so both halves are asserted together; a
    /// change that made one set serve both would pass on its own and be a shape
    /// no browser produces on either.
    #[tokio::test]
    async fn a_navigation_does_not_claim_to_be_the_app() {
        // The last two were applied to every request that could carry them,
        // which put both on the one navigation this program makes. A browser
        // going to `instagram.com/nasa/` sends neither: `X-CSRFToken` is what a
        // running page adds to its own XHRs, and `Priority` on a document is
        // `u=0`, not the default fetch urgency.
        const APP_ONLY: [&str; 5] = [
            "x-ig-app-id",
            "x-asbd-id",
            "x-ig-www-claim",
            "x-requested-with",
            "x-csrftoken",
        ];

        let server = instagram_that_takes_a_write(FOLLOWED).await;
        writer(&server)
            .await
            .follow(7, "someone", &Known)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let page = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::GET)
            .expect("the page");
        for header in APP_ONLY {
            assert!(
                !page.headers.contains_key(header),
                "a navigation carried {header}"
            );
        }
        assert_eq!(page.headers.get("sec-fetch-mode").unwrap(), "navigate");
        assert_eq!(page.headers.get("sec-fetch-dest").unwrap(), "document");
        // **The whole value, not its first word.** Asserting `starts_with` is
        // what let a seventeen-space run sit in the middle of this header and
        // go out on the wire before every write: the test was standing next to
        // the defect and could not see it. The App surface's `Accept` was
        // already pinned to the byte; this one was not.
        assert_eq!(
            page.headers.get("accept").unwrap(),
            concat!(
                "text/html,application/xhtml+xml,application/xml;q=0.9,",
                "image/avif,image/webp,image/apng,*/*;q=0.8,",
                "application/signed-exchange;v=b3;q=0.7"
            ),
            "a navigation asks for what Chrome asks for"
        );
        assert_eq!(
            page.headers.get("priority").unwrap(),
            client_hints::NAVIGATION_PRIORITY,
            "a document is the most urgent thing on the page, not a default fetch"
        );

        // And the mutation, which is the other half of the same distinction.
        let write = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::POST)
            .expect("the mutation");
        assert_eq!(
            write.headers.get("x-fb-friendly-name").unwrap(),
            "usePolarisFollowMutation",
            "a Relay request names its operation in the headers as well as the body"
        );
        assert!(
            write.headers.contains_key("x-fb-lsd"),
            "a Relay request carries the page token it was given"
        );
        assert!(
            write.headers.contains_key("x-csrftoken"),
            "a write still carries the token the write rule requires"
        );
        // **And does not wear the XHR's clothes.** Measured, not reasoned:
        // across 171 `/api/graphql` requests in a real session, neither of
        // these appeared once, while both appeared on all 97 `/api/v1/` ones.
        for xhr_only in ["x-requested-with", "x-ig-www-claim"] {
            assert!(
                !write.headers.contains_key(xhr_only),
                "a Relay request carried {xhr_only}, which Relay never sends"
            );
        }
        assert!(
            write.headers.contains_key("x-ig-app-id"),
            "the app id is on every surface that has an app behind it"
        );
        assert_eq!(
            write.headers.get("priority").unwrap(),
            client_hints::FETCH_PRIORITY
        );
    }

    /// A private account answers `requested`, and that is not a follow.
    #[tokio::test]
    async fn a_private_account_answers_that_it_was_asked() {
        let server = instagram_that_takes_a_write(r#"{"result":"requested","status":"ok"}"#).await;

        let status = writer(&server)
            .await
            .follow(7, "someone", &Known)
            .await
            .unwrap();
        assert!(status.outgoing_request);
        assert!(!status.following, "a request is not a follow");
    }

    /// The three headers a POST carries that a GET does not, plus the CSRF
    /// token, which a GET carries too but a write cannot go without.
    #[tokio::test]
    async fn a_write_carries_origin_a_content_type_and_the_csrf_token() {
        let server = instagram_that_takes_a_write(FOLLOWED).await;
        writer(&server)
            .await
            .follow(7, "someone", &Known)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let headers = &requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::POST)
            .expect("the mutation")
            .headers;
        assert_eq!(
            headers.get("origin").unwrap(),
            server.uri().trim_end_matches('/'),
            "a same-origin POST carries Origin, unlike a same-origin GET"
        );
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/x-www-form-urlencoded"
        );
        assert_eq!(headers.get("x-csrftoken").unwrap(), "TOKEN");
        assert!(headers.contains_key("cookie"));
        // And it does announce itself as the app, because `/api/graphql` is
        // the app's route. The surface that does not is the page fetch, and
        // `a_navigation_does_not_claim_to_be_the_app` next door asserts the
        // two against each other.
        assert!(headers.contains_key("x-ig-app-id"));
        assert!(headers.contains_key("user-agent"));
        assert!(headers.contains_key("accept-language"));
    }

    /// **Nothing is sent at all** when the session cannot sign the request.
    /// Discovering it from Instagram's 403 would cost a request, a slot of the
    /// write budget, and a message telling the user their session had expired
    /// when it had not.
    #[tokio::test]
    async fn a_session_without_a_csrf_token_sends_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FOLLOWED))
            .mount(&server)
            .await;

        // `client` builds a pasted session, which has no token.
        let error = client(&server)
            .await
            .follow(7, "someone", &graphql::NoDocIds)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::NoCsrfToken), "{error:?}");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "the refusal must happen before anything goes out"
        );
    }

    /// A redirect on a write is refused rather than followed. Following one
    /// would mean asking Instagram to do the thing a second time, and reqwest
    /// repeats the method and the body on a 307 or a 308.
    #[tokio::test]
    async fn a_redirected_write_is_not_replayed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", "/api/graphql"))
            .mount(&server)
            .await;

        let error = writer(&server)
            .await
            .unfollow(7, "someone", &Known)
            .await
            .unwrap_err();
        assert!(
            matches!(error, IgError::Unexpected { status: 307, .. }),
            "{error:?}"
        );
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.method == wiremock::http::Method::POST)
                .count(),
            1,
            "the write must have been sent exactly once"
        );
    }

    /// An action block on a write earns a cooldown, and the budget is asked to
    /// remember it. Driven through `Recording`, which does remember, rather
    /// than through `Pacer::unlimited`, which answers `Ok(0)` and forgets.
    #[tokio::test]
    async fn an_action_block_on_a_write_is_recorded() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            // No `spam` field: that one short-circuits to `RateLimited` before
            // the message is read, and what is under test here is the action
            // block, which carries the longer cooldown.
            .and(path("/api/graphql"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"message":"feedback_required","status":"fail"}"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;

        let (client, budget) = watching_as(&server.uri(), true);

        let error = client.follow(7, "someone", &Known).await.unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(budget.calls().len(), 1, "the cooldown was not written down");
    }

    /// **The refusal that was being read as success.**
    ///
    /// This is the shape a real block arrives in, and it is the one the earlier
    /// test above could not have caught: HTTP 200, no `status`, no `message`,
    /// the reason inside an `errors` array. Every field of `FriendshipResult`
    /// is optional, so before `declares_failure` learned this envelope the body
    /// deserialized cleanly with all of them absent, `status()` fell to its
    /// default arm, and the command reported "Instagram accepted it, but
    /// nothing changed" -- while the write budget had been spent and no
    /// cooldown had been written down.
    #[tokio::test]
    async fn a_graphql_refusal_under_a_200_is_an_action_block_and_not_a_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":null,"errors":[{"message":"feedback_required",
                    "summary":"Try Again Later",
                    "description":"We restrict certain activity to protect our community."}]}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;

        let (client, budget) = watching_as(&server.uri(), true);

        let error = client.follow(7, "someone", &Known).await.unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(budget.calls().len(), 1, "the cooldown was not written down");
    }

    /// The reason may be in any of the three fields, and which one is not ours
    /// to choose. `summary` alone is enough.
    #[tokio::test]
    async fn a_graphql_reason_is_found_wherever_instagram_put_it() {
        for body in [
            r#"{"errors":[{"message":"checkpoint_required"}]}"#,
            r#"{"errors":[{"summary":"checkpoint_required"}]}"#,
            r#"{"errors":[{"description":"checkpoint_required"}]}"#,
        ] {
            assert!(declares_failure(body), "not seen as a failure: {body}");
            assert!(
                matches!(
                    crate::error::classify(200, body),
                    IgError::Checkpoint { .. }
                ),
                "not classified from: {body}"
            );
        }
    }

    /// An empty `errors` array is what a *successful* GraphQL answer may carry.
    /// Reading it as a refusal would fail every write that worked.
    #[tokio::test]
    async fn an_empty_errors_array_is_not_a_refusal() {
        assert!(!declares_failure(r#"{"data":{"x":1},"errors":[]}"#));
    }

    /// **The write path measures its own push-backs now.**
    ///
    /// It was the one class of request that never did: `note_push_back` was
    /// called from `get` and from `get_body`, never from `post`, and `post` did
    /// not so much as read the header. So the endpoint most likely to say
    /// something worth hearing was the one nobody was listening to.
    #[tokio::test]
    async fn a_write_keeps_the_retry_after_it_was_given() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "30")
                    .set_body_string(r#"{"message":"please wait a few minutes"}"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;

        let (client, _) = watching_as(&server.uri(), true);
        let error = client.follow(7, "someone", &Known).await.unwrap_err();
        assert!(matches!(error, IgError::RateLimited), "{error:?}");
    }

    /// Both envelope shapes carry the same reel, and the caller cannot tell
    /// which arrived. Reading only the one seen during development is how this
    /// breaks quietly when Instagram switches.
    #[tokio::test]
    async fn stories_are_read_out_of_either_envelope() {
        let item = r#"{"pk":"1","media_type":1,"taken_at":100,"expiring_at":200,
            "image_versions2":{"candidates":[{"url":"https://x/s.jpg","width":640,"height":1136},
            {"url":"https://x/b.jpg","width":1080,"height":1920}]}}"#;

        for envelope in [
            format!(r#"{{"reels_media":[{{"items":[{item}]}}]}}"#),
            format!(r#"{{"reels":{{"42":{{"items":[{item}]}}}}}}"#),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/feed/reels_media/"))
                .and(query_param("reel_ids", "42"))
                .respond_with(ResponseTemplate::new(200).set_body_string(&envelope))
                .mount(&server)
                .await;

            let reel = client(&server)
                .await
                .stories(42, "someone")
                .await
                .unwrap()
                .expect("a reel");
            assert_eq!(reel.items.len(), 1);
            assert_eq!(
                crate::model::largest(&reel.items[0].image_versions2.clone().unwrap().candidates)
                    .unwrap()
                    .url,
                "https://x/b.jpg",
                "the biggest candidate wins, not the first"
            );
        }
    }

    /// An account with nothing up is not a missing account.
    #[tokio::test]
    async fn an_account_with_no_stories_is_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/feed/reels_media/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"reels_media":[]}"#))
            .mount(&server)
            .await;

        assert!(
            client(&server)
                .await
                .stories(42, "someone")
                .await
                .unwrap()
                .is_none()
        );
    }
}
