//! How a request goes out to Instagram's API, and how the answer comes back.
//!
//! The origin rule, the redirect loop that pays for every hop it follows, the
//! ceilings on what will be read, and the two races against the cancel token.
//! [`IgClient::get`] is what the endpoints in [`super::read`] go through, and
//! [`Answer`] is what it hands back for [`IgClient::decode`] to read.
//!
//! What a request *says* about itself is in [`super::headers`]. The CDN's own
//! rules are in [`super::media`] rather than here, because they are the
//! opposite rules: an asset request has to be allowed to move between hosts
//! and an API call must not. Keeping the two apart is what stops the looser of
//! them governing the requests that carry the credentials.

use serde::de::DeserializeOwned;
use url::Url;

use crate::error::IgError;

use super::IgClient;
use super::headers::Surface;

/// Ceiling on an API response.
///
/// A page of fifty accounts is tens of kilobytes; this is orders of magnitude
/// above anything real. It exists because the body is read into memory whole,
/// so without a limit a hostile or broken answer decides how much memory this
/// process uses.
pub(super) const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// How long a single request may take end to end, and how long the connection
/// itself may take to come up. Generous: the walk's own pacing is what keeps
/// requests apart, and these are only here so that nothing waits forever.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How far a redirect chain may go before it is a loop by another name.
pub(super) const MAX_HOPS: usize = 3;

/// Same scheme, same host, same port. Not "the same host", which is what this
/// used to be in one place and is the reason the rest of this section exists.
pub(super) fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Redirects for the API: none, because following one is a request and a
/// request has to be paid for.
///
/// `IgClient::get_body` follows them itself, charging `Pacer::clear_to_send`
/// per hop and holding every hop to the same origin. Leaving it to the HTTP
/// client meant the hops went out unpaced and uncounted, which is what this
/// exists to stop — the reasoning is on `get_body`, next to the loop that does
/// it.
pub(super) fn api_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::none()
}

/// One HTTP client under a given redirect policy.
///
/// The timeouts are not optional. Without them, a server that accepts the
/// connection and then says nothing hangs the process for good: neither the
/// cancel token nor any deadline above reaches a socket that is simply waiting.
pub(super) fn build_client(
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
pub(super) async fn read_capped(response: reqwest::Response, cap: u64) -> Result<String, IgError> {
    // Lossy rather than strict: a body that is not valid UTF-8 is not JSON
    // either, and saying "could not parse" is more use than "invalid encoding".
    Ok(utf8_or_lossy(read_capped_bytes(response, cap).await?))
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
pub(super) async fn read_capped_bytes(
    response: reqwest::Response,
    cap: u64,
) -> Result<Vec<u8>, IgError> {
    // Sized from the declaration when there is one, which in practice means
    // the CDN: tower-http drops `Content-Length` when it decompresses, and
    // every API answer is compressed, so an API body grows by doubling and a
    // picture arrives into a buffer of the right size. Capped by the ceiling
    // so a lying header cannot reserve more than this would ever accept.
    let mut bytes: Vec<u8> = Vec::with_capacity(
        response
            .content_length()
            .map_or(0, |declared| declared.min(cap) as usize),
    );
    stream_capped(response, cap, &mut bytes).await?;
    Ok(bytes)
}

/// The body, chunk by chunk, into whatever the caller hands over -- a `Vec`
/// for the API and a file for a story -- refusing past `cap`.
///
/// **The one copy of the chunked read.** The in-memory reader above is this
/// over a `Vec`, and the story download is this over a file on disk, so the
/// ceiling is counted in one loop whichever way the bytes are going. The
/// declared-length check in front of it only ever fires on an uncompressed
/// answer (see `read_capped_bytes` for why that means the CDN); the running
/// count is the barrier every answer meets.
///
/// Returns how many bytes were written. On `TooLarge` the sink holds what
/// arrived before the ceiling; a caller writing to disk removes the file.
pub(super) async fn stream_capped(
    mut response: reqwest::Response,
    cap: u64,
    sink: &mut (impl std::io::Write + Send),
) -> Result<u64, IgError> {
    let too_large = || IgError::TooLarge {
        limit: cap as usize,
    };

    if let Some(declared) = response.content_length()
        && declared > cap
    {
        return Err(too_large());
    }

    let mut written: u64 = 0;
    while let Some(chunk) = response.chunk().await? {
        if written + chunk.len() as u64 > cap {
            return Err(too_large());
        }
        sink.write_all(&chunk)
            .map_err(|e| IgError::Decode(format!("could not write the download: {e}")))?;
        written += chunk.len() as u64;
    }
    Ok(written)
}

/// A body as text, copied only when it has to be.
///
/// `String::from_utf8_lossy(..).into_owned()` copies the whole body even when
/// it is valid UTF-8 -- which every answer here is -- because the borrowed
/// `Cow` has to be owned. `from_utf8` moves the buffer instead, and the lossy
/// path is kept for the byte sequence that is not text, with the same
/// replacement characters it always produced.
fn utf8_or_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// The load headers, if Instagram volunteered any, as one string to log.
///
/// Absent from a CDN answer and from anything that is not the API, so `None` is
/// ordinary rather than notable.
pub(super) fn load_of(response: &reqwest::Response) -> Option<String> {
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

/// `Retry-After`, if the answer carried one.
///
/// A string rather than a parsed duration on purpose: the header has two legal
/// forms, seconds and an HTTP date, and until it is known which of them these
/// endpoints send — if either — turning it into a number would be deciding the
/// answer to the question the logging exists to ask.
pub(super) fn retry_after(response: &reqwest::Response) -> Option<String> {
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
pub(super) struct Answer {
    pub(super) status: u16,
    pub(super) body: String,
    pub(super) retry_after: Option<String>,
    /// What Instagram said about its own load while answering.
    ///
    /// `x-ig-capacity-level` and `x-ig-peak-time`, joined. Not decided on, and
    /// the reason is with the rest of the pacing reasoning in [`crate::pace`]:
    /// they describe a datacenter's headroom, which is the same for everyone in
    /// that region, and what the pace is managing is a checkpoint on one
    /// account. Carried so that the run which is finally refused can say what
    /// the load was at that moment, which is the observation nobody has.
    pub(super) load: Option<String>,
}

impl Answer {
    /// Whether the status is a 2xx. Half the question: a 200 can still
    /// declare failure in the body, and that half is `decode`'s. Spelled once
    /// because `(200..300).contains(...)` was written at three sites, and a
    /// range typo at one of them is invisible in review.
    pub(super) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl IgClient {
    /// One request to Instagram's API, with the headers a browser would send.
    ///
    /// `referer` is the path of the page the call would have come from, without
    /// the leading slash. It is not decoration: the tool asks for a followers
    /// list from a URL that, in a browser, only that account's followers page
    /// ever calls, and a generic referer next to a specific endpoint is an
    /// incoherence that costs nothing to avoid.
    pub(super) async fn get<T: DeserializeOwned>(
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
    pub(super) async fn get_body(
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
                    return Err(self.refuse(&answer));
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
    pub(super) async fn send_or_cancel(
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use snob_core::Pk;
    use snob_core::session::{Session, SessionOrigin};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::Direction;
    use crate::client::harness::{Recording, SID, UA, client, watching};

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
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
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
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
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
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
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
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
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
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
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
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
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
}
