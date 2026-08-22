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
/// Not optional, for the reason `client/transport.rs` gives about its own:
/// without them, a server that accepts the connection and then says nothing
/// hangs the process for good, and neither a cancel token nor any deadline
/// above reaches a socket that is simply waiting.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Which roots a client checks the server's certificate against.
///
/// **Opt-in, and only ever on the Instagram client.** reqwest 0.13 made
/// `rustls-platform-verifier` the default, so the four rustls targets now honor
/// whatever an administrator has installed — which is the right default for a
/// tool that has to work on a managed machine, and is also how a laptop with a
/// TLS-inspecting root in its store lets that middlebox read the session in
/// transit. Narrowing to Mozilla's published roots ends that, and it also ends
/// working behind a corporate proxy, which is why it is a flag and not a
/// decision made here.
///
/// [`Trust::Narrow`] carries the way back out with it. Somebody who needs one
/// private root and no others should be able to say so without giving the whole
/// store back, so `--tls-extra-root` is part of the same value rather than a
/// second switch that could be set without this one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Trust {
    /// The platform's store, enterprise roots included.
    #[default]
    Platform,
    /// Mozilla's published roots, plus the PEM bundles the user named and
    /// nothing else.
    Narrow { extra: Vec<Vec<u8>> },
}

/// Whether narrowing can do anything on this build.
///
/// Windows on ARM64 is on schannel, for the reason written beside the two
/// dependency tables in this crate's manifest, and schannel has no equivalent
/// of "these roots and no others". The flag therefore **refuses** there rather
/// than being accepted and quietly ignored: a security option that silently
/// does nothing is worse than one that is not offered, because somebody
/// believes it.
pub const CAN_NARROW: bool = !cfg!(all(windows, target_arch = "aarch64"));

/// What every Instagram client built from now on trusts.
///
/// A process-global for the same reason [`crate::client::SANDBOX_BASE`] is one:
/// three separate places build a client, and `login::validate` is in this crate
/// and never sees the binary's arguments, so an argument would have to be
/// threaded through five signatures to carry a value that is almost always the
/// default. Set once from `main`, before any client exists.
static TRUST: std::sync::OnceLock<Trust> = std::sync::OnceLock::new();

/// Narrows the trust store for every Instagram client built after this call.
///
/// `Err` carries the value **already chosen** back on a second call, so the
/// caller can tell a repeat of the same answer from a change of mind.
pub fn use_trust(trust: Trust) -> Result<(), Trust> {
    TRUST.set(trust).map_err(|_| chosen_trust())
}

/// What was chosen, or the platform's store.
pub(crate) fn chosen_trust() -> Trust {
    TRUST.get().cloned().unwrap_or_default()
}

/// The builder every client in this program starts from.
///
/// Shared rather than written twice, which it was:
/// `client::transport::build_client` was this statement for statement,
/// differing only in its two numbers and in the error it returns. A setting that has to hold for every client this program
/// makes now has one place to go, instead of two that can be half updated.
///
/// The timeouts are arguments rather than constants here because the two callers
/// really do want different ones — Instagram's walk is slower than a POST to a
/// webhook on the same network — and because a builder that chose for them would
/// be a third opinion about it.
/// **`trust` is an argument rather than something this reads for itself**, and
/// that is the whole of how the narrowing is kept off the webhook client.
/// [`plain`] and [`plain_direct`] pass [`Trust::Platform`] and have no
/// parameter for anything else, so a private CA on the user's own receiver goes
/// on working no matter what the Instagram client was told — the same shape
/// that already stops the session reaching a webhook.
pub fn builder(
    user_agent: &str,
    redirect: reqwest::redirect::Policy,
    connect_timeout: Duration,
    request_timeout: Duration,
    trust: &Trust,
) -> reqwest::Result<reqwest::ClientBuilder> {
    let built = reqwest::Client::builder()
        .user_agent(user_agent.to_string())
        .redirect(redirect)
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        // reqwest drops an idle connection at 90 s, and this tool deliberately
        // waits between requests: the long pause every seventh page is up to 15
        // s on its own, and the request budget is shared between processes, so
        // a second snob can push a wait arbitrarily far out. Measured on the
        // boundary: 89 s reuses the connection, 95 s opens a new one — a fresh
        // TCP and TLS handshake for a request that was only being polite. Five
        // minutes is Chromium's own idle-socket timeout.
        //
        // The keepalive is the other half. A connection held open for minutes
        // is one a NAT or a firewall may drop silently, and without this the
        // first thing that notices is a failed request; with it, the kernel
        // does.
        .pool_idle_timeout(Duration::from_secs(300))
        .tcp_keepalive(Duration::from_secs(60));

    let Trust::Narrow { extra } = trust else {
        return Ok(built);
    };

    // Unreachable on the schannel target, and it has to *compile* there all the
    // same: `webpki-root-certs` is only a dependency where rustls is, so the
    // narrowing below does not exist on that build at all. `CAN_NARROW` is
    // false there and `main` refuses the flag before any client is made, so
    // this arm answers the only thing it can.
    #[cfg(all(windows, target_arch = "aarch64"))]
    {
        let _ = extra;
        return Ok(built);
    }

    // Mozilla's list in full, then whatever the user named. `tls_certs_only`
    // is the one that turns the platform store off; `tls_certs_merge` would
    // add to it and leave the enterprise root exactly where it was.
    #[cfg(not(all(windows, target_arch = "aarch64")))]
    {
        let mut roots: Vec<reqwest::Certificate> = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|der| reqwest::Certificate::from_der(der))
            .collect::<reqwest::Result<_>>()?;
        for bundle in extra {
            roots.extend(reqwest::Certificate::from_pem_bundle(bundle)?);
        }
        Ok(built.tls_certs_only(roots))
    }
}

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
    // `Trust::Platform`, always and with no way to say otherwise. A private CA
    // in front of somebody's own webhook receiver is legitimate, and narrowing
    // the roots there would break it for no gain: the thing worth protecting
    // from a middlebox is the Instagram session, and this client does not carry
    // one.
    builder(
        user_agent,
        redirect,
        CONNECT_TIMEOUT,
        REQUEST_TIMEOUT,
        &Trust::Platform,
    )?
    .build()
}

/// The same, for a destination that is only allowed to be plain `http://`
/// **because it is on the user's own network** — and which therefore must not
/// be sent through a proxy.
///
/// `webhook::check` permits an unencrypted address exactly when it is private,
/// on the stated grounds that the traffic stays inside the user's own network.
/// With `HTTP_PROXY` set in the environment that reasoning is void: hyper-util's
/// matcher has no loopback exemption, so a POST to `http://127.0.0.1:8787/hook`
/// goes to the proxy instead — in the clear, carrying the report, the
/// `Authorization` header and the signature. That was reproduced against a
/// recording proxy, not reasoned about.
///
/// Go's `ProxyFromEnvironment` never proxies loopback for the same reason, and
/// this is narrower still: it is applied only where the private address is the
/// whole justification for the request being unencrypted. A public `https://`
/// receiver keeps honoring the environment, because somebody behind a mandatory
/// proxy has no other route out.
pub fn plain_direct(
    user_agent: &str,
    redirect: reqwest::redirect::Policy,
) -> reqwest::Result<reqwest::Client> {
    // `Trust::Platform` for the same reason as [`plain`].
    builder(
        user_agent,
        redirect,
        CONNECT_TIMEOUT,
        REQUEST_TIMEOUT,
        &Trust::Platform,
    )?
    .no_proxy()
    .build()
}

/// Reads a response body, stopping at `cap`.
///
/// The same reasoning as the reader in `client/transport.rs`: `text()` buffers
/// whatever arrives, which lets the far end decide how much memory this
/// process uses.
/// Reading in chunks bounds a response with no `Content-Length` too, which is
/// most of them.
///
/// Lossy rather than strict, because this is for showing a person what the
/// other end said, and "could not parse" is less use than the bytes.
pub async fn read_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<String, reqwest::Error> {
    // Said, not silently emptied: the same body with no declared length
    // comes back as its first `cap` bytes, and a reader of the excerpt could
    // not tell an empty answer from one too big to show.
    if let Some(declared) = response.content_length()
        && declared > cap as u64
    {
        return Ok(format!("<{declared} bytes, more than the {cap} shown>"));
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

    /// The default is the platform's store, and it has to stay that way.
    ///
    /// Narrowing is the safer setting for somebody being inspected and the
    /// broken one for somebody behind a corporate proxy, and only the person
    /// running it knows which they are. A default that flipped would take the
    /// tool away from the second group with no message.
    #[test]
    fn the_platform_store_is_what_is_trusted_unless_asked() {
        assert!(matches!(Trust::default(), Trust::Platform));
    }

    /// A client that carries no credential is never narrowed, and there is no
    /// argument with which to narrow it.
    ///
    /// This is the rule the audit attached to the whole item: a private CA in
    /// front of the user's own webhook receiver is legitimate. It is held down
    /// structurally rather than by remembering — `plain` and `plain_direct`
    /// take a user agent and a redirect policy and nothing else — so this test
    /// exists to fail the day somebody adds the parameter.
    #[test]
    fn the_webhook_client_cannot_be_narrowed() {
        let source = include_str!("http.rs");
        for name in ["pub fn plain(", "pub fn plain_direct("] {
            let at = source.find(name).expect(name);
            let signature = &source[at..source[at..].find(')').unwrap() + at];
            assert!(
                !signature.contains("Trust"),
                "{name} has grown a way to be told what to trust: {signature}"
            );
        }
    }

    /// Narrowing replaces the roots rather than adding to them.
    ///
    /// `tls_certs_merge` compiles just as well and leaves the enterprise root
    /// exactly where it was, which is the one thing this option exists to
    /// remove. The difference is one word in one call and nothing else would
    /// notice.
    #[test]
    fn narrowing_uses_only_the_roots_it_was_given() {
        // Only the code. The tests below this line talk *about* the wrong call
        // by name, and a search over the whole file finds its own prose.
        let source = include_str!("http.rs");
        let code = &source[..source.find("#[cfg(test)]").expect("a test module")];
        assert!(
            code.contains("tls_certs_only("),
            "the roots are not replaced"
        );
        assert!(
            !code.contains("tls_certs_merge("),
            "merging leaves the platform's roots in place, which is the exposure"
        );
    }

    /// Mozilla's list is not empty, and every entry in it is a certificate the
    /// client can be handed.
    ///
    /// Cheap, and it covers the mistake that would otherwise show up as a
    /// handshake failure against Instagram with `--strict-roots` and nowhere
    /// else: `webpki-roots` publishes trust anchors rather than certificates,
    /// and swapping the crate back would compile until this ran.
    #[cfg(not(all(windows, target_arch = "aarch64")))]
    #[test]
    fn the_bundled_roots_are_certificates() {
        // `CAN_NARROW` is a constant, so asserting it here would be asserting
        // that this `cfg` is spelled the same way twice -- which it is, and
        // which the compiler already knows. What is worth checking is the list.
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS;
        assert!(roots.len() > 50, "only {} roots", roots.len());
        for der in roots {
            reqwest::Certificate::from_der(der).expect("a root that is not a certificate");
        }
    }

    /// And on the target where it cannot work, it says so rather than being
    /// accepted.
    #[cfg(all(windows, target_arch = "aarch64"))]
    #[test]
    fn the_schannel_build_does_not_pretend_it_can_narrow() {
        assert!(!CAN_NARROW);
    }

    /// A run cannot change what it trusts halfway through.
    ///
    /// `IgClient` is built in three places and one of them is inside this
    /// crate, so the value is a process-global; the thing that makes that safe
    /// is that the second write is refused rather than applied. Without it a
    /// client built early and one built late could disagree about what they
    /// check a certificate against.
    #[test]
    fn what_is_trusted_is_settled_once() {
        // Whichever of these runs first in this process wins; both orders have
        // to leave one value in place, which is what is asserted.
        let first = use_trust(Trust::Platform);
        let second = use_trust(Trust::Narrow { extra: Vec::new() });
        assert!(
            first.is_err() || second.is_err(),
            "two different answers were both accepted"
        );
    }
}
