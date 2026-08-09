//! What the tool can still answer while the account is in cooldown, and the
//! rule that stops two stored lists from being crossed when they should not be.
//!
//! A cooldown does not blind the tool. Nothing may be spent — not even the
//! counter poll — so storage is the only thing that can answer, and it answers
//! however old it is: stale beats nothing, and the warning names the date.

use anyhow::Result;
use snob_core::model::{ListKind, User};
use snob_core::store::snapshots;

use crate::app::App;
use crate::cli::ListArgs;
use crate::engine::{ListOutcome, Provenance, target};
use crate::report::{self, Blocked, cooldown_ends_at, stored_on};

/// Serves what is stored, or explains why nothing can be.
///
/// No confirmation is asked because nothing is enumerated, and `--max-age` is
/// ignored for the same reason `--cache` ignores it.
pub fn serve(
    app: &App,
    args: &ListArgs,
    kind: ListKind,
    until_ms: i64,
) -> Result<(Vec<User>, ListOutcome)> {
    if args.refresh {
        return Err(report::refuse_in_cooldown(until_ms, Blocked::RefreshWanted));
    }

    let pk = match args.target.as_deref() {
        None => app.viewer().pk,
        Some(typed) => {
            let username = target::clean(typed);
            match snob_core::store::accounts::find_pk_by_username(app.db().conn(), username)? {
                Some(pk) => pk,
                None => {
                    return Err(report::refuse_in_cooldown(
                        until_ms,
                        Blocked::AccountUnknown(username),
                    ));
                }
            }
        }
    };

    let Some(snapshot) = snapshots::latest_complete(app.db().conn(), pk, kind)? else {
        return Err(report::refuse_in_cooldown(
            until_ms,
            Blocked::NothingStored(kind),
        ));
    };

    let when = cooldown_ends_at(until_ms);
    let taken_at = snapshot.taken_at.unwrap_or_default();
    // The list is named because a crossing serves two of them, and two
    // identical warnings in a row read like the same one printed twice.
    app.warn(&format!(
        "the account is in cooldown until {when}; serving the {kind} list stored on {}",
        stored_on(taken_at)
    ));

    Ok((
        snapshots::members(app.db().conn(), snapshot.id)?,
        ListOutcome::cached(pk, taken_at, Provenance::Cooldown),
    ))
}

/// Two lists may only be crossed if they describe roughly the same moment.
///
/// Only two of the five provenances guarantee that on their own: a walked list
/// is the account as it is, and a counter-verified one was checked against it in
/// this run, so its age is known to be harmless. The other three are stored
/// lists that nothing looked at — during a cooldown nothing may be spent, on a
/// failed poll nothing could be, and with `--cache` nothing was meant to be.
///
/// Stitching two distant moments together invents arrivals and departures that
/// never happened, which is the failure this whole tool is built not to have.
/// The bound is the resume window, for the same reason it bounds that.
pub fn check_same_moment(a: &ListOutcome, b: &ListOutcome) -> Result<()> {
    // Both sides have to carry evidence, not just neither side being a
    // cooldown. This used to ask the second question, and two of the three
    // paths that serve from storage answered it "no cooldown here" — so
    // `snob unfollowers --cache` crossed June against August without so much
    // as comparing the dates.
    if a.provenance.describes_now() && b.provenance.describes_now() {
        return Ok(());
    }
    if (a.taken_at - b.taken_at).abs() <= snapshots::RESUME_WINDOW_SECS {
        return Ok(());
    }

    // What happened, handed over as it is. Which sentence and which exit code
    // that deserves is `report`'s question, not this module's: AGENTS.md is
    // explicit that engine never decides how anything looks.
    Err(report::refuse_different_moments(
        a.provenance,
        b.provenance,
        a.taken_at,
        b.taken_at,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::ExitCode;

    fn outcome(provenance: Provenance, taken_at: i64) -> ListOutcome {
        ListOutcome::cached(1, taken_at, provenance)
    }

    /// A counter checked in this run is what makes age harmless, so any skew
    /// passes — that is the whole point of the cache.
    #[test]
    fn a_verified_pair_may_be_any_distance_apart() {
        let a = outcome(Provenance::CounterVerified, 0);
        let b = outcome(Provenance::CounterVerified, 999_999);
        assert!(check_same_moment(&a, &b).is_ok());
    }

    /// The regression this type exists for. Two of the three storage paths used
    /// to look identical to a verified one, so `snob unfollowers --cache`
    /// crossed a followers list from June against a following list from August
    /// and called the difference unfollowers.
    #[test]
    fn an_unverified_pair_far_apart_is_refused() {
        let june = 0;
        let august = 5_000_000;
        for provenance in [
            Provenance::CacheFlag,
            Provenance::PollFailed,
            Provenance::Cooldown,
        ] {
            let error = check_same_moment(&outcome(provenance, june), &outcome(provenance, august))
                .expect_err(&format!("{provenance:?} carries no evidence"));
            assert!(error.to_string().contains("different moments"), "{error}");
        }
    }

    /// Close enough together and it is still one moment, whatever the reason
    /// nobody checked.
    #[test]
    fn an_unverified_pair_within_the_window_still_describes_one_moment() {
        let a = outcome(Provenance::CacheFlag, 1_000);
        let b = outcome(Provenance::CacheFlag, 1_800);
        assert!(check_same_moment(&a, &b).is_ok());
    }

    /// One side without evidence is enough to lose it, even against a walk.
    #[test]
    fn one_unverified_side_is_enough_to_refuse() {
        let walked = ListOutcome {
            provenance: Provenance::Walked,
            ..outcome(Provenance::CacheFlag, 0)
        };
        let stale = outcome(Provenance::CacheFlag, 5_000_000);
        assert!(check_same_moment(&walked, &stale).is_err());
    }

    /// The code has to match the cause, because `exit.rs` says these exist so a
    /// service can tell "wait a while" from everything else without reading the
    /// sentence. What the sentence says is `report`'s test: this module hands
    /// over the provenances and stops there.
    #[test]
    fn the_code_matches_why_nobody_checked() {
        let throttled = check_same_moment(
            &outcome(Provenance::Cooldown, 0),
            &outcome(Provenance::Cooldown, 5_000_000),
        )
        .unwrap_err();
        assert_eq!(
            ExitCode::from_chain(&throttled),
            Some(ExitCode::RateLimited)
        );

        let asked_for = check_same_moment(
            &outcome(Provenance::CacheFlag, 0),
            &outcome(Provenance::CacheFlag, 5_000_000),
        )
        .unwrap_err();
        assert_eq!(ExitCode::from_chain(&asked_for), Some(ExitCode::Error));
    }
}
