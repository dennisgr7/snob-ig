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

impl User {
    pub fn profile_url(&self) -> String {
        format!("https://www.instagram.com/{}/", self.username)
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
fn printable(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            c if c.is_whitespace() => Some(' '),
            c if c.is_control() || is_reordering(c) => None,
            c => Some(c),
        })
        .collect()
}

/// The invisible formatting characters that reorder what follows them.
///
/// `char::is_control` does not cover these — they are marks, not controls — and
/// a right-to-left override inside a name can make one account read as another
/// entirely.
fn is_reordering(c: char) -> bool {
    matches!(c, '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
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
