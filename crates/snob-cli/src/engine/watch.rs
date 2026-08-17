//! What the monitor found, and what it was allowed to conclude from it.
//!
//! Two ways in. [`from_store`] answers out of what is already on disk and
//! spends nothing, which is what makes `snob watch diff` free to run as often
//! as anybody likes. [`tick`] goes and looks first.
//!
//! `tick` does not have a caching policy of its own, and that is deliberate:
//! it calls [`crate::engine::list`], which is the one place every list in this
//! tool comes out of and which already holds the whole order — cooldown,
//! consent, counter poll, freshness, walk. A second policy here would be a
//! second policy to keep in agreement with the first.
//!
//! Both return data and the interval it covers. What any of it looks like is
//! [`crate::commands::watch`]'s question.

use anyhow::{Result, bail};
use snob_core::Pk;
use snob_core::model::{ListKind, StopReason, User};
use snob_core::store::{accounts, snapshots, users, watch as store};
use snob_core::watch::{Basis, Changes, ListDiff, Rename};

use crate::app::App;
use crate::cli::ListArgs;
use crate::engine::{self, ListOutcome, Provenance, target};
use crate::exit::ExitCode;

/// What one list has to report.
#[derive(Debug, Clone)]
pub struct ListReport {
    pub kind: ListKind,
    pub basis: Basis,
    /// When the last report about this list was made. `None` when there has
    /// never been one.
    ///
    /// The receipt's moment, not the marked capture's `taken_at`. Those come
    /// apart the moment somebody types `snob followers` between two runs, and
    /// this is the one that says what has actually been said out loud.
    pub since: Option<i64>,
    /// The rename history row the last report stopped at. Zero when there has
    /// never been one, which is also what an empty history reads as.
    pub history_cursor: i64,
    /// When the newest capture was taken.
    pub until: i64,
    pub diff: ListDiff,
    /// How many accounts the newest capture holds, so a report can say "three
    /// left, of a hundred and forty" without the caller counting again.
    pub total: usize,
}

impl ListReport {
    /// Whether this is a comparison that actually happened, as opposed to a
    /// baseline being laid down or a list nothing has touched.
    pub fn compared(&self) -> bool {
        matches!(self.basis, Basis::Compare { .. })
    }
}

/// Everything storage can say about one account.
#[derive(Debug, Clone)]
pub struct WatchReport {
    pub account_pk: Pk,
    /// The name, when one was ever learned. Filtered by whoever draws it, not
    /// here: this module returns data.
    pub username: Option<String>,
    pub is_self: bool,
    /// `None` when nothing of that list has ever been walked to completion.
    pub followers: Option<ListReport>,
    pub following: Option<ListReport>,
    pub renamed: Vec<Rename>,
}

impl WatchReport {
    /// The changes, in the shape the payload and the printer both want.
    pub fn changes(&self) -> Changes {
        Changes {
            followers: self.diff_of(ListKind::Followers),
            following: self.diff_of(ListKind::Following),
            renamed: self.renamed.clone(),
        }
    }

    fn diff_of(&self, kind: ListKind) -> ListDiff {
        self.report(kind)
            .map(|r| r.diff.clone())
            .unwrap_or_default()
    }

    pub fn report(&self, kind: ListKind) -> Option<&ListReport> {
        match kind {
            ListKind::Followers => self.followers.as_ref(),
            ListKind::Following => self.following.as_ref(),
        }
    }

    /// Whether anything at all has ever been walked for this account.
    ///
    /// Told apart from "nothing changed", which is what the caller would
    /// otherwise print at somebody who has never run the tool on this account.
    pub fn has_anything_stored(&self) -> bool {
        self.followers.is_some() || self.following.is_some()
    }
}

/// An account this monitor watches.
///
/// The consent is a field rather than a flag the caller passes, because it is
/// the one thing that decides whether a run may go ahead with nobody at the
/// keyboard. A person typing `snob watch once someone` gets asked, the way
/// every other command asks. An unattended run cannot be asked, so it needs an
/// answer that was already given — and [`Watched::may_run_unattended`] is what
/// says whether it has one. A `yes` the program grants itself is not consent,
/// and there is no constructor here that produces one.
#[derive(Debug, Clone)]
pub struct Watched {
    /// `None` is the account the session belongs to: nothing to agree to.
    target: Option<String>,
    consent: Option<Consent>,
}

/// A recorded answer to "may this walk somebody else's lists?".
///
/// Deliberately not a `bool`. A bool can be set to `true` by whatever needs it
/// to be true; this carries when it was given, so what reaches the walk is a
/// record of an answer rather than a decision made at the point of use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Consent {
    /// When it was given, in epoch seconds.
    pub given_at: i64,
}

impl Watched {
    /// The account the session belongs to.
    pub fn own() -> Self {
        Self {
            target: None,
            consent: None,
        }
    }

    /// Somebody else, with the question still to be asked. What a person
    /// typing the command gets: the prompt is where the answer comes from.
    pub fn asking(username: String) -> Self {
        Self {
            target: Some(username),
            consent: None,
        }
    }

    /// Somebody else, with an answer already on record.
    pub fn consented(username: String, consent: Consent) -> Self {
        Self {
            target: Some(username),
            consent: Some(consent),
        }
    }

    /// Whether this may run with nobody there to answer a question.
    ///
    /// Your own account always may. Somebody else's may only when the answer
    /// was already given, because the alternative is a scheduled service
    /// enumerating a stranger's lists on nobody's say-so.
    pub fn may_run_unattended(&self) -> bool {
        self.target.is_none() || self.consent.is_some()
    }

    /// The arguments a tick runs a list command with.
    ///
    /// The only place `yes` is ever set, and only when a [`Consent`] is on
    /// record. `refresh` stays off because the counter poll is what decides
    /// whether to walk, and `cache` stays off because a promise not to look is
    /// not a monitor.
    fn list_args(&self) -> ListArgs {
        ListArgs {
            target: self.target.clone(),
            yes: self.consent.is_some(),
            hide: vec![],
            only: vec![],
            no_verified: false,
            exclude_list: None,
            format: None,
            output: None,
            limit: None,
            refresh: false,
            cache: false,
            // A stored capture whose counter has not moved is still current, so
            // reusing it is right and costs nothing. It reads as `Unchanged`
            // against the mark, which is exactly the answer.
            max_age: std::time::Duration::from_secs(6 * 3600),
            no_resume: false,
            max_pages: None,
            no_progress: true,
        }
    }
}

/// One list, as this run found it.
#[derive(Debug)]
pub struct TickList {
    pub kind: ListKind,
    pub provenance: Provenance,
    pub reason: StopReason,
    /// Why this list could not be compared, when it could not.
    pub skipped: Option<Skipped>,
}

/// Why a run declined to draw a conclusion from a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// Nothing in this run established that the list still describes the
    /// account. That is the three storage paths where no request was spent
    /// finding out: a cooldown, a failed poll, and `--cache`.
    NobodyLooked(Provenance),
    /// The walk did not finish, so accounts are missing from it — and every one
    /// of them would be reported as somebody who left.
    Incomplete(StopReason),
}

/// What a tick did.
#[derive(Debug)]
pub struct TickReport {
    pub report: WatchReport,
    pub requests: u32,
    pub lists: Vec<TickList>,
    /// The captures this report spoke about, and the moment it covers.
    ///
    /// Held rather than recomputed because [`commit`] has to move exactly these
    /// marks and no others: a list that was refused is absent, and asking the
    /// store again afterwards would find it and mark it anyway.
    committable: Vec<(ListKind, i64)>,
    at: i64,
    history_cursor: i64,
}

/// A report on its way to somewhere, ready to be made durable.
///
/// The bytes rather than the events, because the signature covers bytes and a
/// retry has to send the same string.
pub struct Queued<'a> {
    pub run_id: &'a str,
    pub body: &'a str,
}

/// Records that this report has been reported.
///
/// Split from [`tick`] so the body can be built in between, which is where it
/// belongs: what a report looks like on the wire is presentation, and `engine`
/// does not decide how anything looks. What is not split is the writing —
/// queueing the report and retiring the marks happen in one transaction, for
/// the reason `store::watch::commit_report` sets out.
pub fn commit(
    app: &mut App,
    tick: &TickReport,
    delivery: Option<Queued<'_>>,
) -> Result<Option<i64>> {
    let pk = tick.report.account_pk;
    let at = tick.at;
    let (_, db, _) = app.parts();

    let queued = store::commit_report(
        db,
        pk,
        &tick.committable,
        at,
        tick.history_cursor,
        delivery.map(|d| (d.run_id, d.body)),
    )?;

    // Recorded whatever came of it, including a run that concluded nothing.
    // The marks only move when a list was compared, so a monitor sitting in a
    // cooldown for two days moves none of them — and from outside that looks
    // exactly like a monitor that was killed on Monday. This is what lets
    // `status` tell them apart.
    //
    // Not part of the transaction above: that one exists so a report cannot be
    // retired without being queued, and a bookkeeping row has no business being
    // able to fail it.
    let record = store::record_run(
        db.conn(),
        &store::Run {
            account_pk: pk,
            started_at: at,
            finished_at: Some(snob_core::store::now()),
            requests: tick.requests,
            outcome: Some(tick.outcome().as_str().to_string()),
            changes: tick.report.changes().len() as u32,
        },
    );
    if let Err(e) = record {
        tracing::warn!(error = %e, "the run could not be recorded");
    }

    // After the marks have moved, and outside their transaction. A monitor on a
    // six-hour schedule leaves four captures a day per list and nothing reads
    // the old ones — the diff only ever compares against the last reported one.
    // What `prune` keeps, and why each of them, is on `prune` itself.
    //
    // A failure here does not fail the run. The report has been made and
    // recorded; a database that could not be tidied is worth a line in the log
    // and nothing more.
    match store::prune(db.conn(), at) {
        Ok(removed) if removed > 0 => {
            tracing::debug!(removed, "expired captures nothing needs any more");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "old captures could not be expired"),
    }

    Ok(queued)
}

impl TickReport {
    /// A report that never ran, for a test that only cares what it looks like.
    ///
    /// **Tests only.** The three fields it leaves empty are the ones that say
    /// what to commit, and they are private precisely so nothing outside this
    /// module can decide that — a caller that could set `committable` could
    /// retire the mark of a list the run refused.
    #[doc(hidden)]
    pub fn for_test(report: WatchReport, requests: u32) -> Self {
        Self {
            report,
            requests,
            lists: Vec::new(),
            committable: Vec::new(),
            at: 0,
            history_cursor: 0,
        }
    }

    /// Whether this run established anything at all.
    ///
    /// A caller that sends the result somewhere has to be able to say "nothing
    /// changed" apart from "I could not look", because an automation watching
    /// for silence reads them as the same thing and they are opposites.
    pub fn looked(&self) -> bool {
        self.lists.iter().any(|l| l.skipped.is_none())
    }

    /// What this run amounts to, in the vocabulary `$?` and the README's table
    /// already use.
    ///
    /// Recorded rather than reconstructed later, because the reasons a list was
    /// refused are held on the run and nothing else keeps them. A run that
    /// could not look at either list is reported as what stopped it, so
    /// `status` can say "in cooldown" rather than only "quiet since Monday".
    pub fn outcome(&self) -> ExitCode {
        if self.looked() {
            return ExitCode::Ok;
        }
        // Every list was refused. The most specific reason wins: a cooldown is
        // something that lifts, and saying so is more use than "error".
        self.lists
            .iter()
            .find_map(|list| match list.skipped {
                Some(Skipped::NobodyLooked(Provenance::Cooldown)) => Some(ExitCode::RateLimited),
                Some(Skipped::Incomplete(reason)) => Some(ExitCode::from_stop_reason(reason)),
                _ => None,
            })
            .unwrap_or(ExitCode::Error)
    }
}

/// Goes and looks, then reports what changed since the last time it did.
///
/// The order is the whole of it:
///
/// 1. Get both lists through [`crate::engine::list`], which decides on its own
///    whether anything has to be fetched. Two requests when nothing moved.
/// 2. Refuse to conclude anything from a list this run did not verify, or one
///    that came back short. **The mark does not move for a refused list**: what
///    was never reported stays unreported, and the next run says it.
/// 3. Compare what is left against the receipt, and move it.
pub async fn tick(app: &mut App, watched: &Watched) -> Result<TickReport> {
    let args = watched.list_args();
    let before = app.client().pacer().spent();

    let mut lists = Vec::new();
    let mut usable = Vec::new();
    // Whose account this turned out to be, taken from the engine's answer
    // rather than from the viewer: on a third party they are different, and the
    // id the engine reports is the one that cannot be wrong about it.
    let mut pk = app.viewer().pk;

    for kind in [ListKind::Followers, ListKind::Following] {
        let (_, outcome) = engine::list(app, &args, kind).await?;
        pk = outcome.account_pk;

        let skipped = refusal(&outcome);
        if skipped.is_none() {
            usable.push((kind, outcome.snapshot_id));
        }
        lists.push(TickList {
            kind,
            provenance: outcome.provenance,
            reason: outcome.reason,
            skipped,
        });
    }

    // Read before the comparison, and both handed back, so that whatever
    // commits this report writes the same two numbers the renames were read
    // against. Read again at commit time, a rename filed in between would be
    // marked as reported without having been.
    let at = snob_core::store::now();
    let history_cursor = store::history_head(app.db().conn())?;

    // `advance: false`. The marks move in `commit`, together with the report
    // being queued — reporting and recording having reported are one event.
    let report = compare(app, pk, &usable, None)?;

    Ok(TickReport {
        report,
        requests: app.client().pacer().spent().saturating_sub(before),
        lists,
        committable: usable,
        at,
        history_cursor,
    })
}

/// Whether a list may be the basis of a comparison, and why not when it may not.
///
/// Both questions are asked of the outcome rather than worked out here.
/// `describes_now` is the existing answer to "did anything in this run
/// establish that this list is still true", and the three provenances that say
/// no are the three where no request was spent finding out — which is what
/// makes them safe to print and unsafe to compare.
fn refusal(outcome: &ListOutcome) -> Option<Skipped> {
    if !outcome.provenance.describes_now() {
        return Some(Skipped::NobodyLooked(outcome.provenance));
    }
    if !outcome.is_complete() {
        return Some(Skipped::Incomplete(outcome.reason));
    }
    None
}

/// Reads the report without touching the network.
///
/// `advance` decides whether the marks move. `snob watch diff` looks and leaves
/// them alone — a question whose answer changes when it is asked is one nobody
/// can check.
pub fn from_store(app: &App, typed: Option<&str>, advance: bool) -> Result<WatchReport> {
    let (pk, username) = resolve(app, typed)?;

    // From the view, so an interrupted walk cannot become a basis for
    // comparison. It is the same guard the static crossings already lean on,
    // and it matters more here: the accounts missing from a half-walked list
    // would be reported as people who left.
    let mut usable = Vec::new();
    for kind in [ListKind::Followers, ListKind::Following] {
        if let Some(latest) = snapshots::latest_complete(app.db().conn(), pk, kind)? {
            usable.push((kind, latest.id));
        }
    }

    // Both read before the comparison, so a rename filed while it runs lands on
    // the next report's side rather than being marked as said without having
    // been said.
    let advance = advance
        .then(|| -> Result<_> {
            Ok((
                snob_core::store::now(),
                store::history_head(app.db().conn())?,
            ))
        })
        .transpose()?;

    let mut report = compare(app, pk, &usable, advance)?;
    report.username = username;
    Ok(report)
}

/// Compares each named capture against its receipt.
///
/// The one place a comparison is made, so that a tick and a plain look cannot
/// disagree about what "since the last report" means. The captures are named by
/// the caller because the two callers know them differently: a tick has the id
/// the walk it just ran produced, which is the only id that is certainly the
/// one those users came from, while a look asks the store for the newest.
///
/// `advance` is `Some` only for the caller that does not queue a report — a
/// tick hands `None` and commits later, together with the report itself. The
/// two numbers come in rather than being read here, so that both callers write
/// the same pair the renames were read against.
fn compare(
    app: &App,
    pk: Pk,
    usable: &[(ListKind, i64)],
    advance: Option<(i64, i64)>,
) -> Result<WatchReport> {
    let mut followers = None;
    let mut following = None;
    for &(kind, snapshot_id) in usable {
        let report = list_report(app, pk, kind, snapshot_id)?;
        match kind {
            ListKind::Followers => followers = report,
            ListKind::Following => following = report,
        }
    }

    // Looked for in one capture, not both. An account can be in the followers
    // list and the following list at once — that is what a friend is — and
    // asking about each list separately would report their rename twice.
    let renamed = match followers.as_ref().or(following.as_ref()) {
        Some(report) if report.compared() => store::renames_since(
            app.db().conn(),
            report.basis.mark_to(),
            report.history_cursor,
        )?,
        _ => Vec::new(),
    };

    if let Some((at, head)) = advance {
        // Only the lists that made it this far. A list the caller left out was
        // refused — nobody looked, or the walk came back short — and moving its
        // mark would file it as reported when nothing was said about it, which
        // loses whatever changed in it for good.
        for report in [followers.as_ref(), following.as_ref()]
            .into_iter()
            .flatten()
        {
            store::set_mark(
                app.db().conn(),
                pk,
                report.kind,
                report.basis.mark_to(),
                at,
                head,
            )?;
        }
    }

    Ok(WatchReport {
        account_pk: pk,
        username: users::name(app.db().conn(), pk)?,
        is_self: app.viewer().pk == pk,
        followers,
        following,
        renamed,
    })
}

fn list_report(app: &App, pk: Pk, kind: ListKind, snapshot_id: i64) -> Result<Option<ListReport>> {
    let conn = app.db().conn();
    // `find_usable`, not `find`: an id that names a walk which stopped short
    // answers `None` here rather than handing back a capture with accounts
    // missing from it.
    let Some(latest) = snapshots::find_usable(conn, snapshot_id)? else {
        return Ok(None);
    };

    let mark = store::mark(conn, pk, kind)?;
    // A receipt whose capture has been pruned carries no baseline, so it is
    // handed to `decide` as the absence it is and the next report starts over.
    // What survives is the history cursor, so the renames are not re-announced.
    let Some(basis) = Basis::decide(mark.and_then(|m| m.snapshot_id), Some(latest.id)) else {
        return Ok(None);
    };

    let diff = match basis {
        // Nothing is reported and nothing is read: a baseline has no earlier
        // capture, and an unchanged list has nothing new in it. Not reading the
        // members is the whole reason the common case is cheap.
        Basis::Baseline { .. } | Basis::Unchanged { .. } => ListDiff::default(),
        Basis::Compare { before, after } => ListDiff::between(
            &snapshots::members(conn, before)?,
            &snapshots::members(conn, after)?,
        ),
    };

    Ok(Some(ListReport {
        kind,
        basis,
        since: mark.map(|m| m.compared_at),
        history_cursor: mark.map(|m| m.history_cursor).unwrap_or_default(),
        until: latest.taken_at.unwrap_or_default(),
        diff,
        total: latest.member_count as usize,
    }))
}

/// Which account this is about, from storage alone.
///
/// Deliberately not `target::from_store`: that one refuses with a sentence
/// about `--cache`, which is a flag this command does not have. What the user
/// has to do here is walk the account once, and the refusal says so.
fn resolve(app: &App, typed: Option<&str>) -> Result<(Pk, Option<String>)> {
    let Some(typed) = typed else {
        let viewer = app.viewer();
        return Ok((viewer.pk, viewer.username.clone()));
    };

    let name = target::clean(typed);
    let Some(pk) = accounts::find_pk_by_username(app.db().conn(), name)? else {
        bail!(
            "nothing is stored about @{}. Run \"snob followers {}\" once and the monitor \
             will have something to compare against from then on.",
            snob_core::model::printable(name),
            snob_core::model::printable(name),
        );
    };

    Ok((pk, users::name(app.db().conn(), pk)?))
}

/// Users in a capture, for a caller that has an id and wants the people.
///
/// Re-exported rather than reached for through `snapshots` so that the monitor
/// has one door to the members and the guard about which captures are readable
/// stays on this side of it.
pub fn members(app: &App, snapshot_id: i64) -> Result<Vec<User>> {
    Ok(snapshots::members(app.db().conn(), snapshot_id)?)
}
