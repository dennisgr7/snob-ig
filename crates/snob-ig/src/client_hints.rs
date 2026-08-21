//! The User-Agent Client Hints that go with a request, derived from the
//! User-Agent itself.
//!
//! These are ordinary HTTP request headers with a written specification:
//! `Sec-CH-UA`, `Sec-CH-UA-Platform`, `Sec-CH-UA-Mobile`, `Priority` and
//! `Accept-Language`. Instagram's own edge answers
//! `Vary: Sec-Fetch-Site, Sec-Fetch-Mode`, which is the server saying out loud
//! that headers of this kind change its reply — so getting them right is a
//! correctness requirement, not a nicety.
//!
//! **The rule is that the set has to agree with itself.** A request that
//! declares Chrome 151 in its User-Agent and then sends no client hints at all
//! describes a browser that does not exist, and a server is entitled to answer
//! nonsense however it likes. So every value here is *computed* from the
//! User-Agent the session was actually created with, and anything that cannot
//! be computed is left out rather than guessed: an absent header is honest, an
//! invented one is not.
//!
//! `Accept-Language` is the one exception, and has to be — it describes the
//! person rather than the program, so it is read from the operating system.
//! That is still deriving it from something true rather than picking a value.
//!
//! Two things this module deliberately does not do. It does not touch **the
//! TLS handshake, the HTTP/2 SETTINGS or the order of the headers on the
//! wire**, and it does not invent a browser — the User-Agent comes from one
//! actually installed on the machine (see `browser.rs`), and everything here
//! follows from that.
//!
//! The reason given for the first of those used to be that Chrome randomizes
//! its ClientHello extension order, so there is nothing stable to copy. The
//! premise is true and the conclusion has not held since 2023: JA4, which is
//! what fingerprinting moved to, sorts the extension list before hashing it,
//! precisely so that the shuffling changes nothing. There *is* something
//! stable to copy. Three reasons that do hold, in the order they matter:
//!
//! - **It is detection evasion, and that is not what this tool is for.**
//!   Matching a browser's cryptographic identity is not making a request
//!   honestly; it is making a program harder to recognize as a program. The
//!   goal here is to lower the risk to a real account, not to be harder to
//!   catch, and those two come apart exactly here.
//! - Copying a handshake means leaving `rustls`, and with it the clean static
//!   cross-compilation to five targets that is most of what "single binary, no
//!   runtime" costs to keep.
//! - It would buy nothing anyway. What decides whether Instagram throttles an
//!   account is, in order, the address the requests come from, how many there
//!   are, and how fast.
//!
//! This applies to the whole wire signature and not only to TLS, which is
//! worth saying plainly because the next version of the argument arrives as
//! `http2_initial_stream_window_size` and the Akamai h2 fingerprint. Same
//! answer, same three reasons.
//!
//! Headers are also not the lever that matters. What determines whether
//! Instagram throttles an account is, in order, the address the requests come
//! from, how many there are, and how fast. Only the last two are the project's
//! to control, and they live in `pace.rs`.

/// From which Chromium sends `Priority` on a fetch or XHR.
///
/// Below this it does not send the header at all, so a request claiming to be
/// an older Chrome and carrying one would be the same kind of mismatch as
/// inventing client hints for Firefox.
const PRIORITY_SINCE_CHROMIUM: u32 = 123;

/// What Chromium sends on a fetch it did not prioritize itself: the default
/// urgency, and incremental delivery.
pub const FETCH_PRIORITY: &str = "u=1, i";

/// And what it sends on a top-level navigation, which is the highest urgency
/// there is: nothing on the page can start until the document arrives.
///
/// Split from the constant above because they are different requests, and the
/// name of that one says so -- it is what Chromium sends **on a fetch**, and it
/// was going out on the page fetch in `IgClient::page`, which is a navigation.
///
/// **Not read off the August 2026 capture.** That capture kept the headers the
/// page set rather than the ones the network stack added, and `Priority` is one
/// of the latter, so no value for it survived. This is the urgency RFC 9218
/// describes for a main-frame document and that Chromium sends for one. It is
/// written down here rather than assumed, so that the next capture -- one taken
/// with the wire headers included -- has something to confirm or correct.
pub const NAVIGATION_PRIORITY: &str = "u=0, i";

/// From which Chromium offers `zstd` in `Accept-Encoding`.
///
/// The same release as [`PRIORITY_SINCE_CHROMIUM`], and a constant of its own on
/// purpose: two facts that happened to ship together, and folding them into one
/// number would make a later divergence look like a typo. This header was a
/// literal for a long time and was never inside this module's stated scope, so a
/// session created with a Chrome 120 User-Agent correctly left `Priority` off
/// and then offered a codec that browser cannot decode — the self-contradiction
/// the rest of this module exists to prevent, on an axis the other end can check
/// for nothing.
const ZSTD_SINCE_CHROMIUM: u32 = 123;

/// What a current browser offers to have its answers compressed with.
///
/// Stated here rather than left to the HTTP client, which builds its own from
/// whichever decoders were compiled in and produces `zstd,gzip,deflate,br` — a
/// fixed string, on every request, that no browser has ever sent. This is
/// Chrome's, and every codec in it is one the client can actually decode.
///
/// There is deliberately no non-Chromium branch. Firefox has offered the same
/// four since 126, so this is the honest value for a browser this module knows
/// nothing else about, and inventing a different one is the thing the module
/// refuses to do.
pub const ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";

/// What Chromium offered before it could decode `zstd`.
pub const ACCEPT_ENCODING_BEFORE_ZSTD: &str = "gzip, deflate, br";

/// Language preference, as a browser would state it.
///
/// Every browser sends `Accept-Language` on every request without exception,
/// so having none at all was the most conspicuous thing missing from the set.
/// It is also the one header here that cannot be derived from the User-Agent:
/// it comes from the person, not the program.
///
/// So it is read from the system, and the fallback is the last resort rather
/// than the plan. A Spanish user on a Spanish address announcing `en-US` is
/// the same class of incoherence as a Windows User-Agent with a macOS platform
/// hint — Instagram can see the address the request came from, and it knows
/// which language the account reads in.
///
/// Worked out once: the answer cannot change while the process runs, and this
/// is on the path of every request.
pub fn accept_language() -> &'static str {
    static VALUE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VALUE.get_or_init(|| language_header(system_locale().as_deref()))
}

/// Builds the header value from a locale tag such as `es-ES`.
///
/// `es-ES,es;q=0.9` is the shape Chrome produces for a browser configured with
/// one language, and one language is what a system locale describes. English
/// is deliberately **not** appended as a second preference: Chrome only lists
/// what the user actually configured, and inventing an extra entry would be
/// guessing at the person rather than reading them.
fn language_header(locale: Option<&str>) -> String {
    const FALLBACK: &str = "en-US,en;q=0.9";

    let Some(tag) = locale.map(normalize_tag).filter(|t| !t.is_empty()) else {
        return FALLBACK.to_string();
    };
    match tag.split_once('-') {
        Some((language, _)) => format!("{tag},{language};q=0.9"),
        // A bare language with no region is already its own primary
        // preference, and repeating it at a lower weight is not something a
        // browser does.
        None => tag,
    }
}

/// Turns what a platform hands back into a language tag.
///
/// Unix locales arrive as `es_ES.UTF-8` and may be the C locale, which
/// describes no language at all and must not become one.
fn normalize_tag(raw: &str) -> String {
    let tag = raw
        .split(['.', '@'])
        .next()
        .unwrap_or("")
        .trim()
        .replace('_', "-");

    if matches!(tag.as_str(), "C" | "POSIX") || !tag.starts_with(|c: char| c.is_ascii_alphabetic())
    {
        return String::new();
    }
    tag
}

/// The user's configured language, as the operating system states it.
#[cfg(windows)]
fn system_locale() -> Option<String> {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;

    /// `LOCALE_NAME_MAX_LENGTH`, which the binding does not export. Windows
    /// documents it as the ceiling for every locale name it will produce.
    const MAX_LOCALE_UNITS: usize = 85;

    let mut buffer = [0u16; MAX_LOCALE_UNITS];
    // SAFETY: the buffer and the length handed over describe the same array,
    // and Windows writes at most that many units into it.
    let written = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) };
    if written <= 1 {
        return None;
    }
    // The count includes the terminating null.
    String::from_utf16(&buffer[..written as usize - 1]).ok()
}

/// Elsewhere the environment is where this lives. `LC_ALL` overrides `LANG`,
/// which is the order the C library itself resolves them in.
#[cfg(not(windows))]
fn system_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
}

/// Build counter of Instagram's own web bundle.
///
/// A constant, not a session token, whatever the search results say: real
/// captures show a handful of fixed values that move when the bundle is
/// rebuilt. It is the lowest-value header of the set and is sent only because
/// the browser sends it.
///
/// **Updated from a capture rather than from a search.** Chrome 151 on
/// instagram.com in August 2026 sends `359341`; the `198387` this held before
/// is an older bundle's, and a value that stale is the kind of small
/// inconsistency the header set exists to avoid. It moves when Instagram
/// rebuilds, so it will go stale again — which is why it is worth a note that
/// the way to refresh it is to look, not to guess.
pub const ASBD_ID: &str = "359341";

/// What the browser behind a User-Agent would say about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHints {
    /// `sec-ch-ua`, already formatted. `None` for browsers that do not send
    /// client hints at all, which is every non-Chromium one.
    pub ua_brands: Option<String>,
    /// `sec-ch-ua-platform`, quoted, from the enumerated list.
    pub platform: &'static str,
    /// `sec-ch-ua-mobile`, an sf-boolean.
    pub mobile: &'static str,
    /// `priority`, for the browsers that send one on a fetch. `None` for the
    /// rest, and for Chromium old enough to predate it.
    pub priority: Option<&'static str>,
    /// `accept-encoding`: the codecs this browser would offer. Never absent,
    /// because every browser sends one — what moves with the version is which
    /// codecs are in it.
    pub accept_encoding: &'static str,
}

impl ClientHints {
    /// Reads a User-Agent and works out what else the browser would send.
    pub fn from_user_agent(user_agent: &str) -> Self {
        let major = chromium_major(user_agent);
        Self {
            ua_brands: major.map(|major| brands(brand_of(user_agent), major)),
            platform: platform_of(user_agent),
            mobile: if is_mobile(user_agent) { "?1" } else { "?0" },
            priority: major
                .filter(|major| *major >= PRIORITY_SINCE_CHROMIUM)
                .map(|_| FETCH_PRIORITY),
            accept_encoding: match major {
                Some(major) if major < ZSTD_SINCE_CHROMIUM => ACCEPT_ENCODING_BEFORE_ZSTD,
                _ => ACCEPT_ENCODING,
            },
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
/// Three entries — the GREASE entry, Chromium, and the brand — where **both the
/// GREASE entry's spelling and the order of the three are derived from the
/// major version**. GREASE is the standard trick of including a deliberately
/// meaningless value so that parsers cannot come to depend on the list being
/// fixed; it is part of the header, not an embellishment on it. Hardcoding "the
/// GREASE entry goes first" is wrong for two versions out of every three, and
/// since the whole thing is a pure function of the major version, the other end
/// can check it for free.
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
        let f = ClientHints::from_user_agent(firefox);
        assert_eq!(f.ua_brands, None);
        // The platform is still known, and it still has to agree with the UA.
        assert_eq!(f.platform, "\"Windows\"");
    }

    /// The contradiction this module exists to prevent: a User-Agent naming one
    /// operating system and a platform hint naming another.
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
            let f = ClientHints::from_user_agent(&ua);
            assert_eq!(f.platform, expected, "{ua}");
            assert_eq!(f.mobile, "?0");
        }
    }

    #[test]
    fn a_phone_says_so() {
        let android = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 \
                       (KHTML, like Gecko) Chrome/151.0.0.0 Mobile Safari/537.36";
        let f = ClientHints::from_user_agent(android);
        assert_eq!(f.mobile, "?1");
        assert_eq!(f.platform, "\"Android\"");
    }

    /// The shape Chrome produces for a browser configured with one language,
    /// which is what a system locale describes.
    #[test]
    fn the_language_header_follows_the_locale() {
        assert_eq!(language_header(Some("es-ES")), "es-ES,es;q=0.9");
        assert_eq!(language_header(Some("en-US")), "en-US,en;q=0.9");
        // Unix hands it over with an encoding and an underscore attached.
        assert_eq!(language_header(Some("pt_BR.UTF-8")), "pt-BR,pt;q=0.9");
        // A language with no region is already its own preference; a browser
        // does not repeat it at a lower weight.
        assert_eq!(language_header(Some("eu")), "eu");
    }

    /// The C locale describes no language at all, and must not become one.
    /// Neither must an empty or nonsense value.
    #[test]
    fn a_locale_that_names_no_language_falls_back() {
        for nothing in [None, Some(""), Some("C"), Some("POSIX"), Some("C.UTF-8")] {
            assert_eq!(language_header(nothing), "en-US,en;q=0.9", "{nothing:?}");
        }
    }

    /// Whatever the machine says, the header has to be one a browser could
    /// have sent.
    #[test]
    fn the_real_system_language_is_well_formed() {
        let value = accept_language();
        assert!(!value.is_empty());
        assert!(
            value.starts_with(|c: char| c.is_ascii_alphabetic()),
            "{value}"
        );
        assert!(!value.contains('_'), "{value}");
        assert!(!value.contains(' ') || value.contains(";q="), "{value}");
    }

    /// Chromium started sending `Priority` on fetch at 123. Claiming to be an
    /// older one and sending it anyway is the same mismatch as inventing
    /// client hints for Firefox.
    #[test]
    fn priority_is_only_sent_by_the_versions_that_send_it() {
        assert_eq!(
            ClientHints::from_user_agent(&desktop(151)).priority,
            Some(FETCH_PRIORITY)
        );
        assert_eq!(
            ClientHints::from_user_agent(&desktop(123)).priority,
            Some(FETCH_PRIORITY)
        );
        assert_eq!(ClientHints::from_user_agent(&desktop(122)).priority, None);

        let firefox =
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:142.0) Gecko/20100101 Firefox/142.0";
        assert_eq!(ClientHints::from_user_agent(firefox).priority, None);
    }

    /// Chromium began offering `zstd` at 123, the release that also started
    /// sending `Priority`. The header was a literal, so a session created with a
    /// Chrome 120 User-Agent -- an old managed install, or one typed at
    /// `--user-agent` -- correctly left `Priority` off and then offered a codec
    /// that browser cannot decode. One request contradicting itself on an axis
    /// the far end can check for free is what this module exists to prevent.
    #[test]
    fn the_accept_encoding_follows_the_version() {
        assert_eq!(
            ClientHints::from_user_agent(&desktop(151)).accept_encoding,
            ACCEPT_ENCODING
        );
        assert_eq!(
            ClientHints::from_user_agent(&desktop(123)).accept_encoding,
            ACCEPT_ENCODING
        );

        let old = ClientHints::from_user_agent(&desktop(122));
        assert_eq!(old.accept_encoding, ACCEPT_ENCODING_BEFORE_ZSTD);
        assert_eq!(
            old.priority, None,
            "the two move together, and it is the pair that has to agree"
        );

        // Firefox has offered the same four since 126, so a browser this module
        // knows nothing else about still gets the current string rather than an
        // invented one.
        let firefox =
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:142.0) Gecko/20100101 Firefox/142.0";
        assert_eq!(
            ClientHints::from_user_agent(firefox).accept_encoding,
            ACCEPT_ENCODING
        );
    }

    #[test]
    fn a_full_desktop_hint_set_is_coherent() {
        let f = ClientHints::from_user_agent(&desktop(141));
        assert_eq!(
            f.ua_brands.as_deref(),
            Some(r#""Google Chrome";v="141", "Not?A_Brand";v="8", "Chromium";v="141""#)
        );
        assert_eq!(f.platform, "\"Windows\"");
        assert_eq!(f.mobile, "?0");
    }
}
