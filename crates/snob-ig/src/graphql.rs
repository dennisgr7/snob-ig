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
//! `__dyn`, `__csr`, `__hsdp`, `__hblp`, `__sjsp` and their neighbours. Those
//! are Relay's record of what the page had already loaded, they are kilobytes
//! long, and inventing them would be describing a browsing session that did not
//! happen. This project sends what it can say truthfully.

use snob_core::secret::Secret;

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
        fb_dtsg: Secret::new(token_after(html, "DTSGInitData")?),
        lsd: Secret::new(token_after(html, "LSD")?),
    })
}

/// The first `"token":"…"` after `marker`.
///
/// Bounded on purpose: the search for the token starts at the marker and gives
/// up after a few hundred bytes. Without the bound, a page missing
/// `DTSGInitData` would happily return the `lsd` token further down as the
/// `fb_dtsg`, and the request would fail in a way that pointed at the wrong
/// thing.
fn token_after(html: &str, marker: &str) -> Option<String> {
    /// How far past the marker the token may be. In every capture it is within
    /// forty bytes; this is generous without being unbounded.
    const REACH: usize = 512;

    let at = html.find(marker)?;
    let window = &html[at..html.len().min(at + REACH)];
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
    let from = at.saturating_sub(REACH);
    let to = js.len().min(at + quoted.len() + REACH);
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_variables_name_the_account_and_the_screen() {
        let vars = variables(528817151);
        assert!(vars.contains(r#""target_user_id":"528817151""#), "{vars}");
        assert!(vars.contains(r#""container_module":"profile""#), "{vars}");
    }
}
