//! HTTP client against Instagram's web API.
//!
//! It uses the `www.instagram.com` endpoints rather than the `i.instagram.com`
//! ones, to stay consistent with a session that originated in a desktop
//! browser. That consistency between cookie, User-Agent and endpoint is what
//! Instagram evaluates, and breaking it is what produces `useragent mismatch`.

use std::sync::Mutex;

use serde::de::DeserializeOwned;
use snob_core::Pk;
use snob_core::session::Session;
use url::Url;

use crate::error::{IgError, classify, declares_failure};
use crate::fingerprint::{self, Fingerprint};
use crate::model::{
    FriendshipsPage, Identity, UserInfo, UserInfoEnvelope, WebProfileInfo, WebProfileInfoEnvelope,
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

/// Redirects are followed, but never off HTTPS and never far.
///
/// The picture URL comes out of Instagram's own answer, so a redirect chain is
/// the one place where a response gets to choose where the next request goes.
/// Refusing to step down to plain HTTP is what keeps that from reaching a
/// loopback port or a cloud metadata endpoint, neither of which speaks TLS.
/// Test servers run on HTTP, so the first hop is judged by where it started.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let downgraded = attempt.url().scheme() != "https"
            && attempt.previous().first().map(|u| u.scheme()) != Some(attempt.url().scheme());

        if attempt.previous().len() >= 3 {
            attempt.error("too many redirects")
        } else if downgraded {
            attempt.error("a redirect tried to leave https")
        } else {
            attempt.follow()
        }
    })
}

/// Reads a response body, refusing one that will not fit.
///
/// `text()` would buffer whatever arrives, which lets the far end decide how
/// much memory this process uses. Read in chunks so that a response with no
/// `Content-Length` — which is most of them — is bounded too.
async fn read_capped(mut response: reqwest::Response, cap: u64) -> Result<String, IgError> {
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

    // Lossy rather than strict: a body that is not valid UTF-8 is not JSON
    // either, and saying "could not parse" is more use than "invalid encoding".
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
    http: reqwest::Client,
    base: Url,
    session: Session,
    /// Reserving budget lives here rather than in each caller, so a request
    /// that is never paid for cannot be written.
    pacer: Pacer,
    fingerprint: Fingerprint,
    /// Instagram's session-continuity token. See [`IgClient::claim`].
    claim: Mutex<String>,
}

/// What a browser sends before the server has told it anything.
const INITIAL_CLAIM: &str = "0";

impl IgClient {
    pub fn new(session: Session, pacer: Pacer) -> Result<Self, IgError> {
        let http = reqwest::Client::builder()
            .user_agent(session.user_agent.clone())
            .redirect(redirect_policy())
            // Without these, a server that accepts the connection and then says
            // nothing hangs the process for good: neither the cancel token nor
            // any deadline above reaches a socket that is simply waiting.
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            fingerprint: Fingerprint::from_user_agent(&session.user_agent),
            http,
            base: Url::parse(BASE_URL)?,
            session,
            pacer,
            claim: Mutex::new(INITIAL_CLAIM.to_string()),
        })
    }

    /// The current `X-IG-WWW-Claim`.
    ///
    /// Instagram hands one back in `x-ig-set-www-claim` and expects it echoed
    /// on every request after that. A browser sends the literal `0` **once**,
    /// on its first request of the session, and never again — so a client that
    /// ignores the response header keeps announcing `0` forever, which is to
    /// say "I have just been born", four hundred times in a row. Following the
    /// cycle costs a header and removes a free tell.
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
    pub fn with_base_url(mut self, base: Url) -> Self {
        self.base = base;
        self
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn pacer(&self) -> &Pacer {
        &self.pacer
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
                &format!("{username}/"),
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
                format!("{username}/{segment}/")
            },
        )
        .await
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

    /// Refuses a picture URL that does not go where a picture goes.
    ///
    /// The redirect policy covers every hop **after** the first one, which left
    /// the one hop whose address comes straight out of Instagram's answer as
    /// the only unchecked one — and that is the one an attacker gets to choose.
    /// A `profile_pic_url` of `http://127.0.0.1:9222/json` or of a cloud
    /// metadata address would have been fetched as written.
    ///
    /// So the first hop is held to what the CDN actually is: HTTPS, and one of
    /// the two hosts Instagram serves media from. Tests point the client at a
    /// local server, and its base URL is what says so.
    fn check_downloadable(&self, url: &Url) -> Result<(), IgError> {
        const CDN_HOSTS: [&str; 2] = ["cdninstagram.com", "fbcdn.net"];

        let refuse = || IgError::Unexpected {
            status: 0,
            body: format!(
                "the picture URL points somewhere pictures do not come from: {}",
                url.host_str().unwrap_or("nowhere")
            ),
        };

        // A test server is the deliberate exception, and it is recognized by
        // being the very host this client was pointed at rather than by a
        // scheme or an address range.
        if self.base.host_str().is_some() && url.host_str() == self.base.host_str() {
            return Ok(());
        }

        if url.scheme() != "https" {
            return Err(refuse());
        }
        let host = url.host_str().ok_or_else(refuse)?;
        let served_by_the_cdn = CDN_HOSTS
            .iter()
            .any(|cdn| host == *cdn || host.ends_with(&format!(".{cdn}")));

        if served_by_the_cdn {
            Ok(())
        } else {
            Err(refuse())
        }
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
        let mut response = self.http.get(url).send().await?;
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

        let too_large = || IgError::TooLarge { limit: cap };
        if let Some(declared) = response.content_length()
            && declared > cap as u64
        {
            return Err(too_large());
        }

        // Read in chunks rather than all at once: a response with no
        // Content-Length would otherwise sail past the check above.
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > cap {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
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

        let mut request = self
            .http
            .get(url)
            .query(query)
            // Without this header Instagram answers 403 even with a good session.
            .header("X-IG-App-ID", IG_APP_ID)
            .header("X-ASBD-ID", fingerprint::ASBD_ID)
            .header("X-IG-WWW-Claim", self.claim())
            .header("X-Requested-With", "XMLHttpRequest")
            // `*/*`, not `application/json`: that is what `fetch()` sends when
            // the page does not set one, and no browser sends the latter here.
            .header("Accept", "*/*")
            // Instagram answers `Vary` on the first two, which is it saying
            // its reply depends on them.
            .header("Sec-Fetch-Site", "same-origin")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Dest", "empty")
            .header("Referer", format!("{BASE_URL}/{referer}"))
            .header("Cookie", self.session.cookie_header());

        // Deliberately no `Origin`: a browser omits it on same-origin GETs, so
        // sending one alongside `Sec-Fetch-Site: same-origin` is a combination
        // Chrome cannot produce. Several tools like this one send it anyway.

        if let Some(csrf) = &self.session.csrftoken {
            request = request.header("X-CSRFToken", csrf.expose());
        }

        // Only for browsers that send client hints at all. Inventing them for
        // Firefox would be a mismatch rather than an improvement.
        if let Some(brands) = &self.fingerprint.ua_brands {
            request = request
                .header("Sec-CH-UA", brands)
                .header("Sec-CH-UA-Mobile", self.fingerprint.mobile)
                .header("Sec-CH-UA-Platform", self.fingerprint.platform);
        }

        let response = request.send().await?;
        self.remember_claim(&response);

        let status = response.status();
        let body = read_capped(response, MAX_BODY_BYTES).await?;

        // A 200 can still be an error: Instagram returns `{"status":"fail"}`
        // with a 200 in some cases.
        if !status.is_success() || declares_failure(&body) {
            return Err(self.classify_and_record(status.as_u16(), &body));
        }

        serde_json::from_str(&body).map_err(|e| {
            IgError::Decode(format!(
                "{e} - response: {}",
                body.chars().take(200).collect::<String>()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
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

    /// A browser announces the literal `0` once and then echoes whatever the
    /// server hands back. Staying on `0` forever says "I have just been born"
    /// on every request, which is a free tell.
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
