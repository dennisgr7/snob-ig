//! What a request to Instagram says about itself.
//!
//! One list, in [`IgClient::dressed`], because two copies of it is how the
//! coherence `client_hints.rs` exists to maintain gets lost: a header added
//! for a good reason on one and forgotten on the other makes two requests on
//! one session look like two clients.
//!
//! [`Surface`] is the one thing that varies, and it is not cosmetic -- it
//! changes which handler answers. The `X-IG-WWW-Claim` cycle is here too,
//! because a header is the whole of what it is for.

use url::Url;

use crate::client_hints;
use crate::{BASE_URL, IG_APP_ID};

use super::IgClient;

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
pub(super) enum Surface<'a> {
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

impl IgClient {
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

    pub(super) fn remember_claim(&self, response: &reqwest::Response) {
        let Some(fresh) = response
            .headers()
            .get("x-ig-set-www-claim")
            .and_then(|v| v.to_str().ok())
        else {
            return;
        };
        *self.claim.lock().unwrap_or_else(|e| e.into_inner()) = fresh.to_string();
    }

    /// The request itself, with the headers a browser would send.
    ///
    /// Split from the sending so that a redirect hop is built the same way the
    /// first request was, rather than by whatever the HTTP client decided to
    /// carry forward.
    pub(super) fn api_request(
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
    pub(super) fn dressed(
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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use snob_core::Pk;

    use super::*;
    use crate::client::harness::{
        FOLLOWED, Known, UA, client, instagram_that_takes_a_write, writer,
    };

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
            .follow(Pk::new(7), "someone", &Known)
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
}
