//! Getting a list, and everything that decides how.
//!
//! This is the part of the tool that has rules rather than opinions: who is
//! being asked about, whether the network has to be touched at all, what a
//! walk costs, and when a stored answer is still true. It returns data and a
//! description of where the data came from; it never decides how any of it
//! looks. That is [`crate::commands`]' job.
//!
//! Every list, crossing and summary the tool prints comes out of [`list`].

pub mod check;
pub mod cooldown;
pub mod freshness;
pub mod people;
pub mod target;
pub mod walk;
pub mod watch;

use anyhow::Result;
use snob_core::Pk;
use snob_core::model::{ListKind, StopReason, User};
use snob_core::store::{accounts, snapshots, users};

use crate::app::{App, ConsentInAdvance};
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
    /// When the walk that produced this list **began**.
    ///
    /// Half of an interval, and the half that used to be missing. A list is not
    /// an instant: walking six thousand accounts at the documented pace takes
    /// twenty minutes, and every question about whether two lists describe one
    /// moment is really about the time between the two walks rather than
    /// between the two moments they happened to finish at.
    pub started_at: i64,
    /// When it finished. The date shown to a person, and the one the store
    /// orders by.
    pub taken_at: i64,
    /// Whose list this is.
    ///
    /// The caller asked with a name and gets back an id, which is the only
    /// answer that cannot be wrong: a name has to be resolved, may not be the
    /// spelling Instagram uses, and — for your own account — may not be known
    /// at all until something goes and looks it up.
    pub account_pk: Pk,
    /// The stored capture these users came out of.
    ///
    /// Carried rather than looked up afterwards. The monitor has to know which
    /// row this is to compare it against the one it last reported, and asking
    /// the store for "the newest one" after the fact is a different question:
    /// another process sharing this database — the very thing the request
    /// budget is built to expect — can have closed a walk in between, and the
    /// answer would then name a capture these users did not come from.
    pub snapshot_id: i64,
    /// What Instagram actually said, when a walk stopped because it said
    /// something.
    ///
    /// [`StopReason`] is coarser than the error behind it on purpose — the
    /// store only needs to know whether the list is usable. But
    /// `SessionInvalid` covers both "log in again" and "Instagram wants the
    /// account verified", and those are exit code 3 and exit code 4, which the
    /// v2 service is meant to be able to tell apart without reading English.
    pub stopped_by: Option<ExitCode>,
    /// Whether a second run would continue this walk rather than start it
    /// again.
    ///
    /// Asked of the store once the snapshot is closed, rather than worked out
    /// from [`Self::reason`], because the reason does not know: `Truncated`
    /// arrives both from the reclassification that happens after pagination has
    /// already ended — no cursor to store — and from the four guards that stop
    /// in the middle of it with one saved. Only the store can tell those apart,
    /// and only the store knows whether the partial has already aged out of the
    /// resume window.
    pub resumable: bool,
}

impl ListOutcome {
    /// What a stored snapshot answers with. Complete by construction: the
    /// store only ever hands back snapshots that are.
    ///
    /// The provenance is not defaulted here. Every caller has to say which of
    /// the three storage paths it is, because getting that wrong is the whole
    /// of the bug this argument exists to prevent.
    ///
    /// The row itself rather than three fields picked out of it. Two of them
    /// are epoch seconds in the same unit and would sit next to each other in
    /// every call, which is one transposition away from a list claiming to have
    /// finished before it started; and the account id used to come from the
    /// caller while the members came from the row, which is a second pair that
    /// could disagree.
    pub(crate) fn cached(snapshot: &snapshots::Snapshot, provenance: Provenance) -> Self {
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
            started_at: snapshot.started_at,
            // The view this comes from cannot return an open snapshot, so the
            // fallback is unreachable. It used to be written out at all three
            // call sites.
            taken_at: snapshot.taken_at.unwrap_or_default(),
            account_pk: snapshot.account_pk,
            snapshot_id: snapshot.id,
            stopped_by: None,
            // A stored list is a finished one — the view this comes from cannot
            // return anything else — so there is nothing left to continue.
            resumable: false,
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
/// 2. With `--cache` the network is off, resolution included — and with it the
///    consent question, which is about enumerating somebody rather than about
///    reading what was already enumerated.
/// 3. Otherwise, someone else's account needs consent before it is enumerated,
///    and before it is resolved.
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

    // **`--cache` is not asked about**, because there is nothing to agree to:
    // consent governs enumerating somebody else's lists, and this reads a list
    // that was already walked — with permission — off this machine's own disk.
    // Nothing is resolved over the network either, so the rule that consent
    // comes before resolution is not in play.
    //
    // Asking anyway cost more than a redundant prompt. `ui::can_be_asked` is
    // false without a terminal, so `snob unfollowers someone --cache` from cron
    // or down a pipe exited 130 with "there is no terminal to ask at" over an
    // answer that costs nothing and touches nobody. Interactively it warned
    // about "a heavier request" that was never going to be made.
    //
    // And it disagreed with the tool's other storage path: `cooldown::serve`
    // hands back the identical stored lists with no question at all, and says
    // in as many words that none is asked because nothing is enumerated. The
    // same data was gated or not depending on whether Instagram happened to be
    // throttling.
    if !args.cache {
        ask_consent(app, args).await?;
    }

    // **Moved, not added.** The check at the top cannot see a cooldown that
    // landed while the confirmation prompt was open — one written by the
    // service sharing this database, for instance — and resolving is itself a
    // request, so it belongs here rather than after. It used to sit past
    // `target::resolve`, which meant a named target spent exactly the counter
    // poll `cooldown.rs` says must never be spent: "nothing may be spent — not
    // even the counter poll".
    //
    // Only the existing regression test's fixture hid it, by leaving `target`
    // as `None` — the one shape that resolves without a request.
    if let Some(until_ms) = app.client().pacer().cooldown()? {
        return cooldown::serve(app, args, kind, until_ms);
    }

    // A crossing asks for two lists, and resolving is a request. Reusing what
    // the first call worked out is what stops the second asking Instagram the
    // identical question about the identical account seconds later.
    let target = match app.resolved_target(args.target.as_deref()) {
        Some(target) => target,
        None => {
            let target = if args.cache {
                target::from_store(app, args.target.as_deref(), kind)?
            } else {
                target::resolve(app, args).await?
            };
            app.remember_target(args.target.as_deref(), target.clone());
            target
        }
    };

    // The user row has to exist before the account row: `accounts.pk`
    // references `users.pk`.
    //
    // Two calls rather than one, because "we know this account exists" and "we
    // know what it is called" are different claims. Writing a name we never
    // learned is how the numeric id ended up in `users.username`, clobbering a
    // correct stored one and filing a rename that never happened.
    match target.username.as_deref() {
        Some(username) => users::upsert(
            app.db().conn(),
            &User {
                pk: target.pk,
                username: username.to_string(),
                // Nothing is invented: the metadata arrives with the walk, and
                // the upsert leaves whatever it already had alone.
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            },
        )
        .map(|_| ())?,
        None => users::ensure(app.db().conn(), target.pk)?,
    }
    accounts::upsert(app.db().conn(), target.pk, target.is_self)?;

    let stored = snob_core::store::snapshots::latest_complete(app.db().conn(), target.pk, kind)?;

    if args.cache {
        let Some(snapshot) = stored else {
            return Err(crate::report::refuse_nothing_stored(kind));
        };
        return Ok((
            snob_core::store::snapshots::members(app.db().conn(), snapshot.id)?,
            // `--cache` is a promise not to spend a request, so nothing here
            // checked whether the stored list is still true. That is exactly
            // what makes it unsafe to cross against another one.
            ListOutcome::cached(&snapshot, Provenance::CacheFlag),
        ));
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
    ask_consent_with(app, args, ui::can_be_asked()).await
}

/// Split from [`ask_consent`] so a test can say whether anybody is there.
///
/// **The third argument is for tests only.** Being a terminal is a property of
/// the process's streams, which `cargo test` answers differently depending on
/// where the suite was started from — the same reason `Presentation` carries
/// `interactive` as data rather than asking at the point of use.
#[doc(hidden)]
pub async fn ask_consent_with(
    app: &mut App,
    args: &ListArgs,
    someone_is_there: bool,
) -> Result<()> {
    let Some(typed) = args.target.as_deref() else {
        return Ok(()); // your own account, nothing to agree to
    };
    // Cleaned before it is used as a key, so the answer is filed under the
    // account rather than under a spelling of it. `clean` only strips a leading
    // at sign and is idempotent, so this holds whether or not the name arrives
    // already cleaned.
    let name = target::clean(typed);
    if args.yes || app.has_consent(name) {
        return Ok(());
    }

    // Compared raw, deliberately. A typed name with a zero-width character in
    // it is not your own account, and filtering before this comparison would
    // make it match — which skips the question for somebody else's lists.
    let is_own_name = app
        .viewer()
        .username
        .as_deref()
        .is_some_and(|mine| mine.eq_ignore_ascii_case(name));
    if is_own_name {
        return Ok(());
    }

    // How an account is named on screen is `target::label`'s question, and it is
    // the same question here: `args.target` is `Some` at this point, so `label`
    // returns exactly the at sign and the filtered name these three sentences
    // want. Repeating the rule was how one of them ended up unfiltered.
    let shown = target::label(app, args);

    // Being unable to ask and being told no are two different events, and they
    // were reported as one. `confirm` answers with its default the moment
    // nobody can answer, so `snob unfollowers someone > out.json` failed with
    // "canceled", blaming the user for something nobody did.
    //
    // What is asked here is whether somebody is at the keyboard, and nothing
    // else. The question is written to standard error and the answer read from
    // standard input, so a pipe or a redirect on standard output does not touch
    // it — and `snob scan someone | jq` is a shape the README promises, which
    // this used to refuse with exit 130 before spending a single request. The
    // comment here used to justify the wider gate by saying the question went
    // to standard output. That stopped being true when the prompt moved to
    // standard error, and the gate did not follow it.
    if !someone_is_there {
        // Which way to answer in advance is the *caller's* fact, not this
        // function's. Both commands that reach here take an answer beforehand
        // and they do not take it the same way, and one sentence named `-y` for
        // both — so `snob watch once someone` refused with advice that then
        // failed to parse, because `watch once` deliberately has no `-y`.
        let in_advance = match app.consent_in_advance() {
            ConsentInAdvance::Flag => "Pass -y to confirm in advance.".to_string(),
            ConsentInAdvance::WatchConfig => format!(
                "Run \"snob watch setup\" to answer it once, or ask about {shown} \
                 while you are here."
            ),
        };
        return Err(ExitError::new(
            ExitCode::Interrupted,
            format!(
                "reading {shown}'s lists needs confirmation, and there is no terminal to \
                 ask at. {in_advance}"
            ),
        )
        .into());
    }

    app.warn(
        "this reads a list that belongs to somebody else, and lands their followers \
         in your local database. It is also a heavier request than reading your own, \
         and Instagram is readier to refuse it",
    );
    if !ui::confirm_off_thread(app.progress(), format!("Continue with {shown}?"), false).await? {
        // No mention of -y here. They have just said no, and answering that
        // with "pass the flag that skips the question" is telling them to do
        // it anyway.
        return Err(ExitError::new(
            ExitCode::Interrupted,
            format!("nothing was done: {shown} was not confirmed"),
        )
        .into());
    }
    // Asked and answered. A crossing wants two lists and a summary four, and
    // asking again about the same account reads as not having listened.
    app.record_consent(name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cached_outcome_is_complete_by_construction() {
        let snapshot = snapshots::Snapshot {
            id: 1,
            account_pk: 7,
            kind: ListKind::Followers,
            started_at: 0,
            taken_at: Some(0),
            member_count: 0,
            declared_count: None,
            next_cursor: None,
        };
        let outcome = ListOutcome::cached(&snapshot, Provenance::CounterVerified);
        assert!(outcome.is_complete());
        assert_eq!(outcome.source(), ResultSource::Cached);
        // Both ends of the interval come off the row, so they cannot disagree
        // with the members read from the same one.
        assert_eq!(outcome.account_pk, 7);
    }
}
