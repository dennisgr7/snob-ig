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
        if self.excluded.contains(&u.username.to_lowercase()) {
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
pub fn parse_username_list(contents: &str) -> HashSet<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.trim_start_matches('@').to_lowercase())
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
             THREE\n",
        );
        assert_eq!(read.len(), 3);
        assert!(read.contains("one"));
        assert!(read.contains("two"));
        assert!(read.contains("three"));
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
