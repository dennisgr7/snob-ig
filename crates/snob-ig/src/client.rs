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
use crate::model::{
    FriendshipResult, FriendshipStatus, FriendshipsPage, Identity, Reel, ReelsMedia, UserInfo,
    UserInfoEnvelope, WebProfileInfo, WebProfileInfoEnvelope,
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

/// Redirects for the API: the same origin, or nowhere.
///
/// Instagram's JSON endpoints do not redirect off their own host, so refusing
/// costs nothing — and two of the headers on those requests are credentials.
/// reqwest drops `Cookie` when a redirect crosses hosts, but `X-CSRFToken` is
/// not on the list it knows about and would travel to wherever the response
/// pointed. A boundary that depends on somebody else's list of header names is
/// not one, so this one is drawn here.
fn api_policy(base: Url) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_HOPS {
            attempt.error("too many redirects")
        } else if same_origin(&base, attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("a redirect tried to take an API call off instagram.com")
        }
    })
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
    Ok(crate::http::builder(user_agent, redirect, CONNECT_TIMEOUT, REQUEST_TIMEOUT).build()?)
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

/// Which of the two things a browser does on instagram.com a request is.
///
/// **They do not carry the same headers, and sending the wrong set is not a
/// cosmetic mismatch — it changes which handler answers.** This was found the
/// hard way: `POST /web/friendships/{pk}/follow/` with the app's headers
/// answers 404, and the same route is what the browser really uses.
///
/// - [`Surface::App`] is the single-page application talking to `/api/v1/`.
///   It announces itself with `X-IG-App-ID`, `X-ASBD-ID`, `X-IG-WWW-Claim` and
///   `X-Requested-With`, and without the first of those those routes answer 403
///   even with a good session. Every read this tool makes is one of these.
/// - [`Surface::Page`] is an ordinary `fetch()` from the page, to the older
///   `/web/` routes. It sends **none** of those four: the browser adds only
///   what it always adds, plus whatever the call asked for. Reference:
///   `davidarroyo1234/InstagramUnfollowers`, which is the project this tool's
///   pacing is copied from and which does this every day —
///   `fetch(url, { headers: { "content-type": ..., "x-csrftoken": ... } })`
///   and nothing else.
///
/// So "coherence" here means per request rather than per client. Sending one
/// superset of headers everywhere would be a shape no browser produces on
/// either route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    App,
    Page,
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
            api: build_client(&session.user_agent, api_policy(base.clone()))?,
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
    /// the high-resolution picture. One request.
    pub async fn web_profile_info(&self, username: &str) -> Result<WebProfileInfo, IgError> {
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
    pub async fn follow(&self, pk: Pk, username: &str) -> Result<FriendshipStatus, IgError> {
        self.friendship("follow", pk, username).await
    }

    /// Unfollows an account. **A write.** See [`IgClient::post`].
    pub async fn unfollow(&self, pk: Pk, username: &str) -> Result<FriendshipStatus, IgError> {
        self.friendship("unfollow", pk, username).await
    }

    /// The body of both, because the two differ by one word in the path.
    ///
    /// # Which route, and why it took three attempts to find out
    ///
    /// `POST /web/friendships/{pk}/{follow|unfollow}/`, with **no body** and
    /// with [`Surface::Page`] headers. Both halves of that were established by
    /// sending the alternatives live in August 2026 and reading the
    /// relationship back afterwards:
    ///
    /// - `POST /api/v1/friendships/create/{pk}/` — the spelling every write-up
    ///   of this API gives — answers **200 carrying the web app's HTML shell**.
    ///   No status to classify, no message to read, and a parse failure as the
    ///   only symptom. That is the mobile app's route; `www.instagram.com` does
    ///   not serve it.
    /// - The same `/web/` route below, sent with [`Surface::App`] headers,
    ///   answers **404**. The route was right and the headers were wrong, which
    ///   is the failure that looks most like the route being gone — and is why
    ///   the first reading of that 404 was that it had been removed.
    ///
    /// What settled it is `davidarroyo1234/InstagramUnfollowers`, the project
    /// this tool's pacing is copied from, which unfollows from inside the page
    /// with `content-type` and `x-csrftoken` and **nothing else**. No
    /// `X-IG-App-ID`. That is what [`Surface`] exists to express.
    ///
    /// The body is empty, which is what that project sends and what the route
    /// takes. The `container_module`/`nav_chain`/`user_id` triple belongs to
    /// the `/api/v1/` request and is not sent here: it would be inventing a
    /// shape no browser produces.
    async fn friendship(
        &self,
        verb: &str,
        pk: Pk,
        username: &str,
    ) -> Result<FriendshipStatus, IgError> {
        let answer: FriendshipResult = self
            .post(
                &format!("/web/friendships/{pk}/{verb}/"),
                &[],
                &if username.is_empty() {
                    String::new()
                } else {
                    format!("{}/", snob_core::model::in_a_path(username))
                },
                Surface::Page,
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
            .cdn()?
            .get(url)
            .header("Accept-Encoding", self.hints.accept_encoding)
            .send()
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

        read_capped_bytes(response, cap as u64).await
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
        let url = self.base.join(path)?;
        tracing::debug!(%url, "GET");

        // Paid for before it is sent, and there is no way in that skips this.
        self.pacer.clear_to_send().await?;

        let request = self.browser_headers(self.api.get(url).query(query), referer, Surface::App);

        let response = request.send().await?;
        self.remember_claim(&response);

        let status = response.status();

        // The status is already in hand, and a body that will not read must not
        // take it away. With `?` here, a 429 whose body died mid-stream became
        // `IgError::Network` — whose reaction is `Retry` — so the walker fired
        // three more requests into an endpoint that had just said no, and
        // `classify_and_record`, the only caller of `cooldown_for` there is,
        // never ran: nothing was written down and the next run knocked again.
        // What Instagram said is the status; the body only refines it.
        let body = match read_capped(response, MAX_BODY_BYTES).await {
            Ok(body) => body,
            Err(_) if !status.is_success() => {
                return Err(self.classify_and_record(status.as_u16(), ""));
            }
            Err(e) => return Err(e),
        };

        self.decode(status, &body)
    }

    /// The headers every request to Instagram carries, in one place.
    ///
    /// Factored out when the write path arrived. Two copies of this list is how
    /// the coherence `client_hints.rs` exists to maintain gets lost: a header
    /// added for a good reason on the read path and forgotten on the write path
    /// makes the two requests look like they came from different clients, on
    /// one session, which is the anomaly and not the fix.
    ///
    /// What is deliberately **not** here is `Origin` and `Content-Type`. Both
    /// belong to the write path only, and both are added there — see
    /// [`IgClient::post`].
    ///
    /// `style` picks which of the two things a browser is doing on
    /// instagram.com this request is. See [`Surface`].
    fn browser_headers(
        &self,
        request: reqwest::RequestBuilder,
        referer: &str,
        style: Surface,
    ) -> reqwest::RequestBuilder {
        let mut request = request;

        if style == Surface::App {
            request = request
                // Without this header Instagram answers 403 even with a good
                // session — on the `/api/v1/` routes. On the `/web/` ones it is
                // the header that makes the request fail.
                .header("X-IG-App-ID", IG_APP_ID)
                .header("X-ASBD-ID", client_hints::ASBD_ID)
                .header("X-IG-WWW-Claim", self.claim())
                .header("X-Requested-With", "XMLHttpRequest");
        }

        let mut request = request
            // `*/*`, not `application/json`: that is what `fetch()` sends when
            // the page does not set one, and no browser sends the latter here.
            .header("Accept", "*/*")
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
            // Instagram answers `Vary` on the first two, which is it saying
            // its reply depends on them.
            .header("Sec-Fetch-Site", "same-origin")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Dest", "empty")
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
        // The other half of the same rule, and the reason this comment now says
        // "here": the standard requires `Origin` on a POST even when it is
        // same-origin. Omitting it there would be the identical incoherence the
        // other way round, so `post` adds it, and only `post`.

        if let Some(csrf) = &self.session.csrftoken {
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

        // Likewise: only the versions that send one.
        if let Some(priority) = self.hints.priority {
            request = request.header("Priority", priority);
        }

        request
    }

    /// Turns what Instagram said into either the value asked for or an error,
    /// recording a cooldown on the way if the answer earned one.
    ///
    /// Shared by the read and the write path so that "a 200 can still be a
    /// failure" is one rule rather than two. It was inline in `get` when `get`
    /// was the only caller.
    fn decode<T: DeserializeOwned>(
        &self,
        status: reqwest::StatusCode,
        body: &str,
    ) -> Result<T, IgError> {
        // A 200 can still be an error: Instagram returns `{"status":"fail"}`
        // with a 200 in some cases.
        if !status.is_success() || declares_failure(body) {
            return Err(self.classify_and_record(status.as_u16(), body));
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
    ///   [`IgClient::browser_headers`] for the half of that rule which lives on
    ///   the read side.
    /// - **It follows no redirect at all**, through [`IgClient::writer`].
    ///
    /// `referer` is the profile page the button would have been clicked on, in
    /// the same spelling `get` wants: a path with no leading slash.
    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        form: &[(&str, &str)],
        referer: &str,
        style: Surface,
    ) -> Result<T, IgError> {
        // Before the budget is charged, so that a session which cannot write
        // does not spend a slot discovering it. The token itself is put on the
        // request by `browser_headers`, which adds it whenever the session has
        // one; this guard is what makes "whenever" mean "always" on this path.
        if self.session.csrftoken.is_none() {
            return Err(IgError::NoCsrfToken);
        }

        let url = self.base.join(path)?;
        tracing::debug!(%url, "POST");

        self.pacer.clear_to_send_write().await?;

        let request = self
            .browser_headers(self.writer()?.post(url), referer, style)
            .header("Origin", self.base.as_str().trim_end_matches('/'))
            .form(form);

        let response = request.send().await?;
        self.remember_claim(&response);

        let status = response.status();

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
            Err(_) if !status.is_success() => {
                return Err(self.classify_and_record(status.as_u16(), ""));
            }
            Err(e) => return Err(e),
        };

        self.decode(status, &body)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use snob_core::session::{Session, SessionOrigin};
    use snob_core::store::rate_budget::{RateBudget, RateBudgetError};
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

    impl snob_core::store::rate_budget::RateBudget for Recording {
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
                snob_core::store::rate_budget::rate_limit_cooldown()
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

        // Instagram answers `Vary` on these two, so they change its reply.
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
        assert!(matches!(error, IgError::Network(_)), "{error:?}");

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

    /// A session with a CSRF token, which is what `login --browser` produces
    /// and what the write path requires.
    async fn writer(server: &MockServer) -> IgClient {
        let mut session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        session.csrftoken = Some("TOKEN".into());
        IgClient::new(session, crate::pace::Pacer::unlimited())
            .unwrap()
            .with_base_url(Url::parse(&server.uri()).unwrap())
    }

    /// What `/web/friendships/{pk}/follow/` really answers: one word.
    const FOLLOWED: &str = r#"{"result":"following","status":"ok"}"#;

    /// The mobile shape, which this endpoint does not send and the client reads
    /// anyway. Instagram has been moving the web client onto the `/api/v1/`
    /// routes everywhere else, and the day it moves this one the answer changes
    /// shape without changing meaning.
    const FOLLOWED_OBJECT: &str =
        r#"{"status":"ok","friendship_status":{"following":true,"outgoing_request":false}}"#;

    /// **The route is `/web/friendships/`, and that was settled by trying it.**
    /// `/api/v1/friendships/create/` is what every write-up gives and is the
    /// The request this client sends, asserted against what it is *meant* to
    /// The route and the shape, which took three live attempts to settle — see
    /// [`IgClient::friendship`] for what the other two were.
    #[tokio::test]
    async fn a_follow_goes_to_the_page_route_with_no_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/web/friendships/7/follow/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FOLLOWED))
            .mount(&server)
            .await;

        let status = writer(&server).await.follow(7, "someone").await.unwrap();
        assert!(status.following);

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].body.is_empty(),
            "this route takes no body; the /api/v1/ form fields belong to that one"
        );
    }

    /// Either answer shape means the same thing to the caller.
    #[tokio::test]
    async fn both_answer_shapes_read_the_same() {
        for body in [FOLLOWED, FOLLOWED_OBJECT] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            let status = writer(&server).await.follow(7, "someone").await.unwrap();
            assert!(status.following, "{body}");
            assert!(!status.outgoing_request, "{body}");
        }
    }

    /// **The four headers that say "I am the app" are not sent to the page's
    /// own routes**, and that is not a style preference: sent with them, this
    /// exact route answers 404, which reads as the route having been removed
    /// and cost two live attempts to tell apart. The read path must keep them,
    /// so both halves are asserted here together.
    #[tokio::test]
    async fn a_page_route_does_not_claim_to_be_the_app() {
        const APP_ONLY: [&str; 4] = [
            "x-ig-app-id",
            "x-asbd-id",
            "x-ig-www-claim",
            "x-requested-with",
        ];

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FOLLOWED))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
            .mount(&server)
            .await;

        let client = writer(&server).await;
        client.follow(7, "someone").await.unwrap();
        client.validate().await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let write = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::POST)
            .expect("the write");
        let read = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::GET)
            .expect("the read");

        for header in APP_ONLY {
            assert!(
                !write.headers.contains_key(header),
                "a /web/ route was told {header}, which makes it answer 404"
            );
            assert!(
                read.headers.contains_key(header),
                "an /api/v1/ route needs {header} and did not get it"
            );
        }
        // What the page's own fetch does send, and all it sends.
        assert!(write.headers.contains_key("x-csrftoken"));
        assert!(write.headers.contains_key("content-type"));
        assert!(write.headers.contains_key("cookie"));
    }

    /// A private account answers `requested`, and that is not a follow.
    #[tokio::test]
    async fn a_private_account_answers_that_it_was_asked() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"result":"requested","status":"ok"}"#),
            )
            .mount(&server)
            .await;

        let status = writer(&server).await.follow(7, "someone").await.unwrap();
        assert!(status.outgoing_request);
        assert!(!status.following, "a request is not a follow");
    }

    /// The three headers a POST carries that a GET does not, plus the CSRF
    /// token, which a GET carries too but a write cannot go without.
    #[tokio::test]
    async fn a_write_carries_origin_a_content_type_and_the_csrf_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FOLLOWED))
            .mount(&server)
            .await;

        writer(&server).await.follow(7, "someone").await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let headers = &requests[0].headers;
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
        // What it does **not** carry is asserted next door, in
        // `a_page_route_does_not_claim_to_be_the_app`, together with the read
        // path that must carry it.
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
            .follow(7, "someone")
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
        Mock::given(method("POST"))
            .and(path("/web/friendships/7/unfollow/"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", "/web/friendships/7/unfollow/"),
            )
            .mount(&server)
            .await;

        let error = writer(&server)
            .await
            .unfollow(7, "someone")
            .await
            .unwrap_err();
        assert!(
            matches!(error, IgError::Unexpected { status: 307, .. }),
            "{error:?}"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
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
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"message":"feedback_required","status":"fail"}"#),
            )
            .mount(&server)
            .await;

        let (client, budget) = watching_as(&server.uri(), true);

        let error = client.follow(7, "someone").await.unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(budget.calls().len(), 1, "the cooldown was not written down");
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
