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
    /// Worth trying again: the far end was busy, restarting, unreachable, or
    /// answered with a status it may not answer with next time.
    ///
    /// **Every HTTP answer lands here, 4xx included.** A 4xx used to be a
    /// refusal, which the outbox expires with no retries — and the mark has
    /// already moved by then, so one 404 from an n8n workflow that happened not
    /// to be registered threw away the only copy of a set of arrivals and
    /// departures.
    Failed {
        status: Option<u16>,
        error: String,
    },
    /// The request could not be sent at all, so there is nothing to try again.
    ///
    /// Not "the server said no": a configuration that never went through
    /// [`check`] and produces a header this cannot build. Retrying it sends the
    /// identical unbuildable request.
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
        // One map for the whole request: snob's headers first, then the user's
        // inserted over them, and the map handed over in one go.
        //
        // Two rounds of getting this wrong. `RequestBuilder::header` is
        // `HeaderMap::append`, so two entries of one name both went out — a
        // configured `Authorization` and a `--header` one meant to replace it,
        // or a configured one and the keyring token. Building the user's list
        // into a map with `insert` fixed that among the user's own headers, and
        // then replayed the map with `header` again: so a configured
        // `Content-Type: application/json; charset=utf-8` still went out
        // alongside snob's, and a receiver reading the single-value accessor
        // saw snob's while the override silently did nothing.
        //
        // `HeaderMap` is case-insensitive, so `insert` over the whole map is
        // what "the last one wins" needs to be true. What the user must not be
        // able to replace is refused by `check`, not resolved quietly here.
        let mut headers = reqwest::header::HeaderMap::new();
        let mut own = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (EVENT_HEADER.to_string(), event.to_string()),
            (DELIVERY_HEADER.to_string(), delivery_id.to_string()),
            (ATTEMPT_HEADER.to_string(), attempt.to_string()),
        ];
        if let Some(key) = &self.webhook.key {
            own.push((sign::HEADER.to_string(), sign::sign(body, key)));
        }
        for (name, value) in own.iter().chain(&self.webhook.headers) {
            // Both were validated by `check` before this client was built, so a
            // failure here is a configuration that never went through it.
            match (
                reqwest::header::HeaderName::try_from(name.as_str()),
                reqwest::header::HeaderValue::try_from(value.as_str()),
            ) {
                (Ok(name), Ok(value)) => {
                    headers.insert(name, value);
                }
                _ => {
                    return Attempt::Refused {
                        status: 0,
                        error: format!("\"{name}\" is not a header this can send"),
                    };
                }
            }
        }
        let request = self.client.post(self.webhook.url.clone()).headers(headers);

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

                // Every answer is worth another try, including a 4xx.
                //
                // A 4xx used to come back as `Refused`, which the outbox turns
                // into "expired, no retries", and the mark had already moved —
                // so one 404 threw away the only copy of a set of arrivals and
                // departures. And the 4xx a webhook actually gives are mostly
                // transient: n8n answers 404 for a workflow that is not
                // currently registered, a reverse proxy answers 404 or 403 while
                // it reloads, an expired bearer token answers 401. The far end
                // is the user's own server, so eight attempts over five hours
                // costs nothing that matters, and the attempt and age bounds
                // still stop it going on for ever.
                //
                // `Refused` is left for a request that could not be sent at all,
                // which is the only failure retrying genuinely cannot change.
                Attempt::Failed {
                    status: Some(status.as_u16()),
                    error,
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
    // A password in the address is a credential in a plain-text file, which is
    // the one thing `watch.toml` promises not to hold — and `reqwest` turns it
    // into a second `Authorization` header, so it also collides with the one
    // the user configured. It would be echoed by `status` and written into the
    // log of an unattended service by the refusal below.
    if !webhook.url.username().is_empty() || webhook.url.password().is_some() {
        bail!(
            "the address carries a username or password. Put the credential in a header \
             instead -- \"snob watch setup\" stores one in the keyring -- so it is not sitting \
             in a configuration file and in every log line that names the address."
        );
    }

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

    // Headers the user does not get to set.
    //
    // Two kinds, and both are refused here rather than resolved silently at the
    // point of sending. `X-Snob-*` is the protocol: the signature covers bytes
    // this tool computed, `X-Snob-Event` is what a receiver routes on without
    // parsing the body, and `X-Snob-Delivery` is what it deduplicates on — a
    // configured one of those makes the value ambiguous, and most frameworks
    // join duplicates with ", ".
    //
    // The rest frame the message, and `Content-Length` is the one that matters:
    // hyper honors a caller-supplied one in preference to measuring the body, so
    // `Content-Length = "0"` sent the POST with no body at all and
    // `Content-Length = "4"` sent four bytes of it — while `X-Snob-Signature`,
    // computed over the whole document, still claimed the whole document. A
    // receiver that verifies rejects every attempt; one that does not silently
    // ingests an empty report of changes. That is the exact-bytes guarantee the
    // whole design rests on, broken at the last hop where nothing checks it.
    //
    // `Content-Type` is deliberately **not** here. Asking for
    // `application/json; charset=utf-8` is an ordinary thing to want, and `post`
    // now builds one map so the user's value replaces snob's rather than
    // travelling beside it.
    const FRAMING: [&str; 5] = [
        "content-length",
        "transfer-encoding",
        "connection",
        "host",
        "expect",
    ];
    for (name, _) in &webhook.headers {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("x-snob-") {
            bail!(
                "{} is part of what snob sends and cannot be configured",
                snob_core::model::printable(name)
            );
        }
        if FRAMING.contains(&lower.as_str()) {
            bail!(
                "{} is set by the transport and cannot be configured: a wrong one truncates or \
                 empties the report while the signature still covers all of it.",
                snob_core::model::printable(name)
            );
        }
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
        // An empty value, for the same reason `--sign-with ""` is refused: it is
        // `--header "Authorization: ${SNOB_TOKEN}"` in a unit file whose
        // variable is not set. Taken literally it sends an empty
        // `Authorization`, and because a header given by name is what stops the
        // keyring token being attached, it *replaces* the stored token with
        // nothing. The receiver answers 401, a 401 is a refusal rather than a
        // retry, and the change that report carried is gone.
        if value.trim().is_empty() {
            bail!(
                "\"{}\" was given an empty value. If that came from an environment variable that \
                 is not set, leave the header out: a header with nothing in it replaces whatever \
                 \"snob watch setup\" stored under the same name.",
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
        Some(url::Host::Ipv4(v4)) => is_private_v4(v4),
        Some(url::Host::Ipv6(v6)) => {
            // An IPv4-mapped address is an IPv4 address written the long way,
            // so `[::ffff:127.0.0.1]` is loopback and was being refused.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_v4(v4);
            }
            // The same endpoint as `is_private_v4`'s exclusion, reachable by two
            // more spellings: `fd00:ec2::254` is AWS's documented IMDS over
            // IPv6 and sits inside fc00::/7, and `fe80::a9fe:a9fe` is the
            // link-local form. Both were allowed while the IPv4 address was
            // refused with a paragraph explaining why.
            if METADATA_V6.contains(&v6) {
                return false;
            }
            // `is_unique_local` and `is_unicast_link_local` are still unstable,
            // so the prefixes are matched directly: fc00::/7 and fe80::/10.
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        Some(url::Host::Domain(host)) => {
            // One trailing dot is the fully-qualified form of the same name,
            // and `n8n.local.` resolves exactly where `n8n.local` does.
            let host = host.to_ascii_lowercase();
            let host = host.strip_suffix('.').unwrap_or(&host);
            // And the same endpoint again, by the name its own documentation
            // tells you to use. `metadata.google.internal` resolves to
            // 169.254.169.254, and the `.internal` suffix below — a deliberate
            // affordance for a homelab — let it straight through, so the block
            // held only against the one spelling nobody types.
            if METADATA_HOSTS.contains(&host) {
                return false;
            }
            host == "localhost"
                || host.ends_with(".localhost")
                || host.ends_with(".local")
                || host.ends_with(".internal")
                || host.ends_with(".home.arpa")
        }
        None => false,
    }
}

/// The cloud metadata service, by name.
///
/// Beside [`is_private_v4`]'s address rather than instead of it: a denylist of
/// one spelling is not a denylist. See that function for what is at stake.
const METADATA_HOSTS: [&str; 3] = ["metadata", "metadata.google.internal", "metadata.goog"];

/// The cloud metadata service, over IPv6.
const METADATA_V6: [std::net::Ipv6Addr; 2] = [
    // AWS's documented IMDS over IPv6.
    std::net::Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254),
    // The link-local form of 169.254.169.254.
    std::net::Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0xa9fe, 0xa9fe),
];

/// Whether an IPv4 address is somewhere plain HTTP is reasonable.
///
/// **169.254.169.254 is excluded**, though it is link-local and the rest of
/// that range is allowed. It is the cloud instance metadata endpoint on every
/// major provider, and it answers credentials to anything that asks. A mistyped
/// address is one thing; a mistyped address that POSTs the body and the stored
/// token at the hypervisor is another.
///
/// `0.0.0.0` is treated as loopback: it means "this host" and nothing routes to
/// it, so refusing it only made the local case harder to write.
fn is_private_v4(v4: std::net::Ipv4Addr) -> bool {
    const METADATA: std::net::Ipv4Addr = std::net::Ipv4Addr::new(169, 254, 169, 254);
    if v4 == METADATA {
        return false;
    }
    v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
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
            // The same three written the other ways people write them.
            "http://n8n.local./webhook/snob",
            "http://[::ffff:127.0.0.1]:5678/hook",
            "http://0.0.0.0:5678/hook",
        ] {
            assert!(check(&webhook(url)).is_ok(), "{url} should be allowed");
        }
    }

    /// Link-local, and allowed by that rule — but it is the cloud metadata
    /// endpoint, which answers credentials to whatever asks. A typo that sends
    /// the body and the stored token to the hypervisor is not a typo worth
    /// being permissive about.
    #[test]
    fn the_cloud_metadata_endpoint_is_refused_though_it_is_link_local() {
        assert!(check(&webhook("http://169.254.169.254/hook")).is_err());
        // The rest of the range is still fine.
        assert!(check(&webhook("http://169.254.4.4/hook")).is_ok());
    }

    /// And by every spelling, not only the one nobody types.
    ///
    /// The address was refused with a paragraph explaining why, while the same
    /// endpoint was reachable through the sibling branches: GCP publishes
    /// `metadata.google.internal`, which the `.internal` affordance for homelabs
    /// let straight through, and AWS publishes `fd00:ec2::254`, which sits
    /// inside the fc00::/7 range. Both over plain HTTP, carrying the report and
    /// the stored token.
    #[test]
    fn the_metadata_endpoint_is_refused_by_name_and_over_ipv6() {
        for url in [
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://metadata.google.internal./computeMetadata/v1/",
            "http://METADATA.GOOGLE.INTERNAL/computeMetadata/v1/",
            "http://metadata/computeMetadata/v1/",
            "http://metadata.goog/computeMetadata/v1/",
            "http://[fd00:ec2::254]/latest/meta-data/",
            "http://[fe80::a9fe:a9fe]/latest/meta-data/",
        ] {
            assert!(check(&webhook(url)).is_err(), "{url} was allowed");
        }

        // And the homelab addresses those branches exist for still work.
        for url in [
            "http://n8n.internal/hook",
            "http://[fd00::1]/hook",
            "http://[fe80::1]/hook",
        ] {
            assert!(check(&webhook(url)).is_ok(), "{url} was refused");
        }
    }

    /// A password in the address is a credential in a plain-text file, which is
    /// what the configuration promises not to hold — and it would be echoed
    /// into an unattended service's log by the refusal path.
    #[test]
    fn a_credential_in_the_address_is_refused() {
        for url in [
            "https://alice:s3cret@example.com/hook",
            "https://token@example.com/hook",
        ] {
            let error = check(&webhook(url)).unwrap_err();
            assert!(
                error.to_string().contains("username or password"),
                "{error}"
            );
        }
    }

    /// A header that reqwest cannot build is refused where the address is, not
    /// discovered when every POST dies in the builder and the report is retried
    /// for two hours against an error no waiting can fix.
    #[test]
    fn a_header_that_could_never_be_sent_is_refused_up_front() {
        for (name, value) in [
            ("X Token", "abc"),
            ("", "abc"),
            ("X-Token", "line\r\nInjected: yes"),
        ] {
            let mut hook = webhook("https://example.com/hook");
            hook.headers.push((name.into(), value.into()));
            assert!(
                check(&hook).is_err(),
                "\"{name}: {value}\" should be refused"
            );
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
        assert!(
            error.to_string().contains("part of what snob sends"),
            "{error}"
        );
    }

    /// Nor any other header the protocol owns.
    ///
    /// `X-Snob-Event` is what a receiver routes on without parsing the body and
    /// `X-Snob-Delivery` is what it deduplicates on; a configured one of either
    /// went out beside snob's, and most frameworks join duplicates with ", ".
    #[test]
    fn a_configured_header_cannot_replace_any_of_the_protocol_ones() {
        for name in ["X-Snob-Event", "x-snob-delivery", "X-Snob-Attempt"] {
            let mut hook = webhook("https://example.com/hook");
            hook.headers.push((name.into(), "mine".into()));
            assert!(check(&hook).is_err(), "{name} was accepted");
        }
    }

    /// A header that frames the message is not the user's to set.
    ///
    /// `Content-Length = "0"` sent the POST with no body at all, and `"4"` sent
    /// four bytes — while the signature, computed over the whole document, still
    /// claimed the whole document. A receiver that verifies rejects every
    /// attempt; one that does not ingests an empty report of changes.
    #[test]
    fn a_header_that_frames_the_message_is_refused() {
        for name in ["Content-Length", "transfer-encoding", "Host", "Connection"] {
            let mut hook = webhook("https://example.com/hook");
            hook.headers.push((name.into(), "0".into()));
            let error = check(&hook).unwrap_err();
            assert!(
                error.to_string().contains("set by the transport"),
                "{name}: {error}"
            );
        }
    }

    /// An empty value is a variable that was not set, and it silently replaces
    /// the stored token with nothing.
    ///
    /// `--sign-with ""` is refused with a paragraph about exactly this; the
    /// header never got the guard. A header given by name is what stops the
    /// keyring token being attached, so `--header "Authorization: ${UNSET}"`
    /// dropped the token, sent an empty one, and the 401 that came back is a
    /// refusal rather than a retry.
    #[test]
    fn a_header_with_an_empty_value_is_refused() {
        for value in ["", "   "] {
            let mut hook = webhook("https://example.com/hook");
            hook.headers.push(("Authorization".into(), value.into()));
            let error = check(&hook).unwrap_err();
            assert!(error.to_string().contains("empty value"), "{error}");
        }
    }

    #[test]
    fn an_ordinary_header_is_fine() {
        let mut hook = webhook("https://example.com/hook");
        hook.headers
            .push(("Authorization".into(), "Bearer x".into()));
        assert!(check(&hook).is_ok());

        // Including one snob also sends, now that `post` builds a single map and
        // the user's value replaces snob's instead of travelling beside it.
        let mut hook = webhook("https://example.com/hook");
        hook.headers.push((
            "Content-Type".into(),
            "application/json; charset=utf-8".into(),
        ));
        assert!(check(&hook).is_ok());
    }
}
