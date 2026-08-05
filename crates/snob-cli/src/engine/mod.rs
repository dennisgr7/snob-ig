//! Getting a list, and everything that decides how.
//!
//! This is the part of the tool that has rules rather than opinions: who is
//! being asked about, whether the network has to be touched at all, what a
//! walk costs, and when a stored answer is still true. It returns data and a
//! description of where the data came from; it never decides how any of it
//! looks. That is [`crate::commands`]' job.
//!
//! Every list, crossing and summary the tool prints comes out of [`list`].

pub mod cooldown;
pub mod freshness;
pub mod people;
pub mod target;
pub mod walk;

use anyhow::{Result, bail};
use snob_core::Pk;
use snob_core::model::{ListKind, StopReason, User};
use snob_core::store::{accounts, users};

use crate::app::App;
use crate::cli::ListArgs;
use crate::ui;

/// Where a returned list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultSource {
    /// A fresh walk against Instagram.
    Fetched,
    /// A stored snapshot that still holds.
    Cached,
}

#[derive(Debug)]
pub struct ListOutcome {
    pub source: ResultSource,
    pub reason: StopReason,
    pub requests: u32,
    pub taken_at: i64,
    /// Whether the list was served by the cooldown path, which asks no
    /// confirmation and carries no freshness evidence.
    pub from_cooldown: bool,
    /// Whose list this is.
    ///
    /// The caller asked with a name and gets back an id, which is the only
    /// answer that cannot be wrong: a name has to be resolved, may not be the
    /// spelling Instagram uses, and — for your own account — may not be known
    /// at all until something goes and looks it up.
    pub account_pk: Pk,
}

impl ListOutcome {
    /// What a stored snapshot answers with. Complete by construction: the
    /// store only ever hands back snapshots that are.
    pub(crate) fn cached(account_pk: Pk, taken_at: i64, from_cooldown: bool) -> Self {
        Self {
            source: ResultSource::Cached,
            reason: StopReason::Completed,
            // Filled in by `list`, which is the only place that sees the whole
            // run and can ask the pacer what it really charged.
            requests: 0,
            taken_at,
            from_cooldown,
            account_pk,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.reason.yields_complete_list()
    }

    /// Whether this is the list of the account the run acts as.
    pub fn is_own(&self, viewer: &crate::app::Viewer) -> bool {
        self.account_pk == viewer.pk
    }
}

/// Gets one list, deciding along the way whether anything needs fetching.
///
/// The order of the checks is the whole policy, and each one exists to stop a
/// request being spent that did not have to be:
///
/// 1. In cooldown nothing may be spent, so only storage can answer.
/// 2. With `--cache` the network is off, resolution included.
/// 3. Someone else's account needs consent before it is enumerated.
/// 4. The cooldown is checked again, because it can land while step 3 waits.
/// 5. One counter poll says whether the list moved at all.
/// 6. If it did not, and what is stored is fresh enough, storage answers.
/// 7. Otherwise, walk.
pub async fn list(
    app: &mut App,
    args: &ListArgs,
    kind: ListKind,
) -> Result<(Vec<User>, ListOutcome)> {
    // Measured rather than added up along the way. Every request goes through
    // the pacer, including the ones a retry makes and the ones spent before
    // the walk begins, so asking it afterwards is the only count that cannot
    // drift from what was really charged.
    let before = app.client().pacer().spent();
    let (users, mut outcome) = decide(app, args, kind).await?;
    outcome.requests = app.client().pacer().spent().saturating_sub(before);
    Ok((users, outcome))
}

async fn decide(
    app: &mut App,
    args: &ListArgs,
    kind: ListKind,
) -> Result<(Vec<User>, ListOutcome)> {
    if let Some(until_ms) = app.client().pacer().cooldown()? {
        return cooldown::serve(app, args, kind, until_ms);
    }

    ask_consent(app, args)?;

    let target = if args.cache {
        target::from_store(app, args.target.as_deref(), kind)?
    } else {
        target::resolve(app, args).await?
    };

    // The user row has to exist before the account row: `accounts.pk`
    // references `users.pk`.
    users::upsert(app.db().conn(), &User::from(&target))?;
    accounts::upsert(app.db().conn(), target.pk, target.is_self)?;

    let stored = snob_core::store::snapshots::latest_complete(app.db().conn(), target.pk, kind)?;

    if args.cache {
        let Some(snapshot) = stored else {
            bail!("no snapshot of the {kind} list is stored; drop --cache to fetch it");
        };
        return Ok((
            snob_core::store::snapshots::members(app.db().conn(), snapshot.id)?,
            ListOutcome::cached(target.pk, snapshot.taken_at.unwrap_or_default(), false),
        ));
    }

    // The check at the top cannot see a cooldown that lands while the
    // resolution or the confirmation prompt were underway — one set by the
    // service sharing this database, for instance. Nothing may be spent once
    // it exists, so look again before the poll.
    if let Some(until_ms) = app.client().pacer().cooldown()? {
        return cooldown::serve(app, args, kind, until_ms);
    }

    freshness::decide_and_fetch(app, args, kind, &target, stored).await
}

/// Asks before enumerating somebody else's account, at most once per run.
///
/// It happens **before** the account is resolved, because resolving is already
/// a request: asking afterwards meant that answering "no" — or running down a
/// pipe, where the answer defaults to no — had still spent one on a run the
/// user never authorized.
///
/// The price of asking first is that the name shown is the one typed rather
/// than the one Instagram spells. That costs nothing when it is wrong, and the
/// only account it can wrongly ask about is your own, which needs you to have
/// typed your own name.
fn ask_consent(app: &mut App, args: &ListArgs) -> Result<()> {
    let Some(typed) = args.target.as_deref() else {
        return Ok(()); // your own account, nothing to agree to
    };
    if args.yes || app.has_consent() {
        return Ok(());
    }

    let name = target::clean(typed);
    let is_own_name = app
        .viewer()
        .username
        .as_deref()
        .is_some_and(|mine| mine.eq_ignore_ascii_case(name));
    if is_own_name {
        return Ok(());
    }

    app.warn(
        "enumerating someone else's followers is the pattern Instagram's detection \
         systems watch most closely",
    );
    if !ui::confirm(&format!("Continue with @{name}?"), false)? {
        bail!("canceled; use -y to skip the confirmation");
    }
    // Asked and answered. A crossing wants two lists and a summary four, and
    // asking again about the same account reads as not having listened.
    app.record_consent();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cached_outcome_is_complete_by_construction() {
        let outcome = ListOutcome::cached(7, 0, false);
        assert!(outcome.is_complete());
        assert_eq!(outcome.source, ResultSource::Cached);
    }
}
