//! Sending a report to the address the user chose.
//!
//! Everything here is arranged around one rule: **this client cannot carry the
//! session.** [`WebhookClient::new`] takes no `Session` and no `IgClient`, and
//! the client under it is built by `snob_ig::http::plain`, which has no
//! argument to pass a credential through. The destination is a host somebody
//! typed into a configuration file; sending Instagram's cookie there would be
//! the same failure `IgClient::check_downloadable` exists to prevent for the
//! CDN, one crate up.
//!
//! Two smaller rules follow from the same place. Redirects are not followed at
//! all — a 3xx is exactly how a body lands at a host nobody named — and the
//! User-Agent says `snob`, because claiming to be a browser to the user's own
//! server would be a lie told for no reason and would leak which browser they
//! have.

use anyhow::{Result, bail};
use snob_core::secret::Secret;
use snob_core::watch::sign;
use snob_ig::http::{self, reqwest};
use url::Url;

/// How much of an error response is read back to show the user.
///
/// Bounded for the reason every read in this project is: without it the far end
/// decides how much memory this process uses.
const MAX_RESPONSE_BYTES: usize = 4 * 1024;

/// What the receiver is told, beyond the body.
pub const EVENT_HEADER: &str = "X-Snob-Event";
pub const DELIVERY_HEADER: &str = "X-Snob-Delivery";
pub const ATTEMPT_HEADER: &str = "X-Snob-Attempt";

/// Where a report goes, and what to put on it.
pub struct Webhook {
    pub url: Url,
    /// Extra headers, as the user gave them.
    pub headers: Vec<(String, String)>,
    /// The shared secret, when there is one.
    pub key: Option<Secret>,
}

/// A client that can reach the user's server and nothing else of ours.
pub struct WebhookClient {
    client: reqwest::Client,
    webhook: Webhook,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempt {
    Delivered {
        status: u16,
    },
    /// Worth trying again: the far end was busy, restarting, or unreachable.
    Failed {
        status: Option<u16>,
        error: String,
    },
    /// Not worth trying again. Waiting does not fix a wrong token.
    Refused {
        status: u16,
        error: String,
    },
}

impl WebhookClient {
    /// Builds the client.
    ///
    /// **No credential can reach this.** There is no argument for one, which is
    /// the guard rather than a rule somebody has to keep.
    pub fn new(webhook: Webhook) -> Result<Self> {
        Ok(Self {
            client: http::plain(
                &format!("snob/{}", env!("CARGO_PKG_VERSION")),
                // Not `limited(0)`, which follows nothing but reports a 3xx as
                // an error the same way; `none()` hands the response back so
                // the status can be read and reported. Either way the body does
                // not travel to wherever the redirect pointed.
                reqwest::redirect::Policy::none(),
            )?,
            webhook,
        })
    }

    /// Posts one report.
    ///
    /// `body` is sent verbatim and signed verbatim. It is not re-serialized
    /// here, and `.json()` is deliberately not used: that would render the
    /// value again, and the signature covers bytes.
    /// `event` is what the body says it is, and the header has to agree.
    ///
    /// It used to be the literal `"watch.changes"` on every request, including
    /// heartbeats, whose body says `watch.heartbeat`. The whole point of the
    /// header is that a receiver can route on it without parsing the body —
    /// which is precisely the receiver that would have treated every heartbeat
    /// as a report of changes.
    pub async fn post(&self, body: &str, event: &str, delivery_id: &str, attempt: i64) -> Attempt {
        let mut request = self
            .client
            .post(self.webhook.url.clone())
            .header("Content-Type", "application/json")
            .header(EVENT_HEADER, event)
            .header(DELIVERY_HEADER, delivery_id)
            .header(ATTEMPT_HEADER, attempt.to_string());

        if let Some(key) = &self.webhook.key {
            request = request.header(sign::HEADER, sign::sign(body, key));
        }
        // The user's headers go on last so they can set what they need — but
        // the signature is not among them: `check` refuses a configuration that
        // names it, rather than letting one silently replace the other here.
        //
        // Built as a map and merged, rather than added one at a time.
        // `RequestBuilder::header` is `HeaderMap::append`, so two entries of one
        // name both went out — a configured `Authorization` and a `--header` one
        // meant to replace it, or a configured one and the keyring token. A
        // receiver reading the ordinary single-value accessor sees the first,
        // so the override silently failed and the second value, which was the
        // stored secret, travelled anyway. `insert` is what "the last one wins"
        // needs to be true.
        let mut extra = reqwest::header::HeaderMap::new();
        for (name, value) in &self.webhook.headers {
            // Both were validated by `check` before this client was built, so a
            // failure here is a configuration that never went through it.
            match (
                reqwest::header::HeaderName::try_from(name.as_str()),
                reqwest::header::HeaderValue::try_from(value.as_str()),
            ) {
                (Ok(name), Ok(value)) => {
                    extra.insert(name, value);
                }
                _ => {
                    return Attempt::Refused {
                        status: 0,
                        error: format!("\"{name}\" is not a header this can send"),
                    };
                }
            }
        }
        for (name, value) in extra.iter() {
            request = request.header(name, value);
        }

        match request.body(body.to_string()).send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Attempt::Delivered {
                        status: status.as_u16(),
                    };
                }

                let detail = http::read_capped(response, MAX_RESPONSE_BYTES)
                    .await
                    .unwrap_or_default();
                let error = describe(status, &detail);

                // What waiting can and cannot fix. A 408 and a 429 are the far
                // end asking for time; every other 4xx is it saying the request
                // is wrong, and sending the same request again is knocking on a
                // door that has already been answered.
                if status.is_client_error()
                    && status != reqwest::StatusCode::REQUEST_TIMEOUT
                    && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                {
                    Attempt::Refused {
                        status: status.as_u16(),
                        error,
                    }
                } else {
                    Attempt::Failed {
                        status: Some(status.as_u16()),
                        error,
                    }
                }
            }
            // No response at all: a refused connection, a name that does not
            // resolve, a timeout. All of those are things that come back.
            Err(e) => Attempt::Failed {
                status: None,
                error: e.to_string(),
            },
        }
    }
}

/// What the far end said, in one line.
fn describe(status: reqwest::StatusCode, body: &str) -> String {
    let excerpt: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if excerpt.is_empty() {
        return status.to_string();
    }
    let mut excerpt: String = excerpt.chars().take(200).collect();
    // The body came off a server this tool does not control, so it is filtered
    // like any other text before it reaches a terminal.
    excerpt = snob_core::model::printable(&excerpt);
    format!("{status}: {excerpt}")
}

/// Refuses a destination that should not be posted to before anything is.
///
/// Checked when the address is given rather than at the first run, so a service
/// that would have been shouting a token into the open fails at the moment
/// somebody can still read the message.
pub fn check(webhook: &Webhook) -> Result<()> {
    match webhook.url.scheme() {
        "https" => {}
        "http" if is_private(&webhook.url) => {}
        "http" => bail!(
            "{} is not encrypted, and it is not on a private network.\n\
             The report carries account names, and any header you configured -- a token, \
             usually -- travels with it in the clear. Use https, or an address on your own \
             network.",
            webhook.url
        ),
        other => bail!("\"{other}\" is not an address this can post to; use https"),
    }

    // The signature header is ours, and a configured header that replaced it
    // would leave the receiver checking a value this tool did not compute.
    // Refused here rather than resolved silently at the point of sending.
    if let Some((name, _)) = webhook
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(sign::HEADER))
    {
        bail!("{name} is set by snob itself and cannot be configured");
    }

    // Every header actually has to be one. This used to check the scheme, the
    // host and the collision above, and never tried to build the headers — so
    // `--header "X Token: abc"`, an empty name, or a value with a newline in it
    // all passed, and then every POST died inside reqwest's builder before a
    // socket opened. The report was queued, retried eight times over two hours
    // against an error no waiting could fix, and expired. Every change the
    // monitor ever found was lost, and the only sign was a warning saying it
    // would be tried again.
    for (name, value) in &webhook.headers {
        if reqwest::header::HeaderName::try_from(name.as_str()).is_err() {
            bail!(
                "\"{}\" is not a header name (they may not contain spaces or punctuation \
                 beyond \"-\")",
                snob_core::model::printable(name)
            );
        }
        if reqwest::header::HeaderValue::try_from(value.as_str()).is_err() {
            bail!(
                "the value of \"{}\" is not one a header can carry (a line break, most likely)",
                snob_core::model::printable(name)
            );
        }
    }
    Ok(())
}

/// Whether an address is somewhere plain HTTP is a reasonable thing to speak.
///
/// Loopback, the three private IPv4 ranges, IPv6 unique-local and link-local,
/// and the `.local` and `.internal` suffixes a homelab uses. Anything else is
/// the open internet as far as this can tell, and a token does not go there
/// unencrypted.
fn is_private(url: &Url) -> bool {
    // `url.host()` rather than `host_str()`. The string form of an IPv6 address
    // keeps the brackets the URL syntax requires — `[::1]` — which does not
    // parse as an address, so loopback over IPv6 was refused. The typed host
    // has already done that work and tells the three cases apart.
    match url.host() {
        Some(url::Host::Ipv4(v4)) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        // `is_unique_local` and `is_unicast_link_local` are still unstable, so
        // the prefixes are matched directly: fc00::/7 and fe80::/10.
        Some(url::Host::Ipv6(v6)) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        Some(url::Host::Domain(host)) => {
            let host = host.to_ascii_lowercase();
            host == "localhost"
                || host.ends_with(".localhost")
                || host.ends_with(".local")
                || host.ends_with(".internal")
                || host.ends_with(".home.arpa")
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn webhook(url: &str) -> Webhook {
        Webhook {
            url: Url::parse(url).unwrap(),
            headers: vec![],
            key: None,
        }
    }

    /// A homelab webhook is `http://` on the local network, and refusing that
    /// would make the feature unusable where it is most wanted.
    #[test]
    fn plain_http_is_allowed_on_a_private_network() {
        for url in [
            "http://localhost:5678/webhook",
            "http://127.0.0.1:5678/hook",
            "http://192.168.1.5/hook",
            "http://10.0.0.9/hook",
            "http://172.16.4.1/hook",
            "http://n8n.local/webhook/snob",
            "http://box.home.arpa/hook",
            "http://[::1]:5678/hook",
        ] {
            assert!(check(&webhook(url)).is_ok(), "{url} should be allowed");
        }
    }

    /// And refused anywhere else, because the token travels with it.
    #[test]
    fn plain_http_to_the_open_internet_is_refused_with_the_reason() {
        let error = check(&webhook("http://example.com/hook")).unwrap_err();
        assert!(error.to_string().contains("not encrypted"), "{error}");

        for url in ["http://8.8.8.8/hook", "http://n8n.example.com/hook"] {
            assert!(check(&webhook(url)).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn https_is_allowed_anywhere() {
        assert!(check(&webhook("https://n8n.example.com/webhook/snob")).is_ok());
    }

    #[test]
    fn a_scheme_that_is_not_http_is_refused() {
        for url in ["ftp://host/x", "file:///tmp/x"] {
            assert!(check(&webhook(url)).is_err(), "{url} should be refused");
        }
    }

    /// A configured header that replaced the signature would leave the receiver
    /// verifying something this tool did not compute — which is worse than no
    /// signature, because it looks like one.
    #[test]
    fn a_configured_header_cannot_replace_the_signature() {
        let mut hook = webhook("https://example.com/hook");
        hook.headers
            .push(("x-snob-signature".into(), "anything".into()));

        let error = check(&hook).unwrap_err();
        assert!(error.to_string().contains("set by snob itself"), "{error}");
    }

    #[test]
    fn an_ordinary_header_is_fine() {
        let mut hook = webhook("https://example.com/hook");
        hook.headers
            .push(("Authorization".into(), "Bearer x".into()));
        assert!(check(&hook).is_ok());
    }
}
