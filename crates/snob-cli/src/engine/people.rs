//! Who you both know.
//!
//! When the account being looked at is not yours, the useful first line is not
//! a number — it is a name you recognize. Instagram itself leads with it
//! ("Followed by so-and-so and 4 others"), and the tool can work it out with no
//! request at all: the answer is the accounts you follow that also follow them.
//!
//! Strictly from storage, and strictly from a **complete** snapshot. A partial
//! list of your own following would leave people out of the overlap, and naming
//! two mutual acquaintances when there are nine is worse than naming none.

use anyhow::Result;
use snob_core::Epoch;
use snob_core::model::{ListKind, User};
use snob_core::sets;
use snob_store::store::snapshots;

use crate::app::App;

/// The overlap, and when the list it was worked out from was captured.
///
/// The date is not decoration. This is the only stored answer `snob scan`
/// produces that used to arrive without one, while every other figure in the
/// same object dates itself — so the opening line could name accounts you
/// unfollowed months ago and read exactly like one worked out this minute.
/// `check_same_moment` does not cover it: that rule is about the two walked
/// lists, and no flag refreshes this one.
#[derive(Debug)]
pub struct InCommon {
    pub people: Vec<User>,
    /// When the capture of your own following finished.
    pub taken_at: Epoch,
}

impl InCommon {
    /// Whether this is recent enough to stand beside lists walked this minute.
    ///
    /// The same `--max-age` that decides whether a stored list may be reused,
    /// applied to the one stored answer no flag refreshes: `--refresh` walks
    /// the two lists of the account being scanned, not your own following, and
    /// `check_same_moment` compares only those two.
    pub fn is_current(&self, max_age_secs: i64, now: Epoch) -> bool {
        now - self.taken_at <= max_age_secs
    }
}

/// The accounts you follow that are among `followers`.
///
/// `None` means the question could not be answered — no stored list of your own
/// following — which reads differently from `Some(people: vec![])`, "nobody you
/// follow is in there". The caller has to keep them apart: one is silence, the
/// other is an answer.
pub fn in_common(app: &App, followers: &[User]) -> Result<Option<InCommon>> {
    let viewer = app.viewer();
    let Some(snapshot) =
        snapshots::latest_complete(app.db().conn(), viewer.pk, ListKind::Following)?
    else {
        return Ok(None);
    };

    let mine = snapshots::members(app.db().conn(), snapshot.id)?;
    Ok(Some(InCommon {
        // Ordered by the list of people you follow rather than by their
        // followers: the names are there to be recognized, and that is the list
        // you know.
        people: sets::intersection(&mine, followers),
        taken_at: snapshot.taken_at.unwrap_or(snapshot.started_at),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The moment arrives as plain seconds and becomes an [`Epoch`] here, at
    /// the edge, so the assertions below read as an age against `SIX_HOURS`.
    fn found(taken_at: i64) -> InCommon {
        InCommon {
            people: Vec::new(),
            taken_at: Epoch::new(taken_at),
        }
    }

    const SIX_HOURS: i64 = 6 * 3600;

    /// The opening line of `snob scan someone` is the one figure in the answer
    /// that no flag refreshes, so without this it could name accounts
    /// unfollowed months ago and read exactly like one worked out this minute.
    #[test]
    fn a_stored_overlap_expires_like_every_other_stored_answer() {
        let now = 1_000_000;

        assert!(found(now).is_current(SIX_HOURS, Epoch::new(now)));
        assert!(
            found(now - SIX_HOURS).is_current(SIX_HOURS, Epoch::new(now)),
            "the boundary is inclusive, like is_still_good's"
        );
        assert!(!found(now - SIX_HOURS - 1).is_current(SIX_HOURS, Epoch::new(now)));
    }
}
