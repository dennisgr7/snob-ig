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
        ListOutcome::cached(&snapshot, Provenance::Cooldown),
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
///
/// What is measured is the time **between** the two walks, not between the two
/// moments they finished at. See [`gap_between`].
pub fn check_same_moment(a: &ListOutcome, b: &ListOutcome) -> Result<()> {
    // Both sides have to carry evidence, not just neither side being a
    // cooldown. This used to ask the second question, and two of the three
    // paths that serve from storage answered it "no cooldown here" — so
    // `snob unfollowers --cache` crossed June against August without so much
    // as comparing the dates.
    if a.provenance.describes_now() && b.provenance.describes_now() {
        return Ok(());
    }
    if gap_between(a, b) <= SAME_MOMENT_GAP_SECS {
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

/// How much dead time there may be between two walks and still be one moment.
///
/// Deliberately the same size as [`snapshots::RESUME_WINDOW_SECS`] and
/// deliberately not that constant. Fifteen minutes of an account's life is the
/// drift this tool already treats as a single instant — that is what the resume
/// window decides for one interrupted walk, and this decides the same thing for
/// two finished ones. Two questions, so two numbers: changing how long a walk
/// may be paused must not quietly change what may be crossed.
const SAME_MOMENT_GAP_SECS: i64 = 15 * 60;

/// The seconds during which neither walk was looking.
///
/// Each list covers an interval — first page to last — rather than an instant,
/// and what can invent an arrival is a stretch of time one list saw and the
/// other did not. So this is the distance **between** the intervals: zero when
/// they overlap or touch, however long either of them took.
///
/// That difference is the whole point. Two lists walked back to back in one
/// correct run *finish* far apart by definition — on an account following six
/// thousand people the second walk alone is about twenty minutes at the
/// documented pace — so comparing finishing times against a fifteen-minute
/// bound refused precisely the pair that was most obviously one moment, and
/// went on refusing it every time that pair was read back with `--cache`.
///
/// It is not a licence, either. A walk that genuinely took an hour, crossed
/// against a snapshot from two hours later, still has an hour of gap and is
/// still refused.
///
/// A resumed walk widens its own interval by up to the resume window, and that
/// is not an extra concession: [`snapshots::RESUME_WINDOW_SECS`] has already
/// decided a walk paused that long is one capture. The span treated as one
/// moment here is exactly the span the store was already willing to call one.
fn gap_between(a: &ListOutcome, b: &ListOutcome) -> i64 {
    (b.started_at - a.taken_at)
        .max(a.started_at - b.taken_at)
        .max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::ExitCode;

    /// The drift two stored lists may have between them, written out.
    ///
    /// Every other test refers to it by name, so it could be set to a day and
    /// the suite would still pass — while two walks half a day apart were
    /// crossed as though they described one moment, which is what invents an
    /// arrival that never happened.
    ///
    /// Deliberately **not** asserted equal to `snapshots::RESUME_WINDOW_SECS`,
    /// which it currently matches. The constant's own doc says they are two
    /// questions and two numbers, and tying them together in a test is exactly
    /// the quiet coupling it was split apart to prevent.
    #[test]
    fn the_gap_two_lists_may_have_is_the_documented_one() {
        assert_eq!(SAME_MOMENT_GAP_SECS, 15 * 60);
    }

    /// A stored row, which is what the outcomes under test are built from.
    fn stored(started_at: i64, taken_at: i64) -> snapshots::Snapshot {
        snapshots::Snapshot {
            id: 1,
            account_pk: 1,
            kind: ListKind::Followers,
            started_at,
            taken_at: Some(taken_at),
            complete: true,
            member_count: 0,
            declared_count: None,
            pages: 0,
            requests: 0,
            next_cursor: None,
            resumes: 0,
        }
    }

    /// A capture with no duration, which is what these tests used to be able to
    /// assume and real ones never are.
    fn outcome(provenance: Provenance, taken_at: i64) -> ListOutcome {
        walked(provenance, taken_at, taken_at)
    }

    fn walked(provenance: Provenance, started_at: i64, taken_at: i64) -> ListOutcome {
        ListOutcome::cached(&stored(started_at, taken_at), provenance)
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

    /// The defect this predicate was rewritten for, and the shape of every
    /// correct crossing on an account of any size.
    ///
    /// `taken_at` is when a walk **finished**. Walking six thousand accounts
    /// takes about twenty minutes at the documented pace, so the two lists of
    /// one perfectly good `snob unfollowers` run finish far more than fifteen
    /// minutes apart — and reading that same pair back with `--cache`, where
    /// neither side carries evidence, was refused as "different moments". The
    /// answer was correct and the tool would not show it, ever again.
    #[test]
    fn two_walks_run_back_to_back_are_one_moment_however_long_they_took() {
        let followers = walked(Provenance::CacheFlag, 0, 1_200);
        let following = walked(Provenance::CacheFlag, 1_260, 3_600);

        assert!(
            (following.taken_at - followers.taken_at).abs() > SAME_MOMENT_GAP_SECS,
            "the finishing times are far apart; that is the point"
        );
        assert!(check_same_moment(&followers, &following).is_ok());
    }

    /// Two walks that were running at the same time left no unobserved stretch
    /// at all.
    #[test]
    fn overlapping_walks_have_no_gap_at_all() {
        let a = walked(Provenance::CacheFlag, 0, 2_000);
        let b = walked(Provenance::CacheFlag, 1_000, 3_000);
        assert_eq!(gap_between(&a, &b), 0);
        assert!(check_same_moment(&a, &b).is_ok());
    }

    /// Measuring the gap rather than the distance must not become permission.
    /// A long walk widens its own interval; it does not excuse a partner from
    /// hours later.
    #[test]
    fn a_long_walk_is_not_a_licence_for_a_stale_partner() {
        let hour_long = walked(Provenance::CacheFlag, 0, 3_600);
        let much_later = outcome(Provenance::CacheFlag, 10_000);
        assert!(check_same_moment(&hour_long, &much_later).is_err());
    }

    /// A resumed walk keeps the moment its first page was asked for, so its
    /// interval is wider by however long it was paused. That is not a second
    /// concession: `RESUME_WINDOW_SECS` has already decided a pause of that
    /// length leaves one capture, and this treats exactly that span as one
    /// moment. It still expires.
    #[test]
    fn a_resumed_walk_is_one_interval_from_its_first_page() {
        let resumed = walked(Provenance::CacheFlag, 0, 2_000);

        let just_after = walked(Provenance::CacheFlag, 2_010, 2_100);
        assert!(check_same_moment(&resumed, &just_after).is_ok());

        let much_later = outcome(Provenance::CacheFlag, 20_000);
        assert!(check_same_moment(&resumed, &much_later).is_err());
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
