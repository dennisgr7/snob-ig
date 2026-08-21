//! The one part of Instagram that this tool reaches through GraphQL, and the
//! machinery for not being broken by it.
//!
//! # Why this exists at all
//!
//! Everything else here goes to `/api/v1/`, and that is deliberate: those
//! routes are the durable ones. Instagram deprecated GraphQL `doc_id`s in bulk
//! in June 2026 and broke `instaloader` twice doing it, which is exactly the
//! failure mode this module is built to survive.
//!
//! Following and unfollowing are the exception, and not by choice. A capture of
//! the real web client in August 2026 settled it: the Follow button sends
//! `POST /api/graphql` with `fb_api_req_friendly_name: usePolarisFollowMutation`,
//! and **the word `friendship` does not appear anywhere in the browser's
//! traffic**. The REST spellings every write-up gives are the mobile app's and
//! are not served here — `/api/v1/friendships/create/{pk}/` answers 200 with
//! the web app's HTML shell, and `/web/friendships/{pk}/follow/` answers 404.
//! Both were sent live and the relationship read back after each; neither
//! changed anything.
//!
//! # What a request needs, and where each piece comes from
//!
//! | Piece | Where |
//! |---|---|
//! | `fb_dtsg` | the HTML of a logged-in page |
//! | `lsd` | the same HTML |
//! | `jazoest` | **computed** from `fb_dtsg`; see [`jazoest`] |
//! | `doc_id` | discovered from the page's own JavaScript; see [`doc_id_in`] |
//! | `variables` | the account id, and the two strings saying which screen |
//!
//! The `doc_id` being **discovered rather than written down** is the whole
//! point. A constant would work today and stop working on the day Instagram
//! rebuilds, with no warning and no way for a user to fix it. Reading it out of
//! the page that would have used it means the rotation fixes itself.
//!
//! What is deliberately **not** sent is the telemetry the browser attaches —
//! `__dyn`, `__csr`, `__hsdp`, `__hblp`, `__sjsp` and their neighbors. Those
//! are Relay's record of what the page had already loaded, they are kilobytes
//! long, and inventing them would be describing a browsing session that did not
//! happen. This project sends what it can say truthfully.

use snob_core::secret::Secret;

/// What a run already knows about the mutation identifiers.
///
/// **Discovery has to be cached or it is unaffordable.** Finding a `doc_id`
/// means fetching JavaScript bundles until one contains the mutation, and those
/// are megabytes. Once per rotation is fine; once per follow is not.
///
/// A trait rather than a field, because the thing that remembers is the
/// database and this crate does not open one. `snob-cli` implements it over the
/// `meta` table; the tests implement it over a `HashMap`.
///
/// **Deliberately not `Send + Sync`.** The obvious bound would not compile
/// against the one implementation that matters: `rusqlite::Connection` is
/// neither, by design, because a SQLite connection belongs to one thread. It is
/// not needed either — the cache is consulted on the task that is already
/// making the request, and never moved.
pub trait DocIds {
    fn get(&self, friendly_name: &str) -> Option<String>;
    fn put(&self, friendly_name: &str, doc_id: &str);
}

/// A cache that forgets immediately.
///
/// For a caller with nothing to remember with — and for the tests, where
/// discovery running every time is the point rather than a cost.
pub struct NoDocIds;

impl DocIds for NoDocIds {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
    fn put(&self, _: &str, _: &str) {}
}

/// The two tokens a page hands out, which together authorize a mutation.
///
/// Both are per-session and short-lived, and both are `Secret` for the same
/// reason the session cookie is: they are credentials, they must not be
/// printed, and they should not be left in freed memory.
#[derive(Debug)]
pub struct PageTokens {
    pub fb_dtsg: Secret,
    pub lsd: Secret,
}

/// Which of the two mutations is being sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    Follow,
    Unfollow,
}

impl Mutation {
    /// Every write this program can make.
    ///
    /// Named as a set so that "there are two, and there is no third" is a value
    /// a test can read rather than a sentence in a document. The same shape
    /// `ExitCode::ALL` and `secrets::Kind::ALL` use, and for the same reason.
    ///
    /// Adding a variant is already a compile error in the two matches below,
    /// which have no wildcard arm on purpose. This is what stops the quieter
    /// version: pointing an existing variant at a different operation.
    pub const ALL: [Self; 2] = [Self::Follow, Self::Unfollow];

    /// Where the write goes.
    ///
    /// **On the mutation rather than at the call site, and that is the point.**
    /// `IgClient::post` used to take a path, which meant the promise that only
    /// two writes exist rested on nobody having typed a third string. A guard
    /// that reads the source for spellings cannot hold that line -- the
    /// identifier Instagram acts on is a `doc_id`, a number, and no list of
    /// English words contains it. Taking a `Mutation` instead means a third
    /// write needs a third variant, and a third variant does not compile until
    /// somebody has written it into all three matches here.
    pub fn path(self) -> &'static str {
        match self {
            // Both go to the same place today. Kept per-variant anyway, because
            // the browser also uses `/graphql/query` for some operations and a
            // future variant may need it -- and because a single shared
            // constant would put the path back at the call site by the back
            // door.
            Self::Follow | Self::Unfollow => "/api/graphql",
        }
    }

    /// The name the request announces itself with, and the name to look for
    /// when discovering the `doc_id`. One constant for both, because a
    /// disagreement between them would send a request whose id belongs to a
    /// different operation.
    pub fn friendly_name(self) -> &'static str {
        match self {
            Self::Follow => "usePolarisFollowMutation",
            Self::Unfollow => "usePolarisUnfollowMutation",
        }
    }

    /// The identifier as it was on the day this was captured.
    ///
    /// # Why there is a written-down value here at all
    ///
    /// The intention was to discover it every time and never write one down,
    /// because a constant stops working the day Instagram rebuilds. **That
    /// turned out not to be buildable, and the attempt is worth recording so
    /// nobody spends the afternoon again.**
    ///
    /// A profile page names around four hundred and forty JavaScript bundles.
    /// The mutation is in none of them: it lives in a chunk the client loads on
    /// demand once the profile route has mounted, and the loader builds that
    /// URL from a manifest that is not resolvable by reading the page. Measured
    /// in August 2026 against the real logged-in page — sixty bundles opened,
    /// about seven megabytes, no match; and against the logged-out one, the
    /// same. What the page *does* carry is the two tokens, which is why those
    /// are still read rather than stored.
    ///
    /// So the value below is a seed, and it is treated the way `ASBD_ID` is:
    /// captured on a date, from a real request, with the way to refresh it
    /// written next to it. The recovery path is not a rebuild — it is
    /// [`crate::client::IgClient::doc_id_for`] trying the discovery walk when
    /// the seed is refused, and failing that, an error that tells the user
    /// where to look. Both are worth more than a silent stop.
    ///
    /// Captured 21 August 2026, Chrome 151, from `POST /api/graphql`.
    pub fn seed_doc_id(self) -> &'static str {
        match self {
            Self::Follow => "26508036048874888",
            Self::Unfollow => "27789106940691111",
        }
    }
}

/// Instagram's checksum over `fb_dtsg`.
///
/// **Not a secret and not fetched: it is derived.** The literal `2`, then the
/// sum of the code points of the token. Facebook's own client computes it
/// exactly this way, and it is sent alongside the token it summarizes, so it
/// adds no security — but it is part of the shape, and a request without it is
/// a shape the site does not produce.
///
/// Bytes rather than characters. The token is ASCII in every capture, so the
/// two agree today; bytes are what the reference implementation sums, and
/// choosing characters would differ the moment a non-ASCII one appeared.
pub fn jazoest(fb_dtsg: &str) -> String {
    let sum: u32 = fb_dtsg.bytes().map(u32::from).sum();
    format!("2{sum}")
}

/// Pulls `fb_dtsg` and `lsd` out of a rendered page.
///
/// They arrive inside the JSON blob the page bootstraps itself from, as
/// `["DTSGInitData",[],{"token":"..."}]` and `["LSD",[],{"token":"..."}]`.
/// Read by locating the marker and taking the next `"token":"..."`, rather than
/// with a JSON parse: the blob is megabytes of Relay state and parsing all of
/// it to reach two strings would be the expensive way to be no more correct.
///
/// `None` when either is missing, which is what a logged-out page looks like —
/// so the caller reports a dead session rather than sending a mutation that
/// cannot be authorized.
pub fn extract_tokens(html: &str) -> Option<PageTokens> {
    Some(PageTokens {
        // **The opening bracket and the comma are part of the marker, not
        // decoration.** `LSD` on its own is three characters, and the page has
        // `LSDatabaseSingletonLazyWrapper` in it five times, in the module-name
        // lists further down. Today the real entry comes first and the bare
        // substring finds it -- by ordering, which is not a property anybody
        // maintains. If that order ever flipped the answer would not be
        // `None`: it would be whatever `"token":"` sat within five hundred
        // bytes of the decoy, and a mutation sent with a good-looking wrong
        // token is refused with a 400 that `worth_rediscovering` reads as a
        // stale identifier -- so the recovery walk runs, seven megabytes off
        // the CDN, and then spends a second write slot on the same wrong token.
        fb_dtsg: Secret::new(token_after(html, r#"["DTSGInitData","#)?),
        lsd: Secret::new(token_after(html, r#"["LSD","#)?),
    })
}

/// The first `"token":"…"` after `marker`.
///
/// Bounded on purpose: the search for the token starts at the marker and gives
/// up after a few hundred bytes. Without the bound, a page missing
/// `DTSGInitData` would happily return the `lsd` token further down as the
/// `fb_dtsg`, and the request would fail in a way that pointed at the wrong
/// thing.
/// The largest index at or below `at` that a `&str` can be cut on.
///
/// **Both windows below are byte offsets applied to text that is not ASCII**,
/// and slicing a `str` between the bytes of one character is a panic, not an
/// error. The page is six hundred kilobytes of profile bootstrap full of names
/// and biographies; the bundle is `from_utf8_lossy` of minified JavaScript,
/// where every replacement character is three bytes wide, so a window edge
/// landing mid-character there is ordinary rather than unlucky. Every fixture
/// in this file was ASCII, so the suite could not see it, and `doc_id_in` --
/// which is the recovery path -- would have panicked on the day it was needed.
///
/// `str::floor_char_boundary` would say this in one call and is not stable on
/// the version this crate builds against. `error::body_excerpt` solves the same
/// problem by counting characters instead; here the offsets have to stay bytes,
/// because they are measured against `find`.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn token_after(html: &str, marker: &str) -> Option<String> {
    /// How far past the marker the token may be. In every capture it is within
    /// forty bytes; this is generous without being unbounded.
    const REACH: usize = 512;

    let at = html.find(marker)?;
    let window = &html[at..floor_boundary(html, at + REACH)];
    let key = "\"token\":\"";
    let start = window.find(key)? + key.len();
    let end = window[start..].find('"')? + start;
    let token = &window[start..end];
    (!token.is_empty()).then(|| token.to_string())
}

/// Finds the `doc_id` of a named mutation in a compiled Relay artifact.
///
/// Relay writes the operation's parameters as an object carrying both the name
/// and the id, so the two sit within a few dozen bytes of each other:
/// `name:"usePolarisFollowMutation"` beside `id:"26508036048874888"`. Which
/// comes first is a property of the minifier rather than of the format, so both
/// orders are looked for.
///
/// The id is all digits and long. That is checked rather than assumed: a match
/// on some other `id:` in the neighbourhood would send a request naming an
/// operation Instagram does not have, and the answer to that is an error nobody
/// could act on.
///
/// **The id taken is the one nearest the name, not the first one in the
/// window**, and that distinction is the whole safety of this function. The two
/// mutations sit next to each other in the same chunk, so a window centred on
/// `usePolarisUnfollowMutation` reaches back over the follow operation's id. A
/// first-match search returned it — which is to say, it followed when asked to
/// unfollow. The test below is the one that found that.
pub fn doc_id_in(js: &str, friendly_name: &str) -> Option<String> {
    /// How far either side of the name an id may sit.
    const REACH: usize = 400;
    /// Instagram's ids are seventeen digits today. Ten is a floor that no
    /// unrelated numeric field in the neighbourhood reaches.
    const SHORTEST: usize = 10;

    let quoted = format!("\"{friendly_name}\"");
    let at = js.find(&quoted)?;
    let from = floor_boundary(js, at.saturating_sub(REACH));
    let to = floor_boundary(js, at + quoted.len() + REACH);
    let window = &js[from..to];
    // Where the name sits inside the window. Both ends, because "nearest" is
    // the **gap** between the two spans and not the distance between their
    // starting offsets: an id just before the name starts further away than one
    // just after it, while sitting closer. Measured start-to-start, the follow
    // operation's id won the search for the unfollow one.
    let name_at = at - from;
    let name_ends = name_at + quoted.len();

    let mut best: Option<(usize, String)> = None;
    // `id:"…"` and `"id":"…"`, which is the same field before and after
    // minification.
    for key in ["id:\"", "\"id\":\""] {
        let mut offset = 0;
        while let Some(found) = window[offset..].find(key) {
            let start = offset + found + key.len();
            let Some(len) = window[start..].find('"') else {
                break;
            };
            let candidate = &window[start..start + len];
            if candidate.len() >= SHORTEST && candidate.bytes().all(|b| b.is_ascii_digit()) {
                let ends = start + len;
                let gap = if start >= name_ends {
                    start - name_ends
                } else {
                    name_at.saturating_sub(ends)
                };
                if best.as_ref().is_none_or(|(near, _)| gap < *near) {
                    best = Some((gap, candidate.to_string()));
                }
            }
            offset = start;
        }
    }
    best.map(|(_, id)| id)
}

/// The `variables` object, as the site sends it.
///
/// `container_module` and `nav_chain` say which screen the button was on. They
/// are sent because the site sends them and their absence is the anomaly;
/// `nav_chain` is a constant rather than assembled per request, because
/// assembling one would be inventing a path through the site that nobody walked.
pub fn variables(target_pk: snob_core::Pk) -> String {
    format!(
        r#"{{"target_user_id":"{target_pk}","container_module":"profile","nav_chain":"{NAV_CHAIN}"}}"#
    )
}

const NAV_CHAIN: &str = "PolarisProfilePostsTabRoot:profilePage:1:via_cold_start";

/// Every JavaScript bundle a page pulls in, in the order it names them.
///
/// The `doc_id` lives in one of these and there is no way to know which without
/// looking, so the caller walks them. **The order matters and it is the page's,
/// not ours**: a page lists the chunk that its own route needs, and that is the
/// one holding the profile page's mutations, so walking in order finds it
/// early rather than after megabytes of unrelated code.
///
/// Only the script host is accepted. A page's HTML is full of URLs, some of
/// them from places this tool has no business fetching from, and the one thing
/// that makes walking them safe is that the host is fixed here rather than
/// taken from the document.
/// **Most of them arrive with their slashes escaped**, and missing that was
/// worth six URLs out of four hundred and thirty-seven. A page names a handful
/// of bundles in `<script src>` attributes, where the URL is written plainly,
/// and every other one inside a JSON string in the bootstrap payload, where it
/// is `https:\/\/static.cdninstagram.com\/rsrc.php\/…`. Reading only the plain
/// ones looks like it works — the list is not empty — and finds one and a half
/// per cent of what is there.
pub fn bundles_in(html: &str) -> Vec<String> {
    const HOST: &str = "static.cdninstagram.com";

    let mut found: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find(HOST) {
        // Back up over the scheme, which is `https://` or `https:\/\/`.
        let line_start = rest[..at].rfind("https:").filter(|s| at - s <= 10);
        let from = &rest[line_start.unwrap_or(at)..];
        // A URL ends at the first character that cannot be in one. A backslash
        // can, here, because it is the escape in front of a slash — so it is
        // not a terminator and is unescaped below instead.
        let end = from
            .find(|c: char| c == '"' || c == '\'' || c == '<' || c == ')' || c.is_whitespace())
            .unwrap_or(from.len());
        // **The whole prefix, unescaped, and anchored at the start.** Matching
        // the host as a substring is what let `static.cdninstagram.com.evil.example`
        // through when the escaped form was added; the test below is the one
        // that caught it. The trailing slash is what makes the host the host
        // rather than a prefix of a longer one.
        const PREFIX: &str = "https://static.cdninstagram.com/";
        let url = from[..end].replace("\\/", "/");
        if url.starts_with(PREFIX) && url.ends_with(".js") && !found.contains(&url) {
            found.push(url);
        }
        rest = &rest[at + HOST.len()..];
    }
    found
}

/// The form body of a mutation.
///
/// Ordered the way the browser orders it, which costs nothing and means a
/// capture of ours and a capture of the site's line up when somebody compares
/// them.
///
/// `__user=0` is not a mistake: the real request sends exactly that even when
/// logged in, which is worth a note because it looks like a bug every time
/// somebody reads it.
pub fn mutation_body(
    tokens: &PageTokens,
    mutation: Mutation,
    doc_id: &str,
    target_pk: snob_core::Pk,
) -> Vec<(String, String)> {
    let fb_dtsg = tokens.fb_dtsg.expose().to_string();
    let jazoest = jazoest(&fb_dtsg);
    vec![
        ("__d".into(), "www".into()),
        ("__user".into(), "0".into()),
        ("__a".into(), "1".into()),
        ("__comet_req".into(), "7".into()),
        ("fb_dtsg".into(), fb_dtsg),
        ("jazoest".into(), jazoest),
        ("lsd".into(), tokens.lsd.expose().to_string()),
        ("fb_api_caller_class".into(), "RelayModern".into()),
        (
            "fb_api_req_friendly_name".into(),
            mutation.friendly_name().into(),
        ),
        ("server_timestamps".into(), "true".into()),
        ("variables".into(), variables(target_pk)),
        ("doc_id".into(), doc_id.into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **There are two writes, and there is no third.**
    ///
    /// The rule this pins is the one in `AGENTS.md`, and it is pinned in two
    /// directions because there are two ways to break it. Adding a variant is
    /// already a compile error -- `path`, `friendly_name` and `seed_doc_id`
    /// have no wildcard arm between them. What this catches is the quieter
    /// one: pointing a variant that already exists at a different operation,
    /// which changes what the program does to somebody else's account without
    /// changing a single type.
    ///
    /// The names are written out rather than derived, so that changing one is
    /// a decision somebody has to make here, in front of this comment.
    #[test]
    fn the_only_two_writes_are_the_two_that_are_named() {
        assert_eq!(Mutation::ALL.len(), 2);
        assert_eq!(
            Mutation::ALL.map(Mutation::friendly_name),
            ["usePolarisFollowMutation", "usePolarisUnfollowMutation"],
        );
        for mutation in Mutation::ALL {
            assert_eq!(mutation.path(), "/api/graphql");
            // Nothing that registers a view. The web client's operation for
            // that is named in `crates/snob-core/tests/no_seen.rs`, which is
            // the one file allowed to write it down; this checks the value that
            // would actually go on the wire, which that guard cannot see.
            let name = mutation.friendly_name();
            assert!(
                !name.contains("Seen"),
                "a write that marks something seen: {name}"
            );
        }
    }

    /// The literal `2` and the sum of the bytes. Checked against a hand-worked
    /// value rather than against the implementation restated.
    #[test]
    fn jazoest_is_two_and_the_sum_of_the_bytes() {
        // 'a' is 97, three of them is 291.
        assert_eq!(jazoest("aaa"), "2291");
        assert_eq!(jazoest(""), "20");
    }

    /// The shape the page really has, trimmed to the two markers.
    const PAGE: &str = r#"...{"define":[["DTSGInitData",[],{"token":"NAfyABC:123:456","async_get_token":"x"},258],
        ["LSD",[],{"token":"G8i4s1ETVk"},323]]}..."#;

    #[test]
    fn both_tokens_come_out_of_a_rendered_page() {
        let tokens = extract_tokens(PAGE).expect("a logged-in page has both");
        assert_eq!(tokens.fb_dtsg.expose(), "NAfyABC:123:456");
        assert_eq!(tokens.lsd.expose(), "G8i4s1ETVk");
    }

    /// A logged-out page has no `fb_dtsg`, and that has to be `None` rather
    /// than the next token further down the file — which is what an unbounded
    /// search would return, and would send the `lsd` where the `fb_dtsg` goes.
    #[test]
    fn a_page_without_the_marker_does_not_borrow_the_next_token() {
        let logged_out = r#"["LSD",[],{"token":"onlythisone"}]"#;
        assert!(extract_tokens(logged_out).is_none());
    }

    /// **A decoy before the real entry must not win.**
    ///
    /// The test above covers the marker being *absent*. This covers it being
    /// present twice, which is the case the real page is one reordering away
    /// from: `LSDatabaseSingletonLazyWrapper` appears five times in a profile
    /// page, and a bare three-character `LSD` search would have taken whatever
    /// token sat near the first one it met.
    #[test]
    fn a_name_that_merely_starts_with_the_marker_is_not_the_marker() {
        let page = concat!(
            r#"["LSDatabaseSingletonLazyWrapper",[],{"token":"WRONG"},7],"#,
            r#"["DTSGInitData",[],{"token":"dtsg"},1],"#,
            r#"["LSD",[],{"token":"right"},2]"#
        );
        let tokens = extract_tokens(page).expect("both tokens are there");
        assert_eq!(tokens.lsd.expose(), "right");
        assert_eq!(tokens.fb_dtsg.expose(), "dtsg");
    }

    /// **A page with an accent in it must not end the process.**
    ///
    /// The window is a byte count and the page is UTF-8, so a character lying
    /// across the far edge was a panic rather than a miss. Swept across the
    /// edge rather than placed on it, because the arithmetic that decides
    /// which byte the edge is on is the arithmetic under test.
    #[test]
    fn a_multi_byte_character_on_the_window_edge_is_not_a_panic() {
        let marker = "[\"DTSGInitData\",";
        // The marker is sixteen bytes and the reach is five hundred and
        // twelve, so the edge falls on the character when the filler is around
        // four hundred and ninety-five. Swept, not pinned, so the test does not
        // depend on my arithmetic being right.
        for filler in 490..500 {
            let page = format!(
                "{marker}{}\u{e9}[],{{\"token\":\"t\"}}][\"LSD\",[],{{\"token\":\"l\"}}]",
                "x".repeat(filler)
            );
            // The answer may be `None` -- the token is past the reach at these
            // lengths. What it may not be is an abort.
            let _ = extract_tokens(&page);
        }

        // And with the character in the way but the token still inside, the
        // token still comes out.
        let near = format!("{marker}\u{e9}[],{{\"token\":\"t\"}}][\"LSD\",[],{{\"token\":\"l\"}}]");
        let tokens = extract_tokens(&near).expect("both tokens are within reach");
        assert_eq!(tokens.fb_dtsg.expose(), "t");
        assert_eq!(tokens.lsd.expose(), "l");
    }

    /// The same hazard on the recovery path, where it matters more: the text is
    /// `from_utf8_lossy` of a minified bundle, so it is dense with three-byte
    /// replacement characters, and this is the code that only runs on the day a
    /// `doc_id` has already rotated.
    #[test]
    fn a_lossy_bundle_does_not_panic_the_discovery_walk() {
        let name = "\"usePolarisFollowMutation\"";
        let tail = ",id:\"26508036048874888\"";

        // Below the window: a replacement character where `at - 400` falls.
        for pad in 394..404 {
            let js = format!("\u{fffd}{}{name}{tail}", "y".repeat(pad));
            assert_eq!(
                doc_id_in(&js, "usePolarisFollowMutation").as_deref(),
                Some("26508036048874888"),
                "lower edge, pad {pad}"
            );
        }

        // Above it: one where `at + len + 400` falls.
        for pad in 370..382 {
            let js = format!("{name}{tail}{}\u{fffd}", "z".repeat(pad));
            assert_eq!(
                doc_id_in(&js, "usePolarisFollowMutation").as_deref(),
                Some("26508036048874888"),
                "upper edge, pad {pad}"
            );
        }
    }

    /// Minified Relay, in both the orders the minifier produces.
    #[test]
    fn the_doc_id_is_found_either_side_of_the_name() {
        let after = r#"params:{id:"26508036048874888",metadata:{},name:"usePolarisFollowMutation",operationKind:"mutation"}"#;
        let before = r#"params:{name:"usePolarisFollowMutation",operationKind:"mutation",id:"26508036048874888"}"#;
        for js in [after, before] {
            assert_eq!(
                doc_id_in(js, "usePolarisFollowMutation").as_deref(),
                Some("26508036048874888"),
                "{js}"
            );
        }
    }

    /// The two mutations must not be confused for each other. They sit in the
    /// same file, so a search that found the nearest id rather than the right
    /// one would follow when asked to unfollow.
    #[test]
    fn each_mutation_gets_its_own_id() {
        let js = concat!(
            r#"{name:"usePolarisFollowMutation",id:"26508036048874888"},"#,
            r#"{name:"usePolarisUnfollowMutation",id:"27789106940691111"}"#
        );
        assert_eq!(
            doc_id_in(js, "usePolarisUnfollowMutation").as_deref(),
            Some("27789106940691111")
        );
        assert_eq!(
            doc_id_in(js, "usePolarisFollowMutation").as_deref(),
            Some("26508036048874888")
        );
    }

    /// A chunk that does not contain the mutation says so, rather than handing
    /// back somebody else's id.
    #[test]
    fn a_chunk_without_the_mutation_finds_nothing() {
        let js = r#"params:{id:"11111111111111111",name:"useSomethingElseMutation"}"#;
        assert!(doc_id_in(js, "usePolarisFollowMutation").is_none());
    }

    /// A short number near the name is not an id. Without the length floor,
    /// `version:2` or an array index next to the name would be sent as one.
    #[test]
    fn a_short_number_is_not_mistaken_for_an_id() {
        let js = r#"{id:"7",name:"usePolarisFollowMutation"}"#;
        assert!(doc_id_in(js, "usePolarisFollowMutation").is_none());
    }

    #[test]
    fn the_bundles_come_out_in_the_order_the_page_names_them() {
        let html = concat!(
            r#"<script src="https://static.cdninstagram.com/rsrc.php/v4/yD/r/first.js"></script>"#,
            r#"<link href="https://static.cdninstagram.com/rsrc.php/v4/yE/r/second.js">"#
        );
        let found = bundles_in(html);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].ends_with("first.js"));
        assert!(found[1].ends_with("second.js"));
    }

    /// **The escaped form is most of them.** A page writes a handful plainly in
    /// `<script src>` and every other one inside a JSON string, where the
    /// slashes are escaped. Reading only the plain ones looks like it works and
    /// finds one and a half per cent of what is there — measured on a real
    /// profile page: six out of four hundred and thirty-seven.
    #[test]
    fn a_bundle_written_inside_a_json_string_is_found_too() {
        let html = r#"{"src":"https:\/\/static.cdninstagram.com\/rsrc.php\/v4\/lazy.js","c":1}"#;
        let found = bundles_in(html);
        assert_eq!(
            found,
            vec!["https://static.cdninstagram.com/rsrc.php/v4/lazy.js"],
            "the escaped form has to be unescaped, not skipped"
        );
    }

    /// A page names the same bundle in more than one place. Fetching it twice
    /// is two requests for one answer.
    #[test]
    fn a_bundle_named_twice_is_fetched_once() {
        let once = "https://static.cdninstagram.com/rsrc.php/v4/a.js";
        let html = format!(r#"<script src="{once}"></script><script src="{once}"></script>"#);
        assert_eq!(bundles_in(&html).len(), 1);
    }

    /// **Only Instagram's script host.** A page is full of URLs and some of
    /// them point at places this tool has no business fetching from; the host
    /// is fixed in the code rather than read out of the document, which is what
    /// makes walking the list safe.
    #[test]
    fn a_script_somewhere_else_is_not_walked() {
        let html = r#"<script src="https://evil.example/rsrc.php/x.js"></script>
                      <script src="https://static.cdninstagram.com.evil.example/a.js"></script>"#;
        assert!(bundles_in(html).is_empty());
    }

    /// Things that are not JavaScript are not JavaScript.
    #[test]
    fn only_scripts_are_walked() {
        let html = r#"<img src="https://static.cdninstagram.com/rsrc.php/v4/logo.png">"#;
        assert!(bundles_in(html).is_empty());
    }

    #[test]
    fn the_body_carries_what_the_site_carries() {
        let tokens = PageTokens {
            fb_dtsg: Secret::new("TOKEN".to_string()),
            lsd: Secret::new("LSD".to_string()),
        };
        let body = mutation_body(&tokens, Mutation::Unfollow, "27789106940691111", 7);
        let field = |name: &str| {
            body.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            field("fb_api_req_friendly_name"),
            "usePolarisUnfollowMutation"
        );
        assert_eq!(field("doc_id"), "27789106940691111");
        assert_eq!(field("fb_dtsg"), "TOKEN");
        // Derived, not fetched, and derived from the token beside it.
        assert_eq!(field("jazoest"), jazoest("TOKEN"));
        assert!(field("variables").contains(r#""target_user_id":"7""#));
    }

    /// **The telemetry the browser attaches is not sent.** It is Relay's record
    /// of what the page had already loaded; inventing it would describe a
    /// browsing session that did not happen, and it is kilobytes long.
    #[test]
    fn no_relay_telemetry_is_invented() {
        let tokens = PageTokens {
            fb_dtsg: Secret::new("TOKEN".to_string()),
            lsd: Secret::new("LSD".to_string()),
        };
        let body = mutation_body(&tokens, Mutation::Follow, "1", 7);
        for invented in ["__dyn", "__csr", "__hsdp", "__hblp", "__sjsp", "__spin_t"] {
            assert!(
                !body.iter().any(|(k, _)| k == invented),
                "{invented} was made up"
            );
        }
    }

    #[test]
    fn the_variables_name_the_account_and_the_screen() {
        let vars = variables(528817151);
        assert!(vars.contains(r#""target_user_id":"528817151""#), "{vars}");
        assert!(vars.contains(r#""container_module":"profile""#), "{vars}");
    }
}
