//! Profile pictures and story media, and the rules about where one may come
//! from.
//!
//! The CDN is a different host from the API, under the opposite redirect rule:
//! an asset request has to be allowed to move between CDN hosts, and an API
//! call must not leave instagram.com. That is why there are two policies and
//! two clients, and why this half is not in [`super::transport`].
//!
//! Nothing identifying travels from here: no session cookie, no app id, and no
//! charge against Instagram's budget. [`super::write`] reads the mutation
//! bundles through the same path, for both of those reasons.

use url::Url;

use crate::error::IgError;

use super::IgClient;
use super::transport::{MAX_HOPS, read_capped_bytes, same_origin, stream_capped};

/// Ceiling on a downloaded asset. A profile picture tops out at 1080x1080 and
/// lands far below this; the cap exists so that a redirect to something else
/// cannot make us read until memory runs out.
const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;

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

/// Redirects for an asset: every hop held to the same rule as the first.
///
/// The picture URL comes out of Instagram's own answer, so a redirect chain is
/// the one place where a response gets to choose where the next request goes.
/// Checking only the address as written left hops two and three judged by
/// scheme alone, which is a weaker rule than the one the module documents.
pub(super) fn cdn_policy(base: Url) -> reqwest::redirect::Policy {
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

impl IgClient {
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
    ///
    /// Public because a story video does not fit under [`MAX_ASSET_BYTES`],
    /// which was sized for a 1080x1080 picture: rather than raising that
    /// constant -- and with it the ceiling on every profile picture, for a
    /// reason that has nothing to do with profile pictures -- the caller that
    /// needs a different cap says so, and says why where it says it. It used
    /// to be re-exported through a wrapper whose whole doc explained that it
    /// was identical to this; the wrapper said nothing the visibility cannot.
    /// Everything else is the same for every caller,
    /// [`IgClient::check_downloadable`] included: the URL still has to point
    /// at the CDN, and every redirect hop after it is held to the same rule.
    pub async fn download_capped(&self, url: &str, cap: usize) -> Result<Vec<u8>, IgError> {
        let response = self.fetch_asset(url).await?;
        // Raced against the token like the API read, for the same reason: a
        // CDN that answers with headers and then stalls holds this process for
        // as long as it likes.
        tokio::select! {
            biased;
            () = self.pacer.cancel_token().canceled() => Err(IgError::Canceled),
            bytes = read_capped_bytes(response, cap as u64) => bytes,
        }
    }

    /// [`IgClient::download_capped`], written to `sink` as it arrives rather
    /// than held in memory first.
    ///
    /// For a story video. `download_capped` returns the whole body as a `Vec`,
    /// and a command that then wrote it to disk had the file in memory once
    /// for the download and -- until it stopped copying -- once more for the
    /// write: eighty megabytes of resident memory for a forty-megabyte clip,
    /// whose only destination was a file. This hands each chunk to the sink
    /// and keeps nothing, so the peak is one chunk whatever the size.
    ///
    /// What comes back is [`Downloaded`]: the byte count and the first few
    /// bytes, because the file's extension is decided from its magic number
    /// and a caller that streamed everything to disk no longer has them.
    ///
    /// Everything else is the same as `download_capped` -- the CDN-only
    /// redirect rule, the unpaced client, the browser's `Accept-Encoding`,
    /// the ceiling, the race against Ctrl+C -- because it is the same fetch
    /// with a different place to put the bytes. On any error the sink holds a
    /// prefix of the file; whoever owns the file removes it.
    pub async fn download_to(
        &self,
        url: &str,
        cap: usize,
        sink: &mut (impl std::io::Write + Send),
    ) -> Result<Downloaded, IgError> {
        let response = self.fetch_asset(url).await?;
        let mut head = Head::default();
        let mut tee = Tee {
            head: &mut head,
            sink,
        };
        let len = tokio::select! {
            biased;
            () = self.pacer.cancel_token().canceled() => Err(IgError::Canceled),
            n = stream_capped(response, cap as u64, &mut tee) => n,
        }?;
        Ok(Downloaded {
            len,
            head: head.bytes,
        })
    }

    /// The GET behind both downloads: checked, unpaced, and refused on a
    /// status the CDN's own terms explain.
    async fn fetch_asset(&self, url: &str) -> Result<reqwest::Response, IgError> {
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
        Ok(response)
    }
}

/// What a streamed download leaves the caller with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    /// Bytes written to the sink.
    pub len: u64,
    /// The first bytes of the file, up to [`HEAD_BYTES`]: enough for the magic
    /// number that decides the extension, retained because the rest went
    /// straight to disk.
    pub head: Vec<u8>,
}

/// How many leading bytes [`Downloaded::head`] keeps. Twelve is what the
/// WebP check needs (`RIFF....WEBP`); sixteen leaves room.
pub const HEAD_BYTES: usize = 16;

#[derive(Default)]
struct Head {
    bytes: Vec<u8>,
}

/// Copies the first [`HEAD_BYTES`] aside and forwards everything to the sink.
struct Tee<'a, W: std::io::Write> {
    head: &'a mut Head,
    sink: &'a mut W,
}

impl<W: std::io::Write> std::io::Write for Tee<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let room = HEAD_BYTES.saturating_sub(self.head.bytes.len());
        if room > 0 {
            self.head
                .bytes
                .extend_from_slice(&buf[..buf.len().min(room)]);
        }
        self.sink.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sink.flush()
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::BASE_URL;
    use crate::client::harness::{UA, client};

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

    /// The streamed download leaves the caller the two things it still needs
    /// after the bytes have gone to disk: how many there were, and the magic
    /// number -- the whole file, when the file is shorter than the head.
    #[tokio::test]
    async fn a_streamed_download_keeps_its_head_and_its_length() {
        let server = MockServer::start().await;
        let body: Vec<u8> = (0..40u8).collect();
        Mock::given(method("GET"))
            .and(path("/clip.mp4"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;

        let url = format!("{}/clip.mp4", server.uri());
        let client = client(&server).await;
        let mut sink = Vec::new();
        let got = client.download_to(&url, 64, &mut sink).await.unwrap();

        assert_eq!(sink, body, "every byte reached the sink, in order");
        assert_eq!(got.len, 40);
        assert_eq!(got.head, body[..HEAD_BYTES].to_vec());

        // Shorter than the head: the head is the whole thing.
        Mock::given(method("GET"))
            .and(path("/tiny"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;
        let mut sink = Vec::new();
        let got = client
            .download_to(&format!("{}/tiny", server.uri()), 64, &mut sink)
            .await
            .unwrap();
        assert_eq!(got.head, vec![1, 2, 3]);
        assert_eq!(got.len, 3);
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

        // The streaming twin meets the same ceiling in the same loop, and the
        // sink holds what arrived before it -- a prefix, never the whole
        // body -- which is what lets a caller writing to disk remove the
        // file rather than keep a truncated one.
        let mut sink = Vec::new();
        let error = client.download_to(&url, 8, &mut sink).await.unwrap_err();
        assert!(matches!(error, IgError::TooLarge { limit: 8 }));
        assert!(
            sink.len() < 64,
            "the whole body reached the sink: {}",
            sink.len()
        );

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
}
