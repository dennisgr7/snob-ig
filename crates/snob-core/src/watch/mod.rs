//! The monitor: what changed since the last time anybody looked.
//!
//! Everything in here is domain. It takes lists and moments and answers what
//! moved between them; it opens no database, makes no request and reads no
//! clock. The orchestration — deciding when to look, going and looking, and
//! telling somebody — is `snob-cli`'s, and the split is the same one the rest
//! of the project already keeps between what a thing *is* and how it is
//! presented.

pub mod diff;
pub mod schedule;
pub mod sign;

pub use diff::{Basis, ListDiff, Rename};
pub use schedule::{Due, Schedule, ScheduleError, Weekday};

use crate::model::ListKind;

/// Everything one run found about one account.
///
/// The two lists and the renames travel together because they are one answer to
/// one question — "what changed?" — and a caller that had to assemble them from
/// three separate calls is a caller that can report two of the three.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    pub followers: ListDiff,
    pub following: ListDiff,
    /// Accounts in either list that now go by a different name.
    pub renamed: Vec<Rename>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.followers.is_empty() && self.following.is_empty() && self.renamed.is_empty()
    }

    /// How many individual changes this is.
    ///
    /// What `watch_runs.changes` stores and what decides whether a webhook is
    /// called at all, so it is counted in one place rather than at each of
    /// them.
    pub fn len(&self) -> usize {
        self.followers.len() + self.following.len() + self.renamed.len()
    }

    pub fn of(&self, kind: ListKind) -> &ListDiff {
        match kind {
            ListKind::Followers => &self.followers,
            ListKind::Following => &self.following,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Pk;
    use crate::model::User;

    fn user(pk: Pk) -> User {
        User {
            pk,
            username: format!("u{pk}"),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    #[test]
    fn a_run_that_found_nothing_is_empty() {
        assert!(Changes::default().is_empty());
        assert_eq!(Changes::default().len(), 0);
    }

    /// A rename on its own is a change worth sending. It used to be possible to
    /// read the counting as "changes to the lists", which would have made a run
    /// whose only news was a rename look like a quiet one and never send it.
    #[test]
    fn a_rename_alone_still_counts_as_a_change() {
        let changes = Changes {
            renamed: vec![Rename {
                pk: 7,
                from: "before".into(),
                to: "after".into(),
                at: 1_000,
            }],
            ..Default::default()
        };
        assert!(!changes.is_empty());
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn each_list_is_reachable_by_its_kind() {
        let changes = Changes {
            followers: ListDiff {
                gained: vec![user(1)],
                lost: vec![],
            },
            following: ListDiff {
                gained: vec![],
                lost: vec![user(2)],
            },
            renamed: vec![],
        };

        assert_eq!(changes.of(ListKind::Followers).gained.len(), 1);
        assert_eq!(changes.of(ListKind::Following).lost.len(), 1);
        assert_eq!(changes.len(), 2);
    }
}
