//! The headers a browser sends alongside its User-Agent, derived from it.
//!
//! The goal is **coherence, not disguise**. Instagram's own edge answers
//! `Vary: Sec-Fetch-Site, Sec-Fetch-Mode`, which is it saying out loud that
//! those headers change its reply; and a request claiming to be Chrome 151
//! while sending no client hints at all is a combination Chrome cannot produce.
//! Being inconsistent is worse than being plain, so everything here is computed
//! from one source — the User-Agent the session was created with — and anything
//! that cannot be computed from it is left out rather than guessed.
//!
//! Nothing here touches the TLS stack. Chrome has randomized its ClientHello
//! extension order since version 110, so there is no fixed TLS fingerprint left
//! to match, and a stable one across hundreds of connections is more anomalous
//! than any particular one. What actually gets an account throttled, in order,
//! is IP reputation, request volume and pace, and header coherence — and only
//! the last of those is ours to fix here.

/// Build counter of Instagram's own web bundle.
///
/// A constant, not a session token, whatever the search results say: real
/// captures show a handful of fixed values that move when the bundle is
/// rebuilt. It is the lowest-value header of the set and is sent only because
/// the browser sends it.
pub const ASBD_ID: &str = "198387";

/// What the browser behind a User-Agent would say about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// `sec-ch-ua`, already formatted. `None` for browsers that do not send
    /// client hints at all, which is every non-Chromium one.
    pub ua_brands: Option<String>,
    /// `sec-ch-ua-platform`, quoted, from the enumerated list.
    pub platform: &'static str,
    /// `sec-ch-ua-mobile`, an sf-boolean.
    pub mobile: &'static str,
}

impl Fingerprint {
    /// Reads a User-Agent and works out what else the browser would send.
    pub fn from_user_agent(user_agent: &str) -> Self {
        Self {
            ua_brands: chromium_major(user_agent).map(|major| brands(brand_of(user_agent), major)),
            platform: platform_of(user_agent),
            mobile: if is_mobile(user_agent) { "?1" } else { "?0" },
        }
    }
}

/// The major version out of a Chromium User-Agent, or `None` when it is not one.
///
/// Since Chrome reduced its User-Agent, the minor, build and patch parts are
/// frozen at zero, so the major is the only thing that varies — and the only
/// thing needed to reproduce the rest.
fn chromium_major(user_agent: &str) -> Option<u32> {
    let after = user_agent.split("Chrome/").nth(1)?;
    after.split('.').next()?.parse().ok()
}

/// The brand a Chromium derivative reports as its own. The order matters:
/// every one of them also says `Chrome/` somewhere.
fn brand_of(user_agent: &str) -> &'static str {
    if user_agent.contains("Edg/") {
        "Microsoft Edge"
    } else if user_agent.contains("OPR/") {
        "Opera"
    } else {
        "Google Chrome"
    }
}

/// Builds `sec-ch-ua` exactly as Chromium does.
///
/// Three entries — a deliberately meaningless one, Chromium, and the brand —
/// where **both the fake brand's spelling and the order of the three are
/// derived from the major version**. Hardcoding "the fake one goes first" is
/// wrong for two versions out of every three, and since the whole thing is a
/// pure function of the major, the other end can check it for free.
///
/// The algorithm is Chromium's `GetGreasedUserAgentBrandVersion`, kept here
/// because there is nowhere to read it from at runtime.
fn brands(brand: &str, major: u32) -> String {
    const GREASE_CHARS: [&str; 11] = [" ", "(", ":", "-", ".", "/", ")", ";", "=", "?", "_"];
    const GREASE_VERSIONS: [&str; 3] = ["8", "99", "24"];
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];

    let seed = major as usize;
    let greased = format!(
        "\"Not{}A{}Brand\";v=\"{}\"",
        GREASE_CHARS[seed % 11],
        GREASE_CHARS[(seed + 1) % 11],
        GREASE_VERSIONS[seed % 3],
    );

    let entries = [
        greased,
        format!("\"Chromium\";v=\"{major}\""),
        format!("\"{brand}\";v=\"{major}\""),
    ];

    let order = ORDERS[seed % 6];
    let mut shuffled = [""; 3];
    for (i, entry) in entries.iter().enumerate() {
        shuffled[order[i]] = entry;
    }
    shuffled.join(", ")
}

/// The `sec-ch-ua-platform` value, from the enumerated list the specification
/// allows. Already quoted, because it is a structured-header string.
fn platform_of(user_agent: &str) -> &'static str {
    if user_agent.contains("Android") {
        "\"Android\""
    } else if user_agent.contains("iPhone") || user_agent.contains("iPad") {
        "\"iOS\""
    } else if user_agent.contains("CrOS") {
        "\"Chrome OS\""
    } else if user_agent.contains("Windows") {
        "\"Windows\""
    } else if user_agent.contains("Macintosh") || user_agent.contains("Mac OS X") {
        "\"macOS\""
    } else if user_agent.contains("Linux") || user_agent.contains("X11") {
        "\"Linux\""
    } else {
        "\"Unknown\""
    }
}

fn is_mobile(user_agent: &str) -> bool {
    user_agent.contains("Mobile") || user_agent.contains("Android")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop(major: u32) -> String {
        format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
        )
    }

    /// Checked against real captures. If this drifts, every request the tool
    /// makes carries a combination Chrome never produces.
    #[test]
    fn the_brand_list_matches_what_chromium_builds() {
        assert_eq!(
            brands("Google Chrome", 128),
            r#""Chromium";v="128", "Not;A=Brand";v="24", "Google Chrome";v="128""#
        );
        assert_eq!(
            brands("Google Chrome", 131),
            r#""Google Chrome";v="131", "Chromium";v="131", "Not_A Brand";v="24""#
        );
        assert_eq!(
            brands("Google Chrome", 141),
            r#""Google Chrome";v="141", "Not?A_Brand";v="8", "Chromium";v="141""#
        );
    }

    /// The order of the three entries is derived from the version, not fixed.
    /// Assuming a constant order is wrong two times out of three.
    #[test]
    fn the_order_moves_with_the_version() {
        let first_entry = |major| {
            brands("Google Chrome", major)
                .split(';')
                .next()
                .unwrap()
                .to_string()
        };
        let seen: std::collections::HashSet<String> = (140..146).map(first_entry).collect();
        assert!(
            seen.len() > 1,
            "the leading brand has to change with the version, got {seen:?}"
        );
    }

    #[test]
    fn a_derivative_names_itself_and_keeps_chromium() {
        let edge = brands("Microsoft Edge", 151);
        assert!(edge.contains(r#""Microsoft Edge";v="151""#), "{edge}");
        assert!(edge.contains(r#""Chromium";v="151""#), "{edge}");
    }

    #[test]
    fn the_major_version_is_read_from_the_user_agent() {
        assert_eq!(chromium_major(&desktop(151)), Some(151));
        assert_eq!(chromium_major("Chrome/99.0.4844.51"), Some(99));
        assert_eq!(chromium_major("curl/8.0"), None);
    }

    /// Firefox and Safari send no client hints at all, so inventing them for
    /// their User-Agent would be a mismatch rather than an improvement.
    #[test]
    fn a_browser_that_sends_no_hints_gets_none() {
        let firefox =
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:142.0) Gecko/20100101 Firefox/142.0";
        let f = Fingerprint::from_user_agent(firefox);
        assert_eq!(f.ua_brands, None);
        // The platform is still known, and it still has to agree with the UA.
        assert_eq!(f.platform, "\"Windows\"");
    }

    /// The pairing that gives a client away fastest: a User-Agent naming one
    /// system and a platform hint naming another.
    #[test]
    fn the_platform_agrees_with_the_user_agent() {
        let cases = [
            ("Windows NT 10.0; Win64; x64", "\"Windows\""),
            ("Macintosh; Intel Mac OS X 10_15_7", "\"macOS\""),
            ("X11; Linux x86_64", "\"Linux\""),
            ("X11; CrOS x86_64 14541.0.0", "\"Chrome OS\""),
        ];
        for (platform, expected) in cases {
            let ua = format!(
                "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/151.0.0.0 Safari/537.36"
            );
            let f = Fingerprint::from_user_agent(&ua);
            assert_eq!(f.platform, expected, "{ua}");
            assert_eq!(f.mobile, "?0");
        }
    }

    #[test]
    fn a_phone_says_so() {
        let android = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 \
                       (KHTML, like Gecko) Chrome/151.0.0.0 Mobile Safari/537.36";
        let f = Fingerprint::from_user_agent(android);
        assert_eq!(f.mobile, "?1");
        assert_eq!(f.platform, "\"Android\"");
    }

    #[test]
    fn a_full_desktop_fingerprint_is_coherent() {
        let f = Fingerprint::from_user_agent(&desktop(141));
        assert_eq!(
            f.ua_brands.as_deref(),
            Some(r#""Google Chrome";v="141", "Not?A_Brand";v="8", "Chromium";v="141""#)
        );
        assert_eq!(f.platform, "\"Windows\"");
        assert_eq!(f.mobile, "?0");
    }
}
