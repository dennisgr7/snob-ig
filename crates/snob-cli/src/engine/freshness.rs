//! Whether a stored list still answers the question, and the one request that
//! finds out.
//!
//! This is where nearly all of the tool's savings come from. Walking a list of
//! three hundred costs fourteen requests; asking whether it changed costs one.
//! So the counter is always polled, including before the very first walk —
//! without it the first snapshot would be born with no counter to compare
//! against and the cache could never hit at all.

use anyhow::Result;
use snob_core::model::{ListKind, User};
use snob_store::store::{accounts, now, snapshots};

use crate::app::App;
use crate::engine::target::{Counters, Target};
use crate::engine::{ListOutcome, ListQuery, Provenance, walk};

/// Polls, compares, and either serves what is stored or walks.
pub async fn decide_and_fetch(
    app: &mut App,
    args: &ListQuery,
    kind: ListKind,
    target: &Target,
    stored: Option<snapshots::Snapshot>,
) -> Result<(Vec<User>, ListOutcome)> {
    let declared = match poll(app, target, kind).await {
        Ok(counter) => counter,
        Err(e) => {
            // Walking the whole list right when Instagram is already having
            // trouble is the worst possible reaction, so anything stored wins.
            //
            // Except under `--refresh`, whose whole help text is "walk the list
            // again". This arm answered it with whatever was stored — a capture
            // from any month, printed with exit 0 and nothing on screen saying
            // which — because the flag was not consulted until two statements
            // below, and this one returns first. Falling through puts the flag
            // exactly where a first-ever run already is: no counter to compare
            // against, so walk. It costs one request against an endpoint that
            // just failed, which is the price of the flag meaning what it says;
            // the pager does not retry a push-back, so it is one and not four.
            if let Some(snapshot) = &stored
                && !args.refresh
            {
                app.warn(&crate::report::poll_failed_serving_stored(&e));
                // Served, but with nothing said about whether it is still
                // true. It is fine to print; it is not fine to cross against
                // another list, and only the provenance can carry that.
                return serve(app, snapshot, Provenance::PollFailed);
            }
            app.warn(&crate::report::poll_failed(&e));
            None
        }
    };

    if !args.refresh
        && let Some(snapshot) = &stored
        // Saturating rather than casting. `duration::parse` refuses anything
        // this could not hold, so nothing reaches here that would wrap — but
        // "it is checked somewhere else" is how the wrap got written in the
        // first place, and the next reader of a `Duration` inherits an answer
        // this way rather than a hole. Same shape as `watch::schedule`.
        && is_still_good(
            snapshot,
            declared,
            i64::try_from(args.max_age.as_secs()).unwrap_or(i64::MAX),
        )
    {
        // The counter was polled just now and had not moved, so this describes
        // the account as it is however old the snapshot is. That is what makes
        // it safe to cross.
        return serve(app, snapshot, Provenance::CounterVerified);
    }

    walk::fetch(app, args, kind, target, declared).await
}

/// The two conditions a stored list has to meet, both of them necessary.
///
/// Age alone is not enough — a list that changed two minutes ago is wrong
/// however fresh — and an unmoved counter alone is not enough either, because
/// the same number can hide one arrival and one departure. Together they are
/// what makes reusing it honest.
fn is_still_good(snapshot: &snapshots::Snapshot, declared: Option<u64>, max_age_secs: i64) -> bool {
    let fresh = now() - snapshot.taken_at.unwrap_or_default() <= max_age_secs;
    // `None` never counts as unchanged: not knowing the counter is not the
    // same as knowing it stayed put.
    let unchanged = declared.is_some() && declared == snapshot.declared_count;
    fresh && unchanged
}

fn serve(
    app: &App,
    snapshot: &snapshots::Snapshot,
    provenance: Provenance,
) -> Result<(Vec<User>, ListOutcome)> {
    Ok((
        snapshots::members(app.db().conn(), snapshot.id)?,
        ListOutcome::cached(snapshot, provenance),
    ))
}

/// Reads the counters. The cheapest request there is, and the one that avoids
/// walking a list that has not changed.
///
/// It costs nothing at all when the target was resolved by name: resolving and
/// polling are the same call to the same endpoint, so the answer is already in
/// hand and asking twice would only spend the request that the whole cache
/// policy exists to save.
async fn poll(app: &mut App, target: &Target, kind: ListKind) -> Result<Option<u64>> {
    let counters = match (target.counters, target.username.as_deref()) {
        (Some(counters), _) => counters,
        // The profile endpoint takes a name, so without one there is nothing to
        // ask with. Saying the counter is unknown costs nothing; asking about a
        // numeric id would spend a request on a guaranteed 404 every run.
        (None, None) => return Ok(None),
        (None, Some(username)) => {
            let profile = app.client().web_profile_info(username).await?;
            let counters = Counters {
                followers: profile.follower_count(),
                following: profile.following_count(),
            };
            // One answer carries both counters, so the other list of a crossing
            // does not have to ask again. Without this the memo held the
            // identity and the second list still spent a request on the numbers
            // it already had in hand.
            app.remember_counters(counters);
            counters
        }
    };

    accounts::record_poll(
        app.db().conn(),
        target.pk,
        counters.followers,
        counters.following,
    )?;
    Ok(counters.of(kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::Pk;
    use snob_core::model::ListKind;

    fn snapshot(taken_at: i64, declared: Option<u64>) -> snapshots::Snapshot {
        snapshots::Snapshot {
            id: 1,
            account_pk: Pk::new(1),
            kind: ListKind::Followers,
            started_at: taken_at,
            taken_at: Some(taken_at),
            member_count: 10,
            declared_count: declared,
            next_cursor: None,
        }
    }

    const SIX_HOURS: i64 = 6 * 3600;

    #[test]
    fn fresh_and_unmoved_is_the_only_case_that_is_reused() {
        let recent = snapshot(now(), Some(300));
        assert!(is_still_good(&recent, Some(300), SIX_HOURS));
    }

    #[test]
    fn a_moved_counter_is_walked_however_fresh_the_list_is() {
        let recent = snapshot(now(), Some(300));
        assert!(!is_still_good(&recent, Some(301), SIX_HOURS));
    }

    #[test]
    fn an_old_list_is_walked_however_still_the_counter_is() {
        let old = snapshot(now() - SIX_HOURS - 1, Some(300));
        assert!(!is_still_good(&old, Some(300), SIX_HOURS));
    }

    /// The longest maximum age anyone can write must not mean the shortest.
    ///
    /// `duration::parse` now refuses what will not fit in an `i64`, so this is
    /// the second lock on the same door: a `Duration` built any other way still
    /// saturates instead of wrapping, and a saturated bound reuses everything
    /// rather than walking everything.
    #[test]
    fn an_absurd_maximum_age_reuses_rather_than_walks() {
        let ancient = snapshot(0, Some(300));
        let forever =
            i64::try_from(std::time::Duration::from_secs(u64::MAX).as_secs()).unwrap_or(i64::MAX);

        assert_eq!(forever, i64::MAX);
        assert!(
            is_still_good(&ancient, Some(300), forever),
            "the longest age anyone can write is the one that expires nothing"
        );
    }

    /// Not knowing the counter is not the same as knowing it stayed put. If an
    /// unknown counted as unchanged, a failed poll would freeze the cache.
    #[test]
    fn an_unknown_counter_is_never_taken_as_unchanged() {
        let recent = snapshot(now(), Some(300));
        assert!(!is_still_good(&recent, None, SIX_HOURS));

        let never_counted = snapshot(now(), None);
        assert!(!is_still_good(&never_counted, None, SIX_HOURS));
    }
}
