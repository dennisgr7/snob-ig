//! Filtering of account lists.

use std::collections::HashSet;

use crate::model::User;

/// URL fragments that identify Instagram's default avatar.
///
/// This is the only way to tell that someone has no picture: the API does not
/// say so, it hands back the URL of the generic image as if it were theirs. The
/// values live here rather than buried in the logic because when Instagram
/// changes its default avatar this stops matching and a new one has to be added.
pub const DEFAULT_AVATAR_HASHES: [&str; 2] = [
    "44884218_345707102882519_2446069589734326272_n",
    "464760996_1254146839119862_3605321457742435801_n",
];

/// Attributes an account can be filtered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Attribute {
    Verified,
    Private,
    NoPfp,
}

impl Attribute {
    /// Whether an account has it.
    ///
    /// Returns `false` when the data is unknown. That is deliberate: hiding
    /// someone over an attribute they may not have is worse than showing them
    /// needlessly, because in the first case the user never sees them at all.
    pub fn matches(self, u: &User) -> bool {
        match self {
            Self::Verified => u.is_verified.unwrap_or(false),
            Self::Private => u.is_private.unwrap_or(false),
            Self::NoPfp => u
                .pfp_url
                .as_deref()
                .is_some_and(|url| DEFAULT_AVATAR_HASHES.iter().any(|d| url.contains(d))),
        }
    }
}

/// What to let through.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Anyone matching any of these is dropped.
    pub hide: Vec<Attribute>,
    /// If non-empty, only accounts matching **all** of these pass.
    pub only: Vec<Attribute>,
    /// Usernames that never show up, lowercased.
    pub excluded: HashSet<String>,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.hide.is_empty() && self.only.is_empty() && self.excluded.is_empty()
    }

    pub fn allows(&self, u: &User) -> bool {
        // The emptiness check is not redundant with `apply`'s: that one fires
        // only when the whole filter is empty, and `scan::summarize` calls this
        // directly, three times per account, over both lists. Without it every
        // one of those allocates a lowercased copy of a name to look it up in a
        // set that has nothing in it.
        if !self.excluded.is_empty() && self.excluded.contains(&u.username.to_lowercase()) {
            return false;
        }
        if self.hide.iter().any(|a| a.matches(u)) {
            return false;
        }
        // Several entries in `only` are ANDed: `--only verified,private` means
        // verified AND private. That is the reading needed to narrow a list
        // down, because OR is what you already get by not filtering at all.
        if !self.only.is_empty() && !self.only.iter().all(|a| a.matches(u)) {
            return false;
        }
        true
    }

    pub fn apply(&self, users: Vec<User>) -> Vec<User> {
        if self.is_empty() {
            return users;
        }
        users.into_iter().filter(|u| self.allows(u)).collect()
    }
}

/// Reads a list of usernames, one per line.
///
/// Tolerates a leading at sign, surrounding space, blank lines and comments,
/// because a hand-written file will have them.
///
/// **And a byte-order mark, which is the one that was not obvious.** `str::trim`
/// trims by the `White_Space` property and U+FEFF is not in it — it is `Cf`,
/// the same category the invisible characters in a name are. So the first key
/// out of a file written by PowerShell 5.1's `Set-Content -Encoding UTF8`, or
/// by Notepad, was `"\u{feff}alice"`, which no username can equal. Every line
/// after it worked, so the file looked right: exit 0, no warning, and the one
/// person it was written to hide in the answer. Stripped here rather than in
/// the reader so the next caller inherits it.
///
/// The `@` comes off **before** the last trim, not after it. Stripped last,
/// `@ alice` produced the key `" alice"`, which no username can equal -- the
/// same silent miss as the byte-order mark above, exit 0 and the name in the
/// answer -- and an entry that is nothing but the sign is dropped rather than
/// kept as an empty key.
pub fn parse_username_list(contents: &str) -> HashSet<String> {
    contents
        .trim_start_matches('\u{feff}')
        .lines()
        .map(|l| l.trim().trim_matches('\u{feff}').trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.trim_start_matches('@').trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str) -> User {
        User {
            pk: 1,
            username: name.into(),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    fn verified() -> User {
        User {
            is_verified: Some(true),
            ..user("celebrity")
        }
    }

    fn private() -> User {
        User {
            is_private: Some(true),
            ..user("reserved")
        }
    }

    fn without_picture() -> User {
        User {
            pfp_url: Some(format!(
                "https://scontent.cdninstagram.com/v/{}.jpg",
                DEFAULT_AVATAR_HASHES[0]
            )),
            ..user("faceless")
        }
    }

    #[test]
    fn an_empty_filter_drops_nobody() {
        let f = Filter::default();
        let list = vec![verified(), private(), user("plain")];
        assert_eq!(f.apply(list).len(), 3);
    }

    #[test]
    fn hide_drops_whoever_matches() {
        let f = Filter {
            hide: vec![Attribute::Verified],
            ..Default::default()
        };
        let left = f.apply(vec![verified(), user("plain")]);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].username, "plain");
    }

    #[test]
    fn only_requires_every_attribute() {
        let f = Filter {
            only: vec![Attribute::Verified, Attribute::Private],
            ..Default::default()
        };
        let both = User {
            is_private: Some(true),
            ..verified()
        };
        assert!(f.allows(&both));
        assert!(!f.allows(&verified()));
        assert!(!f.allows(&private()));
    }

    #[test]
    fn the_default_avatar_is_recognized() {
        assert!(Attribute::NoPfp.matches(&without_picture()));
        assert!(!Attribute::NoPfp.matches(&user("has_a_picture")));

        let own_picture = User {
            pfp_url: Some("https://scontent.cdninstagram.com/v/their_own.jpg".into()),
            ..user("has_a_picture")
        };
        assert!(!Attribute::NoPfp.matches(&own_picture));
    }

    /// Data we do not have cannot make anyone disappear from the list.
    #[test]
    fn unknown_does_not_count_as_matching() {
        let no_data = user("mystery");
        assert!(!Attribute::Verified.matches(&no_data));
        assert!(!Attribute::Private.matches(&no_data));
        assert!(!Attribute::NoPfp.matches(&no_data));

        let f = Filter {
            hide: vec![Attribute::Verified],
            ..Default::default()
        };
        assert!(f.allows(&no_data), "not hidden just in case");
    }

    #[test]
    fn exclusion_ignores_case_and_the_at_sign() {
        let f = Filter {
            excluded: parse_username_list("@Friend\nother"),
            ..Default::default()
        };
        assert!(!f.allows(&user("friend")));
        assert!(!f.allows(&user("FRIEND")));
        assert!(!f.allows(&user("other")));
        assert!(f.allows(&user("stranger")));
    }

    #[test]
    fn the_username_list_tolerates_how_people_write_it() {
        let read = parse_username_list(
            "# people I keep\n\
             @one\n\
             \n\
               two  \n\
             THREE\n\
             @ four\n\
             @\n",
        );
        assert_eq!(read.len(), 4, "{read:?}");
        assert!(read.contains("one"));
        assert!(read.contains("two"));
        assert!(read.contains("three"));
        // A space after the sign is how it is typed when the sign is an
        // afterthought; stripped after the trim, the key was " four".
        assert!(read.contains("four"), "{read:?}");
        assert!(!read.contains(""), "a bare sign is not an account");
    }

    /// The file the tool is handed on Windows most of the time.
    ///
    /// `str::trim` goes by `White_Space`, which does not contain U+FEFF, so the
    /// first key came out as `"\u{feff}alice"` — a string no username equals.
    /// Only the first line was affected, which is what made it look like the
    /// file was being read: the exclusion silently applied to everyone in it
    /// except the person on line one.
    #[test]
    fn a_byte_order_mark_does_not_hide_the_first_name() {
        let read = parse_username_list("\u{feff}alice\nbob\n");

        assert!(read.contains("alice"), "{read:?}");
        assert!(read.contains("bob"), "{read:?}");
        assert_eq!(read.len(), 2);

        let f = Filter {
            excluded: read,
            ..Default::default()
        };
        assert!(
            !f.allows(&user("alice")),
            "the name the file exists to hide"
        );
    }

    #[test]
    fn hide_beats_only() {
        // If something is on both lists it does not show up: hiding is an
        // explicit refusal and should weigh more.
        let f = Filter {
            hide: vec![Attribute::Verified],
            only: vec![Attribute::Verified],
            ..Default::default()
        };
        assert!(!f.allows(&verified()));
    }
}
