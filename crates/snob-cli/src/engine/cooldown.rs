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
use crate::engine::{ListOutcome, target};
use crate::exit::{ExitCode, ExitError};
use crate::report::{cooldown_ends_at, stored_on};

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
    let when = cooldown_ends_at(until_ms);
    let refuse =
        |detail: String| -> anyhow::Error { ExitError::new(ExitCode::RateLimited, detail).into() };

    if args.refresh {
        return Err(refuse(format!(
            "the account is in cooldown until {when}; --refresh cannot walk until it lifts"
        )));
    }

    let pk = match args.target.as_deref() {
        None => app.viewer().pk,
        Some(typed) => {
            let username = target::clean(typed);
            match snob_core::store::accounts::find_pk_by_username(app.db().conn(), username)? {
                Some(pk) => pk,
                None => {
                    return Err(refuse(format!(
                        "the account is in cooldown until {when}, and no list of \
                         @{username} is stored to serve in the meantime"
                    )));
                }
            }
        }
    };

    let Some(snapshot) = snapshots::latest_complete(app.db().conn(), pk, kind)? else {
        return Err(refuse(format!(
            "the account is in cooldown until {when}, and no complete snapshot of \
             the {kind} list is stored, so there is nothing to serve"
        )));
    };

    let taken_at = snapshot.taken_at.unwrap_or_default();
    // The list is named because a crossing serves two of them, and two
    // identical warnings in a row read like the same one printed twice.
    app.warn(&format!(
        "the account is in cooldown until {when}; serving the {kind} list stored on {}",
        stored_on(taken_at)
    ));

    Ok((
        snapshots::members(app.db().conn(), snapshot.id)?,
        ListOutcome::cached(pk, taken_at, true),
    ))
}

/// Two lists may only be crossed if they describe roughly the same moment.
///
/// The normal paths guarantee that on their own: a fresh pair is walked back to
/// back, and a cached pair is counter-verified, so its staleness is known to be
/// harmless. A pair served during a cooldown carries no such evidence, and
/// stitching two distant moments together invents arrivals and departures that
/// never happened — the same reasoning behind the resume window, which is why
/// it is also the bound.
pub fn check_same_moment(a: &ListOutcome, b: &ListOutcome) -> Result<()> {
    if !(a.from_cooldown || b.from_cooldown) {
        return Ok(());
    }
    if (a.taken_at - b.taken_at).abs() <= snapshots::RESUME_WINDOW_SECS {
        return Ok(());
    }
    Err(ExitError::new(
        ExitCode::RateLimited,
        format!(
            "the two stored lists are from different moments ({} and {}), so crossing \
             them would invent results.\n\
             Run it again once the cooldown lifts.",
            stored_on(a.taken_at),
            stored_on(b.taken_at)
        ),
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(from_cooldown: bool, taken_at: i64) -> ListOutcome {
        ListOutcome::cached(1, taken_at, from_cooldown)
    }

    #[test]
    fn crossing_two_moments_is_refused_only_in_cooldown() {
        // Outside a cooldown the pair is counter-verified; any skew passes.
        assert!(check_same_moment(&outcome(false, 0), &outcome(false, 999_999)).is_ok());

        // A cooldown pair within the window still describes one moment.
        assert!(check_same_moment(&outcome(true, 1_000), &outcome(true, 1_800)).is_ok());

        // Beyond it, the crossing would invent results, so it is refused with
        // the throttling code.
        let error = check_same_moment(&outcome(true, 0), &outcome(true, 10_000)).unwrap_err();
        assert!(error.to_string().contains("different moments"), "{error}");
        assert_eq!(ExitCode::from_chain(&error), Some(ExitCode::RateLimited));

        // One side served in cooldown is enough to lose the evidence.
        assert!(check_same_moment(&outcome(true, 0), &outcome(false, 10_000)).is_err());
    }
}
