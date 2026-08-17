//! The HTTP plumbing, for a caller that is not talking to Instagram.
//!
//! **Why this is in this crate at all.** The TLS backend is chosen per target,
//! in this crate's manifest, with the reasoning written beside the two tables:
//! `rustls` everywhere except Windows on ARM64, where neither of rustls's crypto
//! providers builds without LLVM. Declaring `reqwest` a second time somewhere
//! else would mean repeating that `cfg`, and a copy that falls behind is a
//! target that stops compiling months later for a reason nobody connects to it.
//! It would also let a second manifest widen the feature set — `cookies` turned
//! on for one caller is `cookies` turned on for [`crate::client`], which is the
//! compile error that currently stops a cookie jar being added there by
//! accident.
//!
//! So the plumbing is exposed from here and `reqwest` is re-exported, and
//! nothing else in the workspace names it.
//!
//! **What is not here is a credential.** [`plain`] builds a client that carries
//! none, which is the same shape [`crate::client`] already uses for the CDN: the
//! session cookie travels as an explicit header on the one client that is
//! supposed to have it, and a client built here cannot acquire it because there
//! is no argument to pass it in.

use std::time::Duration;

/// Re-exported so callers can name `reqwest`'s types without declaring it. See
/// the module header for why that matters.
pub use reqwest;

/// How long a request may take in total, and how long the connection may take
/// to establish.
///
/// Not optional, for the reason `client.rs` gives about its own: without them, a
/// server that accepts the connection and then says nothing hangs the process
/// for good, and neither a cancel token nor any deadline above reaches a socket
/// that is simply waiting.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// A client that carries no credential.
///
/// `redirect` is the caller's to choose and there is no default, because the
/// right answer differs: the CDN follows redirects within itself, and a POST
/// carrying a report should follow none at all — a 3xx is how the body ends up
/// at a host the user never named.
pub fn plain(
    user_agent: &str,
    redirect: reqwest::redirect::Policy,
) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent.to_string())
        .redirect(redirect)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
}

/// Reads a response body, stopping at `cap`.
///
/// The same reasoning as the reader in `client.rs`: `text()` buffers whatever
/// arrives, which lets the far end decide how much memory this process uses.
/// Reading in chunks bounds a response with no `Content-Length` too, which is
/// most of them.
///
/// Lossy rather than strict, because this is for showing a person what the
/// other end said, and "could not parse" is less use than the bytes.
pub async fn read_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<String, reqwest::Error> {
    if let Some(declared) = response.content_length()
        && declared > cap as u64
    {
        return Ok(String::new());
    }

    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > cap {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The guard this module exists to keep. A client built here has no way to
    /// be given the session, and the header it does send names this tool rather
    /// than a browser.
    #[tokio::test]
    async fn a_plain_client_sends_no_cookie_and_its_own_user_agent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = plain("snob/test", reqwest::redirect::Policy::none()).unwrap();
        client.get(server.uri()).send().await.unwrap();

        let received = &server.received_requests().await.unwrap()[0];
        assert!(
            received.headers.get("cookie").is_none(),
            "a client built here must not be able to carry a session"
        );
        assert_eq!(received.headers.get("user-agent").unwrap(), "snob/test");
    }

    /// A body larger than the cap does not decide how much memory this uses.
    #[tokio::test]
    async fn a_body_past_the_cap_is_cut_rather_than_buffered() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(10_000)))
            .mount(&server)
            .await;

        let client = plain("snob/test", reqwest::redirect::Policy::none()).unwrap();
        let response = client.get(server.uri()).send().await.unwrap();
        assert!(read_capped(response, 100).await.unwrap().len() <= 100);
    }
}
