//! The temporal diff: what changed in a list between two captures.
//!
//! `gained` and `lost` are this module's words and belong to nothing else. The
//! set commands answer a question about one instant — `unfollowers` is everyone
//! you follow who does not follow you back, right now — and that answer is a
//! *difference between two lists*. This one is a difference between two
//! *moments* of the same list. Letting either vocabulary drift into the other
//! turns "three people left" into "three people never followed you back", which
//! is a different sentence about different people.
//!
//! Everything here is pure: lists in, changes out. No clock, no database, no
//! network. The rules about *which* two captures may be compared live in
//! [`Basis`], which is the part with the judgment in it and therefore the part
//! that gets tested on its own.

use crate::model::User;
use crate::sets;
use crate::{Epoch, Pk};

/// What changed in one list between two captures.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListDiff {
    /// In the later capture and not the earlier one.
    pub gained: Vec<User>,
    /// In the earlier capture and not the later one.
    pub lost: Vec<User>,
}

impl ListDiff {
    /// Crosses two captures of the same list.
    ///
    /// Both directions come from [`sets`], so both are compared **by `pk`** —
    /// which is what stops somebody who renamed themselves between the two
    /// captures showing up as a departure and an arrival at once. That is not a
    /// hypothetical: renames are one of the things this monitor reports, so the
    /// two features would have contradicted each other on the same account.
    pub fn between(before: &[User], after: &[User]) -> Self {
        Self {
            gained: sets::difference(after, before),
            lost: sets::difference(before, after),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.gained.is_empty() && self.lost.is_empty()
    }

    /// How many changes this is, for the count a caller reports without
    /// having to add the two up itself.
    pub fn len(&self) -> usize {
        self.gained.len() + self.lost.len()
    }
}

/// An account that now goes by a different name.
///
/// The event no tool of this kind offers, and it costs nothing: `users::upsert`
/// has been filing these in `username_history` on every walk since the first
/// release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    pub pk: Pk,
    /// The `username_history` row this came from.
    ///
    /// Carried so a report can say which renames it announced, rather than
    /// leaving a watermark to stand in for the answer. Whether a rename is
    /// reported and whether the scan window may close are independent
    /// conditions — `007_renames_sent` sets out how they come apart in both
    /// directions — and only the row itself can settle the first.
    pub history_id: i64,
    /// What they used to be called.
    pub from: String,
    /// What they are called now.
    pub to: String,
    /// When the change was noticed, not when it happened — we only find out on
    /// the walk that sees the new name.
    pub at: Epoch,
}

/// Which two captures a comparison may be made from, if any.
///
/// Separate from the comparison itself, and pure, because this is where all the
/// rules are. Deciding it takes a mark and an id; making the diff takes two
/// full member lists off the disk. Keeping them apart means the rules can be
/// tested exhaustively without a database, and means the members are only read
/// when they are going to be used — an unchanged list is the common case, and
/// it reads nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Something is stored but nothing has ever been reported for this list.
    ///
    /// **This is everybody's first run**, not a corner case, and it must report
    /// nothing at all: a first tick that announced fourteen hundred "new
    /// followers" would be both useless and, pointed at a webhook, unkind to
    /// whatever is on the other end. The mark is set and the run stays quiet.
    /// There is no [`ListDiff`] to be had from this variant, which is the point
    /// of it being a variant.
    Baseline { snapshot_id: i64 },
    /// The mark already points at the newest usable capture.
    ///
    /// Nothing has been walked since the last report — either the counter had
    /// not moved or the walk was served from storage. Distinct from a `Compare`
    /// that happens to find no changes: this one did not have to look.
    Unchanged { snapshot_id: i64 },
    /// Two different captures, in this order.
    Compare { before: i64, after: i64 },
}

impl Basis {
    /// Works out what can be compared.
    ///
    /// `latest` must come from `usable_snapshots`, which structurally cannot
    /// hand back an incomplete capture — the same guard that already keeps a
    /// half-walked list out of the static crossings. An incomplete list here
    /// would be worse than there: the accounts missing from it would be
    /// reported as departures, and somebody would be told that two hundred
    /// people unfollowed them because a walk was interrupted.
    ///
    /// `None` means there is nothing stored at all, so there is nothing to say.
    pub fn decide(mark: Option<i64>, latest: Option<i64>) -> Option<Self> {
        let latest = latest?;
        match mark {
            None => Some(Self::Baseline {
                snapshot_id: latest,
            }),
            // Compared by id and not by date. Two captures can share a
            // timestamp, and a mark is a receipt for one particular row.
            //
            // **`>=`, not `==`, and the difference is a report that says the
            // opposite of the truth.** Snapshot ids only go up, so a `latest`
            // below the mark means this run's own capture is older than one
            // already reported — which happens without anything going wrong:
            // there is no run lock, `snob watch once` consults no schedule, and
            // a walk that outlives the timer interval is the ordinary way two
            // runs overlap. The slower run then finishes second holding the
            // older id, and `Compare { before: 101, after: 100 }` reports
            // everybody who arrived as `lost` and everybody who left as
            // `gained`, straight into the webhook fields a receiver branches
            // on. The test below says it in as many words and nothing enforced
            // it.
            //
            // The answer is `Unchanged` **at the mark, not at `latest`**: this
            // run has nothing to add, and moving the mark back to its own older
            // capture would make the next run re-report a window that has
            // already been reported. `store::watch` accepts duplicates as the
            // cost of overlap; it does not accept inversions, and it should not
            // have to accept a regression either.
            Some(mark) if mark >= latest => Some(Self::Unchanged { snapshot_id: mark }),
            Some(mark) => Some(Self::Compare {
                before: mark,
                after: latest,
            }),
        }
    }

    /// The capture the mark should point at once this run is over, whichever
    /// way it went.
    ///
    /// One place answers it so that the three variants cannot drift into three
    /// slightly different answers at the call site — and getting this wrong in
    /// the `Baseline` arm is how a first run stays a first run forever.
    pub fn mark_to(self) -> i64 {
        match self {
            Self::Baseline { snapshot_id } | Self::Unchanged { snapshot_id } => snapshot_id,
            Self::Compare { after, .. } => after,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain numbers in, ids out: what these tests are about is which account
    /// is in which list, not how one is spelled.
    fn users(pks: &[u64]) -> Vec<User> {
        pks.iter()
            .map(|&pk| User {
                pk: Pk::new(pk),
                username: format!("u{pk}"),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            })
            .collect()
    }

    fn pks(us: &[User]) -> Vec<u64> {
        us.iter().map(|u| u.pk.get()).collect()
    }

    /// **A run that finishes holding an older capture reports nothing.**
    ///
    /// Two runs overlapping is ordinary: no run lock, `once` consults no
    /// schedule, and a walk longer than the timer interval is how people are
    /// told to drive this from cron. Before the `>=` bound, the slower one
    /// built `Compare { before: 101, after: 100 }` and every arrival came out
    /// as a departure.
    #[test]
    fn a_capture_older_than_the_mark_is_not_compared_backwards() {
        let basis = Basis::decide(Some(101), Some(100)).expect("something is stored");
        assert_eq!(
            basis,
            Basis::Unchanged { snapshot_id: 101 },
            "an older capture has nothing to add, and must not move the mark back"
        );
        assert_eq!(basis.mark_to(), 101, "the mark does not regress");

        // The forward case is untouched.
        assert_eq!(
            Basis::decide(Some(100), Some(101)),
            Some(Basis::Compare {
                before: 100,
                after: 101
            })
        );
    }

    #[test]
    fn an_arrival_is_gained_and_a_departure_is_lost() {
        let before = users(&[1, 2, 3]);
        let after = users(&[2, 3, 4]);
        let diff = ListDiff::between(&before, &after);

        assert_eq!(pks(&diff.gained), vec![4]);
        assert_eq!(pks(&diff.lost), vec![1]);
        assert_eq!(diff.len(), 2);
    }

    #[test]
    fn two_identical_captures_leave_nothing_to_report() {
        let list = users(&[1, 2, 3]);
        assert!(ListDiff::between(&list, &list).is_empty());
    }

    /// The defect the whole module is arranged to avoid. Renames are something
    /// this monitor reports in their own right, so a rename read as a departure
    /// **and** an arrival would have the same run contradict itself about one
    /// person.
    #[test]
    fn somebody_who_renamed_themselves_neither_arrived_nor_left() {
        let before = users(&[7]);
        let mut after = users(&[7]);
        after[0].username = "renamed_themselves".into();

        assert!(ListDiff::between(&before, &after).is_empty());
    }

    /// Nothing walked yet, nothing to say — and in particular no baseline to
    /// set, because setting one against a capture that does not exist would
    /// mark a run that never happened.
    #[test]
    fn nothing_stored_is_nothing_to_compare() {
        assert_eq!(Basis::decide(None, None), None);
        assert_eq!(Basis::decide(Some(1), None), None);
    }

    /// Everybody's first run. It has to be silent, and it has to leave a mark
    /// so the second one is not also a first run.
    #[test]
    fn a_first_run_is_a_baseline_and_still_moves_the_mark() {
        let basis = Basis::decide(None, Some(9)).unwrap();
        assert_eq!(basis, Basis::Baseline { snapshot_id: 9 });
        assert_eq!(basis.mark_to(), 9);
    }

    /// Told apart from a comparison that found nothing: this one never had to
    /// read a single member row.
    #[test]
    fn a_mark_on_the_newest_capture_is_unchanged() {
        assert_eq!(
            Basis::decide(Some(9), Some(9)),
            Some(Basis::Unchanged { snapshot_id: 9 })
        );
    }

    /// The order matters: `before` is the receipt, `after` is what was just
    /// walked. Swapped, every arrival is reported as a departure.
    #[test]
    fn a_newer_capture_is_compared_against_the_mark_in_that_order() {
        let basis = Basis::decide(Some(4), Some(9)).unwrap();
        assert_eq!(
            basis,
            Basis::Compare {
                before: 4,
                after: 9
            }
        );
        assert_eq!(basis.mark_to(), 9);
    }
}
