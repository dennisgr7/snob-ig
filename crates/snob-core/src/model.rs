//! Domain types.
//!
//! Deliberately separate from the models in `snob-ig`, which are the wire
//! format and change whenever Instagram changes. What lives here is what gets
//! stored and what goes out as JSON, which is a contract with whoever consumes
//! the tool. Were they the same type, an Instagram change would break that
//! contract without anyone noticing.

use serde::{Deserialize, Serialize};

use crate::Pk;

/// An Instagram account as we store it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub pk: Pk,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_private: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pfp_url: Option<String>,
}

/// What may sit in a path segment unescaped: RFC 3986's unreserved set, which
/// contains every character Instagram lets a username be made of.
const IN_A_PATH: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

impl User {
    /// The address of this account, with the name encoded into it.
    ///
    /// **Encoded rather than filtered, and the reason is not the terminal.**
    /// This string lands in the URL slot of an OSC 8 sequence, in a markdown
    /// link destination and in a spreadsheet hyperlink, and [`printable`] would
    /// keep all three safe — but it *removes* characters, and a name with one
    /// removed is the address of a **different account**, which may well exist.
    /// A link that quietly points at somebody else is worse than one that does
    /// not work, because nothing on screen says anything happened.
    ///
    /// Percent-encoding cannot do that. It is the identity on the alphabet
    /// Instagram allows, so ordinary names come out byte for byte as they did
    /// before, and anything else round-trips instead of resolving elsewhere.
    /// It also closes the hole this had: an `ESC` in a username ended the OSC 8
    /// sequence early and let the rest of the name open one of its own, so the
    /// cell showed a filtered name and pointed wherever the name said.
    pub fn profile_url(&self) -> String {
        format!(
            "https://www.instagram.com/{}/",
            percent_encoding::utf8_percent_encode(&self.username, IN_A_PATH)
        )
    }

    /// The full name with control characters taken out, for anything a
    /// terminal will interpret.
    pub fn safe_full_name(&self) -> Option<String> {
        self.full_name.as_deref().map(printable)
    }

    /// The username, likewise.
    pub fn safe_username(&self) -> String {
        printable(&self.username)
    }
}

/// Strips what a terminal would obey rather than show.
///
/// Every string here came from another person's profile, and an account name is
/// the cheapest way into this program: anyone who follows you gets one. Left
/// alone, `\x1b]8;;http://…\x1b\\` in a full name turns a row into a link
/// somewhere else, `\x1b[2K\x1b[A` erases the line above, and on terminals with
/// OSC 52 enabled the clipboard can be written. None of that needs Instagram to
/// be compromised.
///
/// It removes rather than escapes: what is wanted is the name, and a name has
/// no control characters in it. Everything printable survives untouched,
/// accents and other scripts included.
///
/// The one exception is whitespace that happens to be a control character —
/// a newline or a tab. Those become a space rather than vanishing, because
/// deleting them would join two words that were never one.
///
/// Public because the same filter is needed for strings that never become a
/// [`User`]: the name a `scan` summary opens with, an account named in an
/// error, an excerpt of a response body. A second, slightly different copy of
/// this list is how one output path ends up covered and another does not.
pub fn printable(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            c if c.is_whitespace() => Some(' '),
            c if c.is_control() || is_invisible(c) => None,
            c => Some(c),
        })
        .collect()
}

/// The characters that show nothing but change how their neighbours read.
///
/// `char::is_control` covers general category `Cc`, which is both the C0 and
/// the C1 ranges. It does **not** cover these: they are category `Cf`, formatting
/// marks rather than controls, and a right-to-left override inside a name can
/// make one account read as another entirely.
///
/// Three groups, and the reason each is here:
///
/// - **Bidirectional overrides** — U+061C, the marks and the embedding,
///   override and isolate controls. This is the "Trojan Source" class, and
///   reversing the visible order of a name is the whole attack.
/// - **Zero-width characters** — ZWSP, ZWNJ, ZWJ, the word joiner, the
///   invisible operators and the byte-order mark. They let two different
///   accounts render as the same name, which is what makes a list of who to
///   unfollow untrustworthy.
/// - **The tag block**, U+E0000–U+E007F, which encodes arbitrary invisible
///   ASCII inside a name and renders as nothing at all.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{061c}'
        | '\u{200b}'..='\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}'
        | '\u{feff}'
        | '\u{e0000}'..='\u{e007f}')
}

/// Which side of the follow relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListKind {
    Followers,
    Following,
}

impl ListKind {
    /// How it is stored and how it appears in the API path.
    ///
    /// **Frozen**: this string feeds `snapshots.kind`, whose CHECK constraint
    /// only accepts these two values.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Followers => "followers",
            Self::Following => "following",
        }
    }
}

/// The other direction, for reading a stored row back.
///
/// The standard trait rather than an inherent `from_str`, which clippy warns
/// about for a good reason: a caller reaching for `"followers".parse()` would
/// otherwise get a different function than the one they expected, or none.
///
/// It fails rather than defaulting. A `kind` column holding something else
/// means the database was written by something that is not this program, and
/// quietly calling that "followers" would answer a question about a list nobody
/// asked about.
impl std::str::FromStr for ListKind {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "followers" => Ok(Self::Followers),
            "following" => Ok(Self::Following),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for ListKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a walk ended. Determines whether the snapshot can be compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The whole list was walked.
    Completed,
    /// The user interrupted it.
    Canceled,
    /// A caller-requested page cap was hit.
    PageLimit,
    /// Instagram stopped serving pages early.
    Truncated,
    /// Instagram is throttling requests.
    RateLimit,
    /// A network failure that did not recover.
    Network,
    /// The session stopped working.
    SessionInvalid,
}

impl StopReason {
    /// Whether the resulting snapshot can be used as the basis of a comparison.
    ///
    /// Only a full walk describes the whole list. With any other ending
    /// accounts are missing, and using it as a basis would make every one of
    /// them look like it left: the classic failure of tools like this one.
    ///
    /// A caller-requested cap is not an exception. The list is still short.
    pub fn yields_complete_list(self) -> bool {
        matches!(self, Self::Completed)
    }

    /// How it is stored in `snapshots.stopped_by`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Canceled => "canceled",
            Self::PageLimit => "page_limit",
            Self::Truncated => "truncated",
            Self::RateLimit => "rate_limit",
            Self::Network => "network",
            Self::SessionInvalid => "session_invalid",
        }
    }

    /// Every variant, so schema and code cannot drift apart untested.
    pub const ALL: [Self; 7] = [
        Self::Completed,
        Self::Canceled,
        Self::PageLimit,
        Self::Truncated,
        Self::RateLimit,
        Self::Network,
        Self::SessionInvalid,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(username: &str) -> User {
        User {
            pk: 1,
            username: username.to_string(),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    /// Encoding has to be the identity on the alphabet Instagram allows, or
    /// every link in every export changes for no reason.
    #[test]
    fn an_ordinary_name_comes_out_of_the_link_unchanged() {
        assert_eq!(
            named("some.user_1-x").profile_url(),
            "https://www.instagram.com/some.user_1-x/"
        );
    }

    /// The hole this closes. The address goes into the URL slot of an OSC 8
    /// sequence and into a markdown link destination, and an `ESC` there ended
    /// the sequence early: the cell showed a filtered name and pointed wherever
    /// the unfiltered one said.
    #[test]
    fn a_name_cannot_break_out_of_the_link_it_is_put_in() {
        let url = named("a\x1b\\\x1b]8;;http://evil.test\x1b\\Official").profile_url();

        assert!(!url.contains('\x1b'), "an escape survived: {url}");
        assert!(!url.contains(']'), "a sequence introducer survived: {url}");
        assert!(
            !url.contains("evil.test/"),
            "the second address is still a path of its own: {url}"
        );
    }

    /// Why this encodes instead of filtering. `printable` would remove the
    /// slashed o, and `/sren/` is somebody else's account — one that may well
    /// exist. A link quietly pointing at the wrong person is worse than a
    /// broken one, because nothing says it happened.
    ///
    /// The letter is Danish rather than Spanish so that the language guard has
    /// nothing to say about it. Which letter it is does not matter here; that
    /// it is not ASCII does.
    #[test]
    fn an_accent_still_points_at_the_same_account() {
        assert_eq!(
            named("søren").profile_url(),
            "https://www.instagram.com/s%C3%B8ren/"
        );
    }

    #[test]
    fn only_a_full_walk_yields_a_complete_list() {
        assert!(StopReason::Completed.yields_complete_list());
        for partial in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Truncated,
            StopReason::RateLimit,
            StopReason::Network,
            StopReason::SessionInvalid,
        ] {
            assert!(
                !partial.yields_complete_list(),
                "{partial:?} does not describe the whole list"
            );
        }
    }

    /// The three groups the filter names, each with the character that makes
    /// the case: a bidirectional override, a zero-width space, and a tag
    /// character. None of them show anything, and all of them change what the
    /// name next to them means.
    #[test]
    fn invisible_characters_do_not_survive() {
        for hidden in [
            '\u{061c}',  // Arabic letter mark
            '\u{200b}',  // zero-width space
            '\u{200d}',  // zero-width joiner
            '\u{202e}',  // right-to-left override
            '\u{2060}',  // word joiner
            '\u{2068}',  // first strong isolate
            '\u{feff}',  // byte-order mark
            '\u{e0041}', // tag latin capital A
        ] {
            let name = format!("real{hidden}name");
            assert_eq!(
                printable(&name),
                "realname",
                "U+{:04X} survived",
                hidden as u32
            );
        }
    }

    /// Two accounts must not be able to render as the same name. This is the
    /// point of taking the zero-width characters out, rather than only the
    /// ones that drive the terminal.
    #[test]
    fn a_zero_width_character_cannot_forge_a_name() {
        let impostor = "insta\u{200b}gram";
        assert_ne!(impostor, "instagram");
        assert_eq!(printable(impostor), "instagram");
    }

    /// Taking the invisible characters out must not take the visible ones with
    /// them. Accents, other scripts and emoji are ordinary content.
    #[test]
    fn ordinary_text_is_untouched() {
        for name in ["Jos\u{e9} Mu\u{f1}oz", "\u{4e2d}\u{6587}", "ana \u{1f600}"] {
            assert_eq!(printable(name), name);
        }
    }

    #[test]
    fn a_user_serializes_without_its_empty_fields() {
        let u = User {
            pk: 42,
            username: "someone".into(),
            full_name: None,
            is_private: None,
            is_verified: Some(true),
            pfp_url: None,
        };
        let json = serde_json::to_string(&u).unwrap();
        assert_eq!(json, r#"{"pk":42,"username":"someone","is_verified":true}"#);
    }

    /// Frozen: this string feeds `snapshots.kind`, and its CHECK constraint
    /// accepts nothing else.
    #[test]
    fn the_list_kind_wire_value_is_lowercase_english() {
        assert_eq!(ListKind::Followers.as_str(), "followers");
        assert_eq!(ListKind::Following.as_str(), "following");
        assert_eq!(
            serde_json::to_string(&ListKind::Followers).unwrap(),
            r#""followers""#
        );
    }

    #[test]
    fn every_stop_reason_has_a_distinct_stored_value() {
        let mut seen = std::collections::HashSet::new();
        for reason in StopReason::ALL {
            assert!(seen.insert(reason.as_str()), "{reason:?} is duplicated");
        }
        assert_eq!(seen.len(), 7);
    }
}
