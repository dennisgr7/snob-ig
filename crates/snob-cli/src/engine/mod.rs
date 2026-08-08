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
use crate::exit::{ExitCode, ExitError};
use crate::ui;

/// Where a returned list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultSource {
    /// A fresh walk against Instagram.
    Fetched,
    /// A stored snapshot that still holds.
    Cached,
}

/// Where a returned list came from, in the sense that decides whether two of
/// them may be crossed against each other.
///
/// This used to be a `bool` called `from_cooldown`, and two of the three paths
/// that serve from storage set it to `false` — so `snob unfollowers --cache`
/// crossed a followers list stored in June against a following list stored in
/// August and reported the difference as unfollowers, with nothing on screen
/// saying either list came out of storage.
///
/// The distinction that matters is not how old a stored list is. It is whether
/// anything in **this run** established that it still describes the account:
/// two lists a month apart are fine to cross if both counters were checked just
/// now and neither had moved, and two lists an hour apart are not fine if
/// nobody looked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Walked in this run. It is the account as it is.
    Walked,
    /// Stored, and a counter poll in this run said it had not moved.
    CounterVerified,
    /// Stored, served during a cooldown. Nothing may be spent to check.
    Cooldown,
    /// Stored, served because the counter poll failed. No evidence either way.
    PollFailed,
    /// Stored, served because `--cache` said not to look.
    CacheFlag,
}

impl Provenance {
    /// Whether this list is known to describe the account as it is now.
    ///
    /// The three that answer `false` are the three where no request was spent
    /// finding out — deliberately, in every case. That is what makes them safe
    /// to *serve* and unsafe to *cross*.
    pub fn describes_now(self) -> bool {
        matches!(self, Self::Walked | Self::CounterVerified)
    }

    /// Whether the cause was Instagram pushing back rather than the user
    /// asking for storage. It decides which of two pieces of advice to give
    /// and which exit code goes with it.
    pub fn is_cooldown(self) -> bool {
        self == Self::Cooldown
    }

    fn source(self) -> ResultSource {
        match self {
            Self::Walked => ResultSource::Fetched,
            _ => ResultSource::Cached,
        }
    }
}

#[derive(Debug)]
pub struct ListOutcome {
    /// How this list was obtained, and therefore what is known about whether
    /// it is still true. [`ResultSource`] falls out of it, which is why there
    /// is no second field that could disagree with this one.
    pub provenance: Provenance,
    pub reason: StopReason,
    pub requests: u32,
    pub taken_at: i64,
    /// Whose list this is.
    ///
    /// The caller asked with a name and gets back an id, which is the only
    /// answer that cannot be wrong: a name has to be resolved, may not be the
    /// spelling Instagram uses, and — for your own account — may not be known
    /// at all until something goes and looks it up.
    pub account_pk: Pk,
    /// What Instagram actually said, when a walk stopped because it said
    /// something.
    ///
    /// [`StopReason`] is coarser than the error behind it on purpose — the
    /// store only needs to know whether the list is usable. But
    /// `SessionInvalid` covers both "log in again" and "Instagram wants the
    /// account verified", and those are exit code 3 and exit code 4, which the
    /// v2 service is meant to be able to tell apart without reading English.
    pub stopped_by: Option<ExitCode>,
}

impl ListOutcome {
    /// What a stored snapshot answers with. Complete by construction: the
    /// store only ever hands back snapshots that are.
    ///
    /// The provenance is not defaulted here. Every caller has to say which of
    /// the three storage paths it is, because getting that wrong is the whole
    /// of the bug this argument exists to prevent.
    pub(crate) fn cached(account_pk: Pk, taken_at: i64, provenance: Provenance) -> Self {
        debug_assert!(
            !matches!(provenance, Provenance::Walked),
            "a stored list was not walked"
        );
        Self {
            provenance,
            reason: StopReason::Completed,
            // Filled in by `list`, which is the only place that sees the whole
            // run and can ask the pacer what it really charged.
            requests: 0,
            taken_at,
            account_pk,
            stopped_by: None,
        }
    }

    pub fn source(&self) -> ResultSource {
        self.provenance.source()
    }

    pub fn is_complete(&self) -> bool {
        self.reason.yields_complete_list()
    }

    /// The code a command should exit with when it has to refuse this result.
    ///
    /// What Instagram said beats what the store had to record, because the
    /// store's vocabulary is about whether the list is usable and the exit
    /// code is about what the caller should do next.
    pub fn exit_code(&self) -> ExitCode {
        self.stopped_by
            .unwrap_or_else(|| ExitCode::from_stop_reason(self.reason))
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

    ask_consent(app, args).await?;

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
            // `--cache` is a promise not to spend a request, so nothing here
            // checked whether the stored list is still true. That is exactly
            // what makes it unsafe to cross against another one.
            ListOutcome::cached(
                target.pk,
                snapshot.taken_at.unwrap_or_default(),
                Provenance::CacheFlag,
            ),
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
/// a request: asking afterwards meant that answering "no" had still spent one
/// on a run the user never authorized.
///
/// The price of asking first is that the name shown is the one typed rather
/// than the one Instagram spells. That costs nothing when it is wrong, and the
/// only account it can wrongly ask about is your own, which needs you to have
/// typed your own name.
async fn ask_consent(app: &mut App, args: &ListArgs) -> Result<()> {
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

    // Being unable to ask and being told no are two different events, and they
    // were reported as one. `confirm` answers with its default the moment
    // there is no terminal, so `snob unfollowers someone > out.json` — a way
    // of running this the README advertises — failed with "canceled", blaming
    // the user for something nobody did. The question goes to standard output,
    // which is the very stream being captured.
    if !ui::is_interactive() {
        return Err(ExitError::new(
            ExitCode::Interrupted,
            format!(
                "reading @{name}'s lists needs confirmation, and there is no terminal to \
                 ask at. Pass -y to confirm in advance."
            ),
        )
        .into());
    }

    app.warn(
        "enumerating someone else's followers is the pattern Instagram's detection \
         systems watch most closely",
    );
    if !ui::confirm_off_thread(format!("Continue with @{name}?"), false).await? {
        // No mention of -y here. They have just said no, and answering that
        // with "pass the flag that skips the question" is telling them to do
        // it anyway.
        return Err(ExitError::new(
            ExitCode::Interrupted,
            format!("nothing was done: @{name} was not confirmed"),
        )
        .into());
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
        let outcome = ListOutcome::cached(7, 0, Provenance::CounterVerified);
        assert!(outcome.is_complete());
        assert_eq!(outcome.source(), ResultSource::Cached);
    }
}
