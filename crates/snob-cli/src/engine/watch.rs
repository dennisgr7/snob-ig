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
use snob_core::model::{ListKind, StopReason};
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

    /// Whether this run established that the list is still true, which is what
    /// makes a rename among its members worth reporting.
    ///
    /// `Unchanged` counts, and it did not: a rename moves nobody in or out of a
    /// list, so a capture whose counter was checked and had not moved is exactly
    /// as good a set of members to look for renames among as one that was walked.
    /// Only a baseline does not count -- announcing renames against one would
    /// report moves from before anything was ever reported.
    pub fn verified(&self) -> bool {
        matches!(self.basis, Basis::Compare { .. } | Basis::Unchanged { .. })
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

    /// Whose account this is, when it is not the session's.
    pub fn name(&self) -> Option<&str> {
        self.target.as_deref()
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
    /// Why this list could not be compared, when it could not.
    ///
    /// The provenance and the stop reason used to sit beside this and nothing
    /// read either: both facts are already inside `Skipped`, which is what
    /// every caller matches on.
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
    /// marks and no others: a list that was refused is absent, and so is one the
    /// comparison could not build a report for, and asking the store again
    /// afterwards would find both and mark them anyway.
    committable: Vec<(ListKind, i64)>,
    at: i64,
    /// How far along the rename history this report has covered, when it read a
    /// window at all. `None` means the cursor must not move.
    rename_cursor: Option<i64>,
    /// The `username_history` rows this tick is announcing, so no later one
    /// repeats them. A different question from the cursor, for the reason
    /// `007_renames_sent.sql` sets out.
    renames_sent: Vec<i64>,
}

/// A report on its way to somewhere, ready to be made durable.
///
/// The bytes rather than the events, because the signature covers bytes and a
/// retry has to send the same string.
pub struct Queued<'a> {
    pub run_id: &'a str,
    pub body: &'a str,
    /// The address this report is addressed to, so a later run cannot drain it
    /// through a client pointed somewhere else.
    pub destination: Option<&'a str>,
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
        tick.rename_cursor,
        &tick.renames_sent,
        delivery.map(|d| store::Queued {
            run_id: d.run_id,
            body: d.body,
            destination: d.destination,
        }),
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

    Ok(queued)
}

/// Expires what nothing needs any more: old captures, reports too old to be
/// news, and finished runs.
///
/// **Once per run, beside the queue drain**, and not at the end of a comparison,
/// which is where it used to be. `commit` is reached only through the delivery
/// step, so a run that ended earlier settled nothing at all: a session that had
/// gone, or a third party that went private, left a report past
/// `deliveries::MAX_AGE_SECS` sitting `pending` for ever — `due` will not hand
/// back an over-age row, and `failed` is the only thing that expires one, so
/// nothing could ever reach it. `deliveries::pending` went on counting it and
/// `snob watch status` went on promising that the next run would try it.
///
/// A failure here does not fail the run. Whatever the run did is already
/// recorded; a database that could not be tidied is worth a line in the log and
/// nothing more.
/// Takes the store rather than the `App`, because that is all it touches — and
/// because the one caller that most needs it has no session to build an `App`
/// from. `snob watch once` on a machine whose session has gone returns before
/// anything is opened, and that is exactly the run that leaves reports ageing
/// past `MAX_AGE_SECS` with `status` promising the next one will try them.
pub fn settle(db: &snob_core::store::Store, at: i64) {
    match store::prune(db.conn(), at) {
        Ok(removed) if removed > 0 => {
            tracing::debug!(removed, "expired what nothing needs any more");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "old captures could not be expired"),
    }
}

impl TickReport {
    /// A report that never ran, for a test that only cares what it looks like.
    ///
    /// **Tests only.** The three fields it leaves empty are the ones that say
    /// what to commit, and they are private precisely so nothing outside this
    /// module can decide that — a caller that could set `committable` could
    /// retire the mark of a list the run refused.
    #[doc(hidden)]
    pub fn for_test(report: WatchReport, requests: u32, at: i64) -> Self {
        Self {
            report,
            requests,
            lists: Vec::new(),
            committable: Vec::new(),
            at,
            rename_cursor: None,
            renames_sent: Vec::new(),
        }
    }

    /// When this run concluded, in epoch seconds.
    ///
    /// Read once, inside [`tick`], just before the comparison, and handed out
    /// rather than read again: this is the moment `commit_report` files the
    /// mark at, and a body or a stream line that asked the clock a second time
    /// would put a different moment on the same event. The field stays
    /// private, so only a real tick can decide it.
    pub fn at(&self) -> i64 {
        self.at
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
        // A canceled walk comes back `Ok`, so without this the loop went
        // straight on to the second list — and `App::resolved_target` drops the
        // remembered counters as soon as the pacer has moved, so it asked
        // Instagram again. The user had already been told "Stopping and saving
        // what has been fetched…".
        //
        // Recorded as a refusal rather than dropped: a list this run never
        // looked at must not be compared or marked, which is exactly what
        // `Skipped` means, and saying so keeps the report honest about why it
        // is short.
        if app.cancel().is_canceled() {
            lists.push(TickList {
                kind,
                skipped: Some(Skipped::Incomplete(StopReason::Canceled)),
            });
            continue;
        }

        // A cooldown stops *this list*; everything else stops the run.
        //
        // The cancel branch above records a refusal and carries on, and an
        // `Err` got no such treatment — so a completed followers walk whose
        // diff names three departures was thrown away with the second list's
        // failure, and no `watch_runs` row was written either. Nothing is lost
        // permanently: the capture is stored and the mark did not move, so the
        // next tick reports the same departures. On `--every 24h` that is a day
        // late, which is a long time to sit on "three people left".
        //
        // Classified rather than blanket-caught, and that distinction is the
        // whole item. Mapping every `Err` to a refusal would swallow a consent
        // refusal, a session that has gone and a challenge as "one list was
        // skipped" — turning the failures a run must surface into a short
        // report nobody notices. `RateLimited` is the one that is genuinely
        // scoped to what could be read now and lifts on its own, which is
        // exactly what `Skipped` was written to describe, and it is what both
        // `refuse_in_cooldown` and `refuse_cooldown_mid_walk` carry.
        let outcome = match engine::list(app, &args, kind).await {
            Ok((_, outcome)) => outcome,
            Err(e) if ExitCode::from_chain(&e) == Some(ExitCode::RateLimited) => {
                lists.push(TickList {
                    kind,
                    skipped: Some(Skipped::Incomplete(StopReason::RateLimit)),
                });
                continue;
            }
            Err(e) => return Err(e),
        };
        pk = outcome.account_pk;

        let skipped = refusal(&outcome);
        if skipped.is_none() {
            usable.push((kind, outcome.snapshot_id));
        }
        lists.push(TickList { kind, skipped });
    }

    // Read before the comparison, and both handed back, so that whatever
    // commits this report writes the same two numbers the renames were read
    // against. Read again at commit time, a rename filed in between would be
    // marked as reported without having been.
    let at = snob_core::store::now();
    let head = store::history_head(app.db().conn())?;

    // Nothing is written here. The marks and the cursor move in `commit`,
    // together with the report being queued — reporting and recording having
    // reported are one event.
    let compared = compare(app, pk, &usable, head)?;

    Ok(TickReport {
        report: compared.report,
        requests: app.client().pacer().spent().saturating_sub(before),
        lists,
        committable: compared.marks,
        at,
        rename_cursor: compared.rename_cursor,
        renames_sent: compared.renames_sent,
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

/// Reads the report without touching the network, and without recording it.
///
/// **It takes `&App`, and that is the guard rather than a convention.**
/// Recording a report means `commit_report`, which needs a `&mut Store`; there
/// is no way to reach one from a shared borrow, so this entry point is
/// incapable of moving a mark.
///
/// It used to be one function with an `advance: bool`, and `snob watch diff`
/// passed `false`. Flipping that one literal to `true` left the whole suite
/// green -- on the command whose entire contract is that asking twice gives the
/// same answer, and with `commit_report` called with nothing to queue, which is
/// the one direction its own doc says must never happen.
pub fn from_store(app: &App, typed: Option<&str>) -> Result<WatchReport> {
    Ok(look(app, typed)?.1.report)
}

/// Reads the report and records having made it, the way a tick does.
///
/// **Tests only.** Nothing in production wants this: a report that was recorded
/// but never sent anywhere is a window nobody will ever hear about again. It
/// exists so a test can put an account into "already reported" state without a
/// network to fetch a walk from, and it writes through the same `commit_report`
/// a tick writes through, so there is one writer of marks in the program rather
/// than two that can come to disagree about which lists a report spoke about.
#[doc(hidden)]
pub fn record_from_store(app: &mut App, typed: Option<&str>) -> Result<WatchReport> {
    let (pk, compared) = look(app, typed)?;

    let (_, db, _) = app.parts();
    store::commit_report(
        db,
        pk,
        &compared.marks,
        snob_core::store::now(),
        compared.rename_cursor,
        &compared.renames_sent,
        None,
    )?;

    Ok(compared.report)
}

/// The comparison both entry points make, and what committing it would write.
fn look(app: &App, typed: Option<&str>) -> Result<(Pk, Compared)> {
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

    // Read before the comparison, so a rename filed while it runs lands on the
    // next report's side rather than being marked as said without having been
    // said.
    let head = store::history_head(app.db().conn())?;
    let mut compared = compare(app, pk, &usable, head)?;
    compared.report.username = username;
    Ok((pk, compared))
}

/// Compares each named capture against its receipt.
///
/// The one place a comparison is made, so that a tick and a plain look cannot
/// disagree about what "since the last report" means. The captures are named by
/// the caller because the two callers know them differently: a tick has the id
/// the walk it just ran produced, which is the only id that is certainly the
/// one those users came from, while a look asks the store for the newest.
///
/// This writes nothing. It hands back what a commit would have to write, and
/// the two callers commit it differently — a tick together with the report it
/// queues, a plain look not at all.
///
/// `head` is the rename history's newest row, read by the caller **before** this
/// runs: a rename filed while the comparison is in progress then lands on the
/// next report's side rather than being filed as said without having been said.
fn compare(app: &App, pk: Pk, usable: &[(ListKind, i64)], head: i64) -> Result<Compared> {
    let mut followers = None;
    let mut following = None;
    for &(kind, snapshot_id) in usable {
        let report = list_report(app, pk, kind, snapshot_id)?;
        match kind {
            ListKind::Followers => followers = report,
            ListKind::Following => following = report,
        }
    }

    // Renames are looked for among the members of **every list this run
    // verified**, and deduplicated afterwards.
    //
    // This used to ask about one capture — followers when there was one — and
    // that was wrong three ways, all of them silent. Somebody only in the
    // following list, which is exactly the `unfollowers` set, had their rename
    // dropped and the cursor advanced past it. A run where followers was
    // `Unchanged` and following was compared reported none at all. And when one
    // list was refused its cursor stayed behind, so the next run read from the
    // older of the two and announced renames it had already sent.
    //
    // `verified` rather than `compared`, so an `Unchanged` list counts: a rename
    // moves nobody in or out of a list, and a capture whose counter was checked
    // this run is as good a set of members to look among as one that was walked.
    // Requiring a walk meant the documented common case — two unmoved counters,
    // one request — read no window at all while still advancing the cursor past
    // whatever was in it.
    //
    // One cursor for the account, not one per list, so there are no two numbers
    // to pick between. Anything that turns up is deduplicated by `pk`: a friend
    // is in both lists and is one person.
    let verified: Vec<&ListReport> = [followers.as_ref(), following.as_ref()]
        .into_iter()
        .flatten()
        .filter(|report| report.verified())
        .collect();

    // What this run announces, and the history rows behind them.
    //
    // **Already-sent is asked per row, not per watermark**, and that is the
    // whole of the fix `007_renames_sent` exists for. Whether a rename is
    // reported and whether the cursor may move are independent conditions, and
    // they come apart both ways: a tick with one list refused announces what it
    // can see and must not move the cursor, so the next tick re-reads the same
    // window and would announce the same rename again under a fresh `run_id`;
    // and a rename of somebody only in the refused list sits inside that same
    // window and is not found at all, so any watermark that suppresses the
    // first also buries the second for ever.
    let mut renamed: Vec<Rename> = Vec::new();
    let mut announcing: Vec<i64> = Vec::new();
    if !verified.is_empty() {
        let since = store::rename_cursor(app.db().conn(), pk)?;
        let already = store::renames_already_sent(app.db().conn(), pk)?;
        let mut seen = std::collections::HashSet::new();
        for report in &verified {
            for rename in
                store::renames_since(app.db().conn(), report.basis.mark_to(), since, head)?
            {
                if already.contains(&rename.history_id) {
                    continue;
                }
                if seen.insert(rename.pk) {
                    announcing.push(rename.history_id);
                    renamed.push(rename);
                }
            }
        }
    }

    // Whether the cursor may move, which is a different question from whether
    // there was anything to report, and it used to be answered by the same
    // `if`.
    //
    // It is **every list this account has a capture of**, and it counts a
    // baseline. Two defects came out of the old answer, in opposite directions:
    //
    // - `verified()` excludes a baseline, so after a first run that laid one
    //   down the cursor was still zero. The next run's lists are `Unchanged`,
    //   which does count, so it read the window from the beginning of time:
    //   every `username_history` row written by every ordinary `snob followers`
    //   since the first release, announced as news. Excluding the baseline
    //   deferred the very window it was written to suppress by exactly one run.
    // - `renames_since` joins the members of one capture, so it only ever sees
    //   the lists that were verified — but the cursor moved as soon as *any* of
    //   them was, and there is one cursor for the account. On an account whose
    //   `following` meets the truncation wall every tick, a rename of somebody
    //   only in `following` was stepped over permanently: no later run and no
    //   `snob watch diff` would ever surface it, because a rename moves nobody
    //   in or out of a list. `006_rename_cursor.sql` calls that shape a defect
    //   in as many words.
    //
    // A list the account has never had a capture of does not hold the cursor
    // back: there are no members to have missed a rename among. And a run that
    // reported on no list at all read no window, so there is nothing to file as
    // covered — an account nothing has ever walked does not even have a row for
    // the cursor to hang off.
    //
    // **Two corrections to that, both about what a capture is.**
    //
    // It asked `latest_complete`, which is true of members and false of
    // `username_history`. A walk that ends `Truncated` is not a capture
    // anything may be compared against — but it ran `save_page`, and
    // `save_page` runs `users::upsert`, so it filed history rows for the people
    // it did see, who are members of that list. On the account whose second
    // list meets the truncation wall every time, `latest_complete` answers
    // `None` for ever while that list goes on filing renames every run, and the
    // cursor closed over every one of them.
    //
    // And a baseline accounts for its list only on the account's **first**
    // report, which is the case the seeding rule above was written for: the
    // monitor starts now, and history from before it is not news. A baseline
    // later on is a different animal — it is a walled list finally completing —
    // and counting it there is what shut the recovery route, because the one
    // run that could see the owed renames is the run *after* it, and this run
    // had already closed the window.
    //
    // Asked of the marks rather than of the captures: `delete_partials` clears
    // a list's partials whenever a new walk starts, so counting captures says
    // "one" on the walled list's fifth attempt as readily as on its first. A
    // mark is what says something was ever reported.
    //
    // The cost is that a permanently walled list holds the window open, so it
    // grows. That is the honest direction — the alternative is losing what is in
    // it — and it is only affordable because announcing is no longer decided by
    // where the cursor is: `007_renames_sent` records what actually went out, so
    // a window read twice does not send anything twice.
    let verified_kinds: Vec<ListKind> = verified.iter().map(|report| report.kind).collect();
    let reported_on: Vec<ListKind> = [followers.as_ref(), following.as_ref()]
        .into_iter()
        .flatten()
        .map(|report| report.kind)
        .collect();

    let first_report = store::mark(app.db().conn(), pk, ListKind::Followers)?.is_none()
        && store::mark(app.db().conn(), pk, ListKind::Following)?.is_none();

    let mut every_list_accounted_for = true;
    for kind in [ListKind::Followers, ListKind::Following] {
        if verified_kinds.contains(&kind) {
            continue;
        }
        if !snapshots::any_capture(app.db().conn(), pk, kind)? {
            continue; // nothing was ever captured, so nothing was missed
        }
        if first_report && reported_on.contains(&kind) {
            continue; // the seeding baseline
        }
        every_list_accounted_for = false;
    }

    // Nothing filed after `head` is inside the window — enforced by
    // `renames_since` rather than asserted here, which is what the comment used
    // to do.
    let covered = (!reported_on.is_empty() && every_list_accounted_for).then_some(head);

    // The lists this report actually spoke about, for whoever commits it.
    //
    // Built here rather than by the caller, and that is the fix: the caller
    // handed everything that had not been refused, and `list_report` can still
    // answer with nothing — for a capture that turns out not to be usable, or a
    // baseline another process pruned between the mark being read and the capture
    // being looked up. The mark then advanced over a list nothing was said about,
    // which loses that window for good. The two callers also disagreed about it,
    // one marking a list the other left alone on identical state.
    let marks = [followers.as_ref(), following.as_ref()]
        .into_iter()
        .flatten()
        .map(|report| (report.kind, report.basis.mark_to()))
        .collect();

    Ok(Compared {
        report: WatchReport {
            account_pk: pk,
            username: users::name(app.db().conn(), pk)?,
            is_self: app.viewer().pk == pk,
            followers,
            following,
            renamed,
        },
        marks,
        rename_cursor: covered,
        renames_sent: announcing,
    })
}

/// A comparison, and what committing it would have to write.
///
/// The three come out together because they have to agree: the marks name the
/// lists the report spoke about, and the cursor is only set when the report read
/// a rename window. Working either out again at the point of writing is how they
/// came apart.
struct Compared {
    report: WatchReport,
    marks: Vec<(ListKind, i64)>,
    rename_cursor: Option<i64>,
    renames_sent: Vec<i64>,
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
        Basis::Compare { before, after } => {
            // The baseline is read through `find_usable` too, not just the
            // newer capture. `members` answers `Ok([])` for an id that is no
            // longer there, which is indistinguishable from a capture that was
            // genuinely empty — so a baseline pruned by another process between
            // reading the mark and reading its members would make every member
            // of the newer capture look like an arrival. Somebody would be told
            // three hundred people had just followed them.
            //
            // Within one process this is impossible: pruning a marked capture
            // sets the mark to NULL and `Basis::decide` answers `Baseline`. It
            // takes a second process's `prune` landing in between, which is
            // exactly the interleaving this branch has to survive.
            let Some(_) = snapshots::find_usable(conn, before)? else {
                return Ok(None);
            };
            ListDiff::between(
                &snapshots::members(conn, before)?,
                &snapshots::members(conn, after)?,
            )
        }
    };

    Ok(Some(ListReport {
        kind,
        basis,
        since: mark.map(|m| m.compared_at),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(provenance: Provenance, reason: StopReason) -> ListOutcome {
        ListOutcome {
            provenance,
            reason,
            requests: 0,
            started_at: 1_000,
            taken_at: 1_100,
            account_pk: 42,
            snapshot_id: 7,
            stopped_by: None,
            resumable: false,
        }
    }

    /// A walk that came back short is refused, and refused as *incomplete*
    /// rather than as unverified.
    ///
    /// The two halves of `refusal` mean different things to the caller: nobody
    /// looked is something the next run fixes, while a truncated walk is a list
    /// whose missing accounts would read as people who left. Only the first was
    /// ever asserted, so deleting the `is_complete` check entirely left the
    /// suite green -- and then a truncated walk is recorded `outcome = "ok"`,
    /// nothing is warned about, and `$?` tells a systemd timer the run went
    /// fine.
    #[test]
    fn a_walk_that_came_back_short_is_refused_as_incomplete() {
        for reason in [
            StopReason::RateLimit,
            StopReason::Truncated,
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Network,
            StopReason::SessionInvalid,
        ] {
            assert!(
                matches!(
                    refusal(&outcome(Provenance::Walked, reason)),
                    Some(Skipped::Incomplete(got)) if got == reason
                ),
                "a walk that ended {reason:?} was accepted as a basis for comparison"
            );
        }

        // A list nothing verified is the other refusal, and it wins: the run
        // never looked, so what the stored capture says about completeness is
        // beside the point.
        assert!(matches!(
            refusal(&outcome(Provenance::Cooldown, StopReason::Completed)),
            Some(Skipped::NobodyLooked(Provenance::Cooldown))
        ));

        assert!(
            refusal(&outcome(Provenance::Walked, StopReason::Completed)).is_none(),
            "a complete walk this run made is exactly what may be compared"
        );
    }

    /// And the run says so in the one place a timer can read without parsing
    /// English.
    #[test]
    fn a_run_whose_lists_all_came_back_short_does_not_exit_zero() {
        let report = WatchReport {
            account_pk: 42,
            username: None,
            is_self: true,
            followers: None,
            following: None,
            renamed: Vec::new(),
        };
        let tick = TickReport {
            report,
            requests: 1,
            lists: vec![TickList {
                kind: ListKind::Followers,
                skipped: Some(Skipped::Incomplete(StopReason::RateLimit)),
            }],
            committable: Vec::new(),
            at: 0,
            rename_cursor: None,
            renames_sent: Vec::new(),
        };

        assert!(!tick.looked());
        assert_eq!(
            tick.outcome(),
            ExitCode::from_stop_reason(StopReason::RateLimit)
        );
    }
}
