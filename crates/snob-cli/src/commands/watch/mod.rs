//! `snob watch`: the monitor.
//!
//! Two ways to ask the same question. `diff` answers out of storage and leaves
//! everything as it found it, so it can be run as often as anybody likes and
//! costs nothing. `once` goes and looks, reports, and remembers having
//! reported — which is the difference that matters, because what a run reports
//! it does not report again.
//!
//! The scheduled mode is what remains: it is `once` on a timer, and the report
//! it produces is the same one.
//!
//! Wording, dates and exit codes live here. What actually changed is
//! [`crate::engine::watch`]'s answer, and this never recomputes any of it.
//!
//! Getting a report to a receiver is [`delivery`], and the split is by question
//! rather than by layer: this module decides what a report says, that one
//! decides where it goes and what travels with it. It is a module of its own
//! because the rules in it are about one another and were six hundred lines
//! apart -- whether a stored credential may be attached, whether the address it
//! would go to can be posted to at all, and what a queued report may be handed
//! to on a later run are one rule asked three times.

use anyhow::{Context, Result, bail};
use snob_core::Pk;
use snob_core::model::{ListKind, User, printable};
use snob_core::paths::AppPaths;
use snob_core::secret::Secret;
use snob_core::secrets::{Kind, SecretStore, Stored};
use snob_core::store::{deliveries, watch::Queued};
use snob_core::watch::config::{self, WatchConfig, WebhookConfig};
use snob_core::watch::schedule::{self, Due, Schedule, Weekday};
use snob_core::watch::{Basis, Changes, ListDiff, Rename};
use url::Url;

use crate::cli::{
    WatchArgs, WatchCheckArgs, WatchCommand, WatchDiffArgs, WatchOnceArgs, WatchRunArgs,
    WebhookArgs,
};
use crate::commands::common::{self, Session};
use crate::engine::Provenance;
use crate::engine::watch::{ListReport, Skipped, TickReport, WatchReport, Watched};
use crate::exit::{ExitCode, ExitError};
use crate::report;
use crate::ui;
use crate::watch::webhook::{self, Attempt, Webhook, WebhookClient};

pub(super) mod delivery;
use delivery::{Delivery, deliver, delivery_from, drain, run_id};

pub async fn run(args: WatchArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    match args.command {
        Some(WatchCommand::Diff(args)) => diff(args, secrets, paths),
        Some(WatchCommand::Once(args)) => once(args, secrets, paths).await,
        Some(WatchCommand::Check(args)) => check(args, secrets, paths).await,
        Some(WatchCommand::Setup(args)) => super::watch_setup::setup(args, secrets, paths).await,
        Some(WatchCommand::Status(args)) => super::watch_setup::status(args, paths),
        None => scheduled(args.run, secrets, paths).await,
    }
}

/// Stays up and runs on a schedule until it is stopped.
///
/// The loop is deliberately thin. Everything with a rule in it — when the next
/// run is due, how many were missed, how far jitter may push one — is
/// `snob_core::watch::schedule`, which reads no clock and is tested with
/// literal timestamps. What is left here is sleeping and asking again, which
/// is the part no test can usefully drive.
async fn scheduled(args: WatchRunArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    // Before anything that can refuse to start. Reading the file, building the
    // schedule and checking the address can each end this process, and `settle`
    // is the only caller of `store::prune` — so a service that dies at startup
    // on a hand-edited file expires nothing for as long as nobody notices. This
    // is the mode the README leads with, and it had no settle at all.
    say_what_was_given_up(crate::engine::watch::settle_without_a_session(
        paths,
        snob_core::store::now(),
    ));

    // Read first, so the flags can override it. A flag beats the file because
    // somebody typing one is saying something about this run in particular.
    let configured = config::load(paths)?;

    let schedule = schedule_from(&args, configured.as_ref())?;
    let watched = watched_from(args.target.clone(), configured.as_ref());
    // Built once and reused, so a service that runs for months holds one
    // connection pool rather than building a TLS stack every few hours. Checked
    // here for the same reason the schedule is: a bad address should stop this
    // at the moment somebody is watching it start.
    let delivery = delivery_from(&args.delivery, configured.as_ref(), &secrets)?;

    // Refused here rather than at the first tick. A service that starts, waits
    // six hours and then exits because it was never allowed to read that
    // account is a service that looked healthy all afternoon.
    if let Some(unallowed) = watched.iter().find(|w| !w.may_run_unattended()) {
        return Err(refuse_unattended(unallowed.name().unwrap_or_default()));
    }

    ui::info(&format!(
        "Watching {}. {} Stop with Ctrl+C.",
        watching_label(&watched),
        describe_schedule(&when_from(&args, configured.as_ref()), &schedule, args.now),
    ));

    // Installed once for the process, which is what lets this open an `App` per
    // run without leaving a signal listener behind on each one.
    let cancel = crate::interrupt::install();

    let mut last_run = seed_for(paths, &schedule, snob_core::store::now())?;

    if args.now {
        // Run one, here, rather than by pretending nothing has ever run.
        //
        // `--now` was `last_run = None`, which means "due immediately" only for
        // an interval: with `--on`, `--at` or `--cron` the search simply
        // returned the next moment on the grid, and the banner said "Starting
        // with one now." before sleeping until nine o'clock tomorrow. It also
        // means the run happens with no jitter, which is right — jitter exists
        // so a *schedule* does not land on the same second every day, and
        // delaying a run somebody just asked for would only look broken.
        last_run = Some(snob_core::store::now());
        if let Err(e) = open_and_run(&args, &watched, delivery.as_ref(), &secrets, paths).await {
            report::print_error(&e);
        }
    }

    // The moment being waited for, the number of runs it stands for, and the
    // jitter roll that shifted it.
    //
    // Held across iterations rather than recomputed: `next_after` searches from
    // `max(floor, now)`, so once `now` has passed the grid minute it would
    // answer with the *next* one and the wake-up would creep forward instead of
    // arriving. Recomputed when the clock stops ticking and starts jumping —
    // see `CLOCK_JUMP_SECS`.
    let mut waiting_for: Option<(i64, u32)> = None;
    // The previous time round's clock reading, which is how a clock that jumped
    // is told from one that ticked.
    let mut clock_was: Option<i64> = None;

    loop {
        let now = snob_core::store::now();

        // A clock that moved by more than a nap did not tick: it was corrected,
        // or the machine was suspended. Either way the moment being waited for
        // was computed against a reading that no longer applies. A machine that
        // booted a year ahead and then had its clock corrected inwards parked
        // the monitor for the whole year — `due` clamps a stored future to the
        // present precisely so that cannot happen, and the cached moment was
        // what kept it from ever being asked.
        if let Some(before) = clock_was
            && !(0..=CLOCK_JUMP_SECS).contains(&(now - before))
        {
            waiting_for = None;
        }
        clock_was = Some(now);

        let (wake_at, missed) = match waiting_for {
            Some(pending) => pending,
            None => {
                let pending = match schedule::due(&schedule, last_run, now, &chrono::Local) {
                    // Already owed, so it goes now. Jitter is not applied to a
                    // run that is already late: it is off the grid by however
                    // late it is, and shifting it further would move the target
                    // every time round the loop, since the target would be
                    // computed from a `now` that keeps advancing.
                    Due::Now { missed } => (now, missed),
                    // Not a literal any more, and not an arm order. This was
                    // `Due::At(i64::MAX)` sitting above the general `At` arm, so
                    // the only thing between a schedule that names nothing and
                    // `wake_at` saturating was where these two arms happened to
                    // be written. `Due::Never` makes the compiler ask.
                    Due::Never => {
                        return Err(anyhow::anyhow!(
                            "this schedule can never come round: nothing matches it"
                        ));
                    }
                    // Rolled once per due moment. The roll is made here rather
                    // than inside `wake_at` so that function reads no randomness
                    // and its bounds stay testable.
                    //
                    // `wake_at` and not `with_jitter`: the jitter the banner
                    // printed is measured against the grid, in seconds-of-day,
                    // and a day a zone springs forward through is an hour
                    // shorter than that. `wake_at` asks the calendar in the zone
                    // from the moment actually due, so the roll cannot reach
                    // past the next moment or spill onto a day the calendar
                    // forbids. It only ever narrows, so the banner stays true.
                    Due::At(at) => (
                        schedule::wake_at(&schedule, at, fastrand::f64(), &chrono::Local),
                        0,
                    ),
                };
                waiting_for = Some(pending);
                pending
            }
        };

        if now >= wake_at {
            if missed > 0 {
                ui::warn(&missed_warning(missed));
            }
            // Recorded before the work, so a run that fails cannot turn into a
            // tight loop retrying it.
            last_run = Some(now);
            waiting_for = None;

            // A scheduled service does not exit because one run failed. A
            // cooldown lifts, a network comes back, and a session that is gone
            // gets reported every time until somebody fixes it — which is the
            // point of something that watches.
            if let Err(e) = open_and_run(&args, &watched, delivery.as_ref(), &secrets, paths).await
            {
                report::print_error(&e);
            }
            continue;
        }

        // Slept in bounded stretches and re-checked against the wall clock each
        // time round, rather than in one long sleep. A laptop that suspends for
        // eight hours makes a single long timer wrong by eight hours, and
        // asking "is it time yet?" costs nothing.
        let nap = (wake_at - now).clamp(1, 60) as u64;
        if cancel
            .sleep_or_cancel(std::time::Duration::from_secs(nap))
            .await
        {
            ui::info("Stopped.");
            return Ok(ExitCode::Interrupted);
        }
    }
}

/// The refusal a scheduled run raises for an account nobody confirmed.
///
/// Its own function so a test can read it. The name lands in two slots of one
/// sentence — the account being refused, and the command that answers for it —
/// and only the first of the two used to be filtered. It comes from
/// `watch.toml`, where nothing validates a username, so an escape sequence in
/// it reached the terminal and the journal through the second slot.
fn refuse_unattended(name: &str) -> anyhow::Error {
    let shown = printable(name);
    ExitError::new(
        ExitCode::Interrupted,
        format!(
            "reading @{shown}'s lists needs confirmation, and a scheduled run has nobody to \
             ask.\nRun \"snob watch setup\" to answer it once, or \"snob watch once {shown}\" \
             while you are here."
        ),
    )
    .into()
}

/// What the monitor says about the runs it was not running for.
///
/// **One is the ordinary case, and it was the ungrammatical one.** This was a
/// single format string with `1` an ordinary value in it: "1 scheduled runs were
/// missed while this was not running. They are reported as one" — which
/// contradicts itself in the case it prints most often. A machine off for a day
/// on a daily schedule misses exactly one, and `missed_since` takes one off the
/// end because the moment being served is itself in the past, so one is what a
/// laptop that was shut overnight produces.
///
/// Its own function so a test can read it, like [`refuse_unattended`]. The
/// sentence is only ever printed from inside the loop, which no test can drive.
fn missed_warning(missed: u32) -> String {
    if missed == 1 {
        "1 scheduled run was missed while this was not running. It is not replayed: there is \
         only one present state, so there is nothing to catch up on"
            .to_string()
    } else {
        format!(
            "{missed} scheduled runs were missed while this was not running. They are reported \
             as one: there is only one present state, so there is nothing to catch up on"
        )
    }
}

/// How far the clock may move between two turns of the loop and still be
/// ticking.
///
/// A nap is at most sixty seconds, so anything past two minutes is a
/// correction, a suspend, or a resume — none of which the moment being waited
/// for was computed against.
const CLOCK_JUMP_SECS: i64 = 120;

/// When the monitor last started a run, for any account.
///
/// Opened and closed here rather than held: the loop deliberately keeps no
/// SQLite connection while it sleeps, so `snob purge` in another terminal is not
/// blocked by a file this has open.
fn last_started(paths: &AppPaths) -> Result<Option<i64>> {
    let store = snob_core::store::Store::open(paths)?;
    Ok(snob_core::store::watch::last_started(store.conn())?)
}

/// What the loop starts its clock from.
///
/// Seeded from the run log rather than from this process's start. The clock used
/// to begin again on every start, so `--every 24h` on a machine that is powered
/// on from eight to six, or under a supervisor with `Restart=always` restarting
/// more often than the interval, never reached its first run at all — while
/// `snob watch status`, reading the very same table, said "It has not run yet."
///
/// With nothing in the log the answer depends on what kind of schedule it is,
/// and that is the whole reason this is a function rather than an `or_else`:
///
/// - **An interval** has no moments of its own, so `None` means "due now" and a
///   fresh install would walk the second it was set up. `Some(now)` is what
///   makes it wait one interval, and `--now` is how somebody asks for the walk
///   at start.
/// - **A calendar** has its own moments, and inventing a run for it is harmful.
///   The invented value is a claim that a run happened this second, and
///   `next_after` believes it: the floor becomes `now + MIN_GAP_SECS`, stepping
///   over every moment in the next quarter of an hour. `snob watch --at 09:00`
///   started at 08:50 printed "Running at 09:00." and then slept for a day and
///   ten minutes. `None` is what the schedule module's own contract asks for
///   here — there is no past to wait from, and no run to be too close to.
fn seed_last_run(recorded: Option<i64>, schedule: &Schedule, now: i64) -> Option<i64> {
    recorded.or_else(|| (!schedule.is_on_a_calendar()).then_some(now))
}

/// The same answer, remembered across process starts.
///
/// [`seed_last_run`] is right and it was never written down. The invented `now`
/// lived in a local variable, and `last_started` reads `watch_runs`, whose only
/// writer is a tick that finished — so while that log stayed empty, **every
/// start measured the interval from that start**. The doc above says this class
/// of failure is fixed; it was fixed only from the second run onwards, and the
/// second run is the one that never came.
///
/// `snob watch` in a login item with `--every 1d` — the wizard's own suggestion
/// — on a laptop up eight hours a day is due at hour twenty-four and shut down
/// at hour eight, every day, for ever. `--every 2w`, another of the wizard's
/// three examples, needs a fortnight of unbroken uptime, and `Restart=always`
/// after one crash puts it back to zero. `status` says "it has not run yet" the
/// whole time, which is exactly true and reads as a tool that is new rather
/// than one that is stuck.
///
/// A calendar seeds nothing, for the reason [`seed_last_run`] gives: inventing
/// a run for it steps over every moment in the next quarter of an hour.
fn seed_for(paths: &AppPaths, schedule: &Schedule, now: i64) -> Result<Option<i64>> {
    if let Some(recorded) = last_started(paths)? {
        return Ok(Some(recorded));
    }
    let Some(invented) = seed_last_run(None, schedule, now) else {
        return Ok(None);
    };

    let store = snob_core::store::Store::open(paths)?;
    if let Some(seeded) = snob_core::store::watch::interval_seeded_at(store.conn())? {
        return Ok(Some(seeded));
    }
    snob_core::store::watch::set_interval_seeded_at(store.conn(), invented)?;
    Ok(Some(invented))
}

/// One run inside the loop: open, tick, print, close.
///
/// A fresh `App` per run rather than one held open for weeks. It picks up a
/// session that was replaced and a refreshed User-Agent, and — the reason that
/// matters on Windows — it holds no SQLite connection while the loop sleeps, so
/// `snob purge` in another terminal is not blocked by a file this has open.
async fn open_and_run(
    args: &WatchRunArgs,
    watched: &[Watched],
    delivery: Option<&Delivery>,
    secrets: &SecretStore,
    paths: &AppPaths,
) -> Result<()> {
    let Session::Open(mut app) = common::open_with_progress(!args.no_progress, secrets, paths)?
    else {
        // Said by `common::open`, and there is nothing this run can do about it
        // — but the loop keeps going, because a session restored later should
        // be picked up without the service having to be restarted.
        //
        // Settled on the way out. This is a whole run, and a service whose
        // session was logged out reaches this door on every one of them: with
        // nothing here, a machine that ran for a week with no session expired
        // nothing at all — not the owed reports, not old captures, not the run
        // log.
        say_what_was_given_up(crate::engine::watch::settle_without_a_session(
            paths,
            snob_core::store::now(),
        ));
        return Ok(());
    };
    // The monitor takes an answer in advance from `watch.toml`, never from a
    // flag, so the refusal when nobody is at a terminal has to say so.
    app.consent_comes_from_the_config();

    run_one(args, &mut app, watched, delivery).await
}

/// One run, over an `App` somebody else opened.
///
/// Split from the opening so a test can hand it one: opening a session wants a
/// keyring, and this is where "the queue is drained once per run, after every
/// account" lives -- AGENTS.md's rule, which regressed once and which nothing
/// could reach to check. Deleting the drain from here left the whole suite
/// green.
async fn run_one(
    args: &WatchRunArgs,
    app: &mut crate::app::App,
    watched: &[Watched],
    delivery: Option<&Delivery>,
) -> Result<()> {
    let outcome = run_accounts(app, watched, delivery, Printing::unattended(args.json)).await;
    match outcome.failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// How much a run says out loud about a tick.
///
/// The two modes differ here and nowhere else. A scheduled service writes one
/// line per run down a pipe and stays quiet when there is no news, which is
/// what makes `snob watch >> events.ndjson` and "silence means nothing changed"
/// both true. `once` is looked at while it runs, so it says so either way and
/// lays its JSON out to be read rather than appended to.
#[derive(Clone, Copy)]
struct Printing {
    json: bool,
    watching: bool,
}

impl Printing {
    fn unattended(json: bool) -> Self {
        Self {
            json,
            watching: false,
        }
    }

    fn watched(json: bool) -> Self {
        Self {
            json,
            watching: true,
        }
    }
}

/// What a run over several accounts came to.
struct RunOutcome {
    /// Requests spent by every account that got as far as spending any.
    spent: u32,
    /// The first non-`Ok` verdict a tick reported, the accounts being in the
    /// order the file names them. [`first_reason`] is where the choice between
    /// first and worst is made, and why there is no worst to choose.
    code: ExitCode,
    /// The last failure, unprinted. Every earlier one has already been printed,
    /// because only one can be handed back and the caller prints the one it
    /// gets — `scheduled` so the service keeps running, `main` so `once` exits
    /// with the right code. Printing here *and* returning is what wrote the
    /// whole `error:` / `caused by:` / `hint:` block twice per failing run.
    failed: Option<anyhow::Error>,
}

/// Every account of one run, and the three things that happen once around them.
///
/// One `App` for all of them, unlike one per run: they share a session and a
/// request budget, and opening a second would be a second connection to the
/// same database for no reason. A failure on one account does not stop the
/// rest — a private account somebody stopped being allowed to read must not
/// silence the monitor's own.
///
/// This was written twice, seven steps each, and the copies had drifted: `once`
/// took the print-when-somebody-else-will-not half and not the guard on the
/// return, and it put the drain and the expiry behind a `?`, so an ordinary
/// failure took both with it. AGENTS.md names both modes in the rule that the
/// queue is drained once per run, after every account, and only one of them was
/// reachable by a test.
async fn run_accounts(
    app: &mut crate::app::App,
    watched: &[Watched],
    delivery: Option<&Delivery>,
    printing: Printing,
) -> RunOutcome {
    let mut spent = 0;
    let mut code = ExitCode::Ok;
    let mut failures = Vec::new();

    for account in watched {
        // Ctrl+C stops the run rather than only the account it landed in.
        // Without this the loop walked every remaining account after the user
        // had asked it to stop.
        if app.cancel().is_canceled() {
            break;
        }
        // Read around the tick rather than out of the report it returns.
        //
        // `TickReport.requests` is worked out at the *end* of `tick`, so any
        // `?` on the way out throws away a measurement the pacer has already
        // been charged for — and `tick_one` also `?`s on serializing the body
        // and on `deliver`, both of them after a whole successful two-list
        // walk. `snob watch once friend` where @friend has gone private spends
        // one `web_profile_info` and then prints "0 requests", under a comment
        // saying a run that stopped halfway still spent some. What was really
        // spent has its own row in AGENTS.md.
        let before = app.client().pacer().spent();
        let outcome = tick_one(app, account, delivery, printing).await;
        let charged = app.client().pacer().spent().saturating_sub(before);
        spent += charged;

        match outcome {
            Ok(tick) => code = first_reason(code, tick.outcome()),
            Err(e) => {
                // A tick that failed is still a tick that happened.
                //
                // `record_run` is reached only through `commit`, which is the
                // last statement of a *successful* `tick_one`, so **any** `Err`
                // out of `tick` wrote no row at all — not only an unresolvable
                // name, but a mid-walk cooldown, a failed `history_head` read,
                // a `compare` that could not run. @friend goes private and
                // `target::resolve` bails; the same for a rename, a delete, and
                // for any named account a DNS outage, which fails in `resolve`
                // before the poll-failure handler that saves the own-account
                // case.
                //
                // So `status` went on printing "@friend last ran on <the last
                // good date> and found nothing", `health` saw a stale `ok`, and
                // `prune` keeps that newest row for ever so it never aged into
                // "it has not run yet". The schema comment says "One row per
                // tick, including the ticks that found nothing".
                //
                // An account whose *first* tick fails still records nothing:
                // `watch_runs.account_pk` references `accounts(pk)`, and
                // nothing has put a row there yet. That is the account a new
                // user would most want a probe to fail for, and it is written
                // down here rather than left to be rediscovered.
                //
                // The moment is read once and given to both, so the row in the
                // run log and the line in the stream name the same second and a
                // reader can put them side by side.
                let at = snob_core::store::now();
                record_failed_run(app, account, &e, charged, at);
                if printing.json {
                    // The stream gets a line for this interval too. Written
                    // after the `?` in `tick_one`, there was none: the failure
                    // went to standard error as prose and `events.ndjson` had
                    // nothing at all for that run.
                    println!("{}", json_line(&failed_tick_json(account, &e, at, charged)));
                }
                failures.push(e);
            }
        }
    }

    // Once, after every account, and whatever the accounts did. Whatever is
    // owed from earlier runs goes out here — including when no account had news
    // of its own, which is the common case and the one that used to leave the
    // queue untouched. A machine whose hourly run failed all day left rows
    // ageing past `MAX_AGE_SECS`, where `due` no longer returns them and
    // `failed` — the only thing that expires one — is never reached, while
    // `status` went on promising the next run would try them.
    //
    // Not after a cancellation, though: the queue is `DRAIN_LIMIT` POSTs of up
    // to thirty seconds each, so draining it there was up to five more minutes
    // of a run the user had already stopped. Nothing is lost by leaving it —
    // what is owed stays owed, and the next run drains it, which is the whole
    // point of the queue.
    if let Some(delivery) = delivery
        && !app.cancel().is_canceled()
    {
        drain(app, delivery).await;
    }
    say_what_was_given_up(crate::engine::watch::settle(
        app.db(),
        snob_core::store::now(),
    ));

    let (print_here, failed) = to_print_and_to_return(failures);
    for earlier in print_here {
        report::print_error(&earlier);
    }

    RunOutcome {
        spent,
        code,
        failed,
    }
}

/// Writes the row a failed tick owes the run log.
///
/// Best-effort, like `commit`'s own recording: a run log that cannot be written
/// is worth a line in the journal and is not a reason to turn a failure into a
/// different failure.
///
/// The account has to be resolved locally, because a tick that failed at
/// `target::resolve` never learned an id. An account that has never been seen
/// has no `accounts` row for the foreign key to point at, so nothing is written
/// — which is exactly the case the comment at the call site names.
fn record_failed_run(
    app: &crate::app::App,
    watched: &Watched,
    error: &anyhow::Error,
    requests: u32,
    at: i64,
) {
    let pk = match watched.name() {
        Some(name) => snob_core::store::accounts::find_pk_by_username(
            app.db().conn(),
            snob_core::model::printable(name).trim(),
        )
        .ok()
        .flatten(),
        None => Some(app.viewer().pk),
    };
    let Some(account_pk) = pk else {
        return;
    };

    let outcome = ExitCode::from_chain(error).unwrap_or(ExitCode::Error);
    let record = snob_core::store::watch::record_run(
        app.db().conn(),
        &snob_core::store::watch::Run {
            account_pk,
            started_at: at,
            finished_at: Some(at),
            requests,
            outcome: Some(outcome.as_str().to_string()),
            changes: 0,
        },
    );
    if let Err(e) = record {
        tracing::warn!(error = %e, "the failed run could not be recorded");
    }
}

/// Splits a run's failures into the ones it prints and the one it hands back.
///
/// Only one can be returned, and whoever gets it prints it — `scheduled` so the
/// service keeps going, `main` so `once` exits with the right code. So the rest
/// are printed here, in the order they happened, and each failure reaches the
/// journal exactly once.
///
/// This is the line the defect lived on. Printing every failure here *and*
/// returning one wrote the whole `error:` / `caused by:` / `hint:` block twice
/// per failing run — and the guard that avoided it, `if watched.len() > 1`,
/// bought that by returning `Ok` from a run in which an account had failed.
fn to_print_and_to_return(
    failures: Vec<anyhow::Error>,
) -> (Vec<anyhow::Error>, Option<anyhow::Error>) {
    let mut failures = failures.into_iter();
    let last = failures.next_back();
    (failures.collect(), last)
}

/// The verdict a run answers with, folded over the ticks that succeeded.
///
/// **The first non-`Ok` one, not the worst.** `ExitCode` is a handful of
/// independent reasons rather than a severity scale: it derives no `Ord`, and
/// its numbers are a shell convention — 130 is 128 plus SIGINT — so "the worst"
/// is not something this could compute. The field's doc said "worst" anyway,
/// which invites the fix that derives `Ord` and takes the maximum, and that
/// ordering would put `Interrupted` above `NoSession`: a run somebody stopped
/// would outrank a session that has gone, in the value `once` hands back as its
/// process exit code.
///
/// So the rule is the order the accounts are already in, and the earliest reason
/// wins. A tick that failed outright is not here at all — it leaves through
/// `RunOutcome::failed`, and `once` returns this only when nothing failed.
fn first_reason(so_far: ExitCode, tick: ExitCode) -> ExitCode {
    match so_far {
        ExitCode::Ok => tick,
        earlier => earlier,
    }
}

/// One account, inside a run that may cover several.
async fn tick_one(
    app: &mut crate::app::App,
    watched: &Watched,
    delivery: Option<&Delivery>,
    printing: Printing,
) -> Result<TickReport> {
    let tick = crate::engine::watch::tick(app, watched).await;
    app.progress().finish();
    let tick = tick?;

    if printing.json {
        println!("{}", json_line(&tick_json(&tick)));
    } else if printing.watching || !tick.report.changes().is_empty() {
        for line in describe(&tick.report, tick.lists.iter().any(|l| l.skipped.is_some())) {
            println!("{line}");
        }
    }

    warn_about_refusals(&tick);

    deliver(app, &tick, delivery).await?;
    Ok(tick)
}

/// Says out loud which lists this run could not look at, and why.
///
/// To standard error, so it does not land in the middle of a report something
/// else is parsing — and said even in JSON, where a caller reading `looked`
/// would otherwise have to guess why. Both callers had a verbatim copy of the
/// loop, one destructured and one indexing the pair.
fn warn_about_refusals(tick: &TickReport) {
    for (kind, skipped) in tick
        .lists
        .iter()
        .filter_map(|l| l.skipped.map(|s| (l.kind, s)))
    {
        ui::warn(&refusal_line(kind, skipped));
    }
}

/// Which account a scheduled run watches, and whether it may.
///
/// A name on the command line is checked against the file, because that is the
/// only place a consent can have been recorded — and an unattended run that
/// could be pointed at a stranger by an argument would make the recording
/// pointless.
pub(super) fn watched_from(
    target: Option<String>,
    configured: Option<&WatchConfig>,
) -> Vec<Watched> {
    if let Some(name) = target {
        return vec![with_recorded_consent(&name, configured)];
    }

    let listed: Vec<Watched> = configured
        .into_iter()
        .flat_map(|c| c.accounts.iter())
        .map(|account| {
            if account.is_own() {
                Watched::own()
            } else {
                with_recorded_consent(&account.target, configured)
            }
        })
        .collect();

    // A file with no `[[account]]` at all means the obvious thing rather than
    // nothing: somebody who configured a schedule and a webhook and never
    // mentioned an account meant their own.
    if listed.is_empty() {
        vec![Watched::own()]
    } else {
        listed
    }
}

/// One account, with whatever answer is on record for it.
///
/// The file is the only place a consent can have come from, which is what makes
/// an unattended run safe: an argument cannot grant one.
///
/// **The at sign comes off here, on both sides**, which makes this the boundary
/// a `Watched` is built at and every reader downstream of it. `Watched::target`
/// used to carry the string exactly as typed or configured while every other
/// reader in the tool cleaned — `target::resolve`, `target::from_store`,
/// `target::label` — and the two that did not are both load-bearing. Typing
/// `snob watch "@friend"` against a file recording an answer for `friend`
/// matched nothing, so a correctly consented monitor refused at startup citing
/// a consent written in the file it had just read, and `refuse_unattended`
/// printed the doubled `@@friend` that gives it away. A hand-edited `target = "@friend"`
/// went the other way and reached `engine::check`, which asked Instagram for
/// `username=@friend` and reported a working configuration as broken. README.md
/// promises without qualification that a username may be written either way, so
/// this is that promise being kept rather than a special case.
fn with_recorded_consent(name: &str, configured: Option<&WatchConfig>) -> Watched {
    let name = crate::engine::target::clean(name);
    let recorded = configured
        .into_iter()
        .flat_map(|c| c.accounts.iter())
        .find(|account| {
            !account.is_own()
                && crate::engine::target::clean(&account.target).eq_ignore_ascii_case(name)
        })
        .and_then(|account| account.consent);

    if recorded.is_some() {
        // The table is what matters, not what is in it. `[account.consent]`
        // exists because somebody was asked; its `agreed_at` is the record of
        // when, kept in the file, and nothing downstream of here reads it or
        // could tell a real moment from whatever a hand-edit wrote.
        Watched::consented(name.to_string(), crate::engine::watch::Consent)
    } else {
        Watched::asking(name.to_string())
    }
}

/// What the opening line names, so somebody starting the service can see that
/// it understood which accounts it is for.
fn watching_label(watched: &[Watched]) -> String {
    let names: Vec<String> = watched
        .iter()
        .map(|w| crate::app::target_label(w.name()))
        .collect();

    match names.len() {
        0 => "nothing".to_string(),
        1 => names[0].clone(),
        _ => {
            let (last, rest) = names.split_last().expect("more than one");
            format!("{} and {last}", rest.join(", "))
        }
    }
}

/// The schedule as written, before it is built: whichever of the two sources
/// won, in the words it was given in.
///
/// The flags win as a set rather than field by field, because a schedule half
/// from the file and half from the command line is one nobody can read.
#[derive(Debug, Default, PartialEq, Eq)]
struct When {
    cron: Option<String>,
    at: Vec<String>,
    on: Vec<String>,
    every: Option<std::time::Duration>,
    jitter: Option<std::time::Duration>,
}

/// Resolves which source the schedule comes from.
///
/// Asked by [`schedule_from`] and by the line printed at startup, so the
/// sentence on screen cannot describe a different schedule from the one that
/// runs.
///
/// **It chooses a source; it does not copy fields.** The five fields were
/// spelled three times here and copied out by hand in two arms, and the jitter
/// fell through that copy in *both* directions before anybody noticed — then it
/// was repaired by special-casing the one field that had been forgotten rather
/// than by taking the copy out. Which leaves the next field added to
/// `watch.toml` dropped exactly the same way: silently, with the banner
/// announcing whatever the default happened to be. The two `From` impls are the
/// only place the fields are named, so a field added to either source arrives
/// here by being a field.
fn when_from(args: &WatchRunArgs, configured: Option<&WatchConfig>) -> When {
    let given =
        args.cron.is_some() || !args.at.is_empty() || !args.on.is_empty() || args.every.is_some();

    // The schedule halves come as a set, so a flag does not merge into a
    // configured calendar — but the jitter is not one of those halves, and it
    // was dropped with them in **both** directions.
    //
    // `given` looks only at `cron`/`at`/`on`/`every`, so with the schedule in
    // the file `snob watch --jitter 0` threw the flag away; and the early return
    // below carries `args.jitter` alone, so a file's `jitter = "0"` was thrown
    // away the moment any schedule flag was typed. Both are documented as
    // turning jitter off, unqualified, and each direction leaves the banner
    // announcing a value nobody chose.
    //
    // Worked out once, over both sources, because it does not depend on which of
    // them won the halves — and applied after the choice rather than inside it,
    // so it cannot be forgotten in one arm and not the other, which is how it
    // went wrong the first time.
    let jitter = args.jitter.or_else(|| configured.and_then(|c| c.jitter));

    let mut when = match (given, configured) {
        (true, _) => When::from(args),
        (false, Some(config)) => When::from(config),
        (false, None) => When::default(),
    };
    when.jitter = jitter;
    when
}

/// The schedule as the command line wrote it.
impl From<&WatchRunArgs> for When {
    fn from(args: &WatchRunArgs) -> Self {
        Self {
            cron: args.cron.clone(),
            at: args.at.clone(),
            on: args.on.clone(),
            every: args.every,
            jitter: args.jitter,
        }
    }
}

/// The schedule as the file wrote it.
impl From<&WatchConfig> for When {
    fn from(config: &WatchConfig) -> Self {
        Self {
            cron: config.cron.clone(),
            at: config.at.clone(),
            on: config.on.clone(),
            every: config.every,
            jitter: config.jitter,
        }
    }
}

/// The two halves of a calendar, parsed and checked together.
///
/// This was written out twice: the wizard validating what it is about to write,
/// and a run validating what it read back. The same two maps, the same
/// `Schedule::calendar`, down to the sentence that says what a day looks like —
/// two halves of one contract, and a contract with two implementations is one
/// that can be half-changed.
pub(crate) fn calendar_from<S: AsRef<str>>(days: &[S], times: &[S]) -> Result<Schedule> {
    let days = days_named(days)?;
    let times = times
        .iter()
        .map(|time| schedule::parse_time(time.as_ref()).map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()?;
    Ok(Schedule::calendar(&days, &times)?)
}

/// The `--on` half, parsed, for both of the schedules that have one.
///
/// Shared for the reason the function above is: `--on tues` must not mean one
/// thing to a calendar and another to a days-only schedule, and the sentence
/// that says what a day looks like is part of that contract.
fn days_named<S: AsRef<str>>(days: &[S]) -> Result<Vec<Weekday>> {
    days.iter()
        .map(|day| {
            let day = day.as_ref();
            Weekday::parse(day)
                .ok_or_else(|| anyhow::anyhow!("\"{day}\" is not a day (try mon, thu)"))
        })
        .collect()
}

/// Days with no time of day, which is a schedule only because an interval says
/// how often. [`snob_core::watch::schedule::Schedule::days`] says why the
/// interval cannot be added afterwards.
fn days_from<S: AsRef<str>>(days: &[S], every: std::time::Duration) -> Result<Schedule> {
    Ok(Schedule::days(&days_named(days)?, every)?)
}

/// Builds the schedule from the flags, or the file, or explains what is
/// missing.
///
/// **A flag replaces the schedule rather than merging with it.** Half from the
/// file and half from the command line is a schedule nobody can read back: the
/// only honest reading of `--every 6h` against a configured `--on mon` is the
/// one the person typing meant, and there is no way to know which.
pub(super) fn schedule_from(
    args: &WatchRunArgs,
    configured: Option<&WatchConfig>,
) -> Result<Schedule> {
    let When {
        cron,
        at,
        on,
        every,
        jitter,
    } = when_from(args, configured);

    let mut schedule = if let Some(expression) = &cron {
        Schedule::cron(expression)?
    } else if at.is_empty()
        && !on.is_empty()
        && let Some(every) = every
    {
        // Days and an interval and no time of day. This branch has to come
        // before the calendar one, which is where it used to land: a calendar is
        // a set of minutes, days-only names none, and it was refused with "a
        // calendar needs a time of day" without `every` being looked at — the
        // one shape README.md, CHANGELOG.md and AGENTS.md all advertise.
        days_from(&on, every)?
    } else if !at.is_empty() || !on.is_empty() {
        calendar_from(&on, &at)?
    } else if let Some(every) = every {
        Schedule::every(every)?
    } else {
        return Err(anyhow::anyhow!(
            "how often should this run?\n\
             Run \"snob watch setup\" once, or say it here: \"--every 6h\", \
             \"--on mon,thu --at 09:00\", or \"--cron '0 9 * * 1,4'\"."
        ));
    };

    // An interval alongside a calendar is a floor on it, not a second schedule.
    // `--every 2w --on mon --at 09:00` is "one Monday in every two weeks".
    //
    // Not for the days-only branch above: `Schedule::days` has already set the
    // interval, because a grid naming every minute of the day does not survive
    // `validated` without one.
    if let Some(every) = every
        && (cron.is_some() || !at.is_empty())
    {
        schedule = schedule.and_every(every)?;
    }
    if let Some(jitter) = jitter {
        schedule = schedule.with_jitter(jitter);
    }
    Ok(schedule)
}

/// The opening line, so somebody starting this can see it understood them.
///
/// It names the schedule, which is what its own doc used to promise while both
/// branches read nothing but the jitter — so a monitor about to sit for a
/// fortnight opened with a sentence about seconds.
fn describe_schedule(when: &When, schedule: &Schedule, now: bool) -> String {
    let parts = report::schedule_clauses(when.every, &when.on, &when.at, when.cron.as_deref());

    let mut line = format!("Running {}.", parts.join(", "));

    // `Schedule::jitter()` rather than what was typed: this is the announcement
    // of what is about to happen, and the schedule has already clamped the
    // spread to what its calendar can absorb.
    if let Some(sentence) = report::jitter_sentence(schedule.jitter()) {
        line.push(' ');
        line.push_str(&sentence);
    }
    if now {
        line.push_str(" Starting with one now.");
    }
    line
}

/// One run of the monitor: look, report, and remember having reported.
///
/// **The scheduled run without the loop**, which is what the README's "one run,
/// for cron or a systemd timer" describes and what `once` already was for the
/// webhook half. It was not for the accounts: it read `watch.toml` for the
/// address and then built the watched set from the command line alone, so
/// somebody who ran `watch setup`, added @friend and put this on a timer never
/// had @friend walked, marked or reported — and typing the name instead failed
/// every unattended run, because `Watched::asking` discards the recorded
/// consent that is the only thing such a run accepts.
async fn once(args: WatchOnceArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    // Settled before anything can refuse. This mode had the settle below and the
    // scheduled one had none, and the two doors above them closed first in both:
    // a session that has gone leaves owed reports ageing past `MAX_AGE_SECS`,
    // where `due` no longer returns them and `failed` — the only thing that
    // expires one — is never reached, while `status` goes on promising the next
    // run will try them. A webhook address `webhook::check` refuses does the
    // same thing one line earlier. One call, above both, so the pair cannot
    // drift a third time.
    say_what_was_given_up(crate::engine::watch::settle_without_a_session(
        paths,
        snob_core::store::now(),
    ));

    // Before the session is opened and long before a request is spent, so a
    // webhook address that could never work costs nothing to find out about.
    // The file is read here too: `once` on a timer should need no more
    // arguments than the scheduled mode does.
    let configured = config::load(paths)?;
    let delivery = delivery_from(&args.delivery, configured.as_ref(), &secrets)?;

    let Session::Open(mut app) = common::open_with_progress(!args.no_progress, &secrets, paths)?
    else {
        // Already settled, at the top: this run is the one that most needs it,
        // and it is not the only door that closes before `run_accounts`.
        return Ok(ExitCode::NoSession);
    };
    // The monitor takes an answer in advance from `watch.toml`, never from a
    // flag, so the refusal when nobody is at a terminal has to say so.
    app.consent_comes_from_the_config();

    let watched = watched_from(args.target.clone(), configured.as_ref());

    let outcome = run_accounts(
        &mut app,
        &watched,
        delivery.as_ref(),
        Printing::watched(args.json),
    )
    .await;

    // Said whether or not an account failed: a run that stopped halfway still
    // spent requests, and this is the mode somebody is watching.
    ui::info(&format!(
        "{} - {}",
        report::stored_on(snob_core::store::now()),
        report::requests(outcome.spent)
    ));

    if let Some(e) = outcome.failed {
        return Err(e);
    }

    // The run's own verdict, which was computed, written into `watch_runs`, and
    // then thrown away in favor of a literal zero. The exit codes exist so a
    // caller on a timer can tell "wait" from "log in again" without parsing
    // text, and `once` is the mode that is put on a timer.
    Ok(outcome.code)
}

/// Checks the configuration would work, before it runs unattended.
///
/// Everything `setup` writes down is a claim about a machine, a session and
/// somebody else's server, and every one of them used to be tested for the
/// first time by an unattended run at three in the morning. This puts the same
/// questions while there is still somebody to answer them.
///
/// **It writes nothing and walks no list.** `engine::check` takes `&App`, so it
/// cannot reach the `&mut Store` that recording needs — the guard `watch diff`
/// already rests on — and the cost is one request for the session plus one per
/// configured account. That is what makes it safe to point a monitoring system
/// at.
///
/// It degrades rather than stopping. A missing session does not hide a broken
/// schedule, and a broken schedule does not hide a webhook that has stopped
/// answering: every line is reported, and the exit code is the worst of them.
async fn check(args: WatchCheckArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    let report = preflight(&args, &secrets, paths).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&check_json(&report))?);
    } else {
        for line in describe_check(&report) {
            println!("{line}");
        }
    }

    Ok(report.verdict().exit_code())
}

/// The checks themselves, without the printing, so `setup` can run them too.
pub(super) async fn preflight(
    args: &WatchCheckArgs,
    secrets: &SecretStore,
    paths: &AppPaths,
) -> Result<crate::engine::check::CheckReport> {
    use crate::engine::check::{self, Verdict};

    let configured = config::load(paths)?;
    let now = snob_core::store::now();

    // Built the same way a run builds it, or this would be checking a schedule
    // nobody is on. A configuration with none at all is not an error here — it
    // is one of the things worth reporting.
    //
    // **The error is kept.** This used to be `.ok()`, and `without_a_session`
    // pushed a schedule line only on `Some`, so a file whose schedule the
    // scheduler refuses produced no schedule line at all — and
    // `CheckReport::verdict()` is `max().unwrap_or(Ok)`, so `check` exited 0
    // about a monitor that dies at `schedule_from` on every single invocation.
    // The `NotConfigured` warning does not cover it either, because a file
    // exists. `schedule_of`'s own doc says it "is what catches a file the
    // scheduler would refuse at every run"; it was never reached for that case.
    // Every shape gets here through a hand-edit, which the first line of
    // `watch.toml` says is fine: `every = "5m"`, `cron = "0 9 * *"`,
    // `at = ["25:00"]`, `every` with `on` and no `at`. All pass `config::parse`,
    // which reads TOML, the schema number and one key clash, and nothing else.
    let schedule = configured
        .as_ref()
        .map(|c| schedule_from(&WatchRunArgs::default(), Some(c)).map_err(|e| e.to_string()));
    let mut report = check::without_a_session(configured.as_ref(), schedule.as_ref(), now);

    // Before the session, like `once` does, so an address that could never work
    // is reported even on a machine that cannot log in.
    let delivery = match delivery_from(&WebhookArgs::default(), configured.as_ref(), secrets) {
        Ok(delivery) => delivery,
        Err(e) => {
            report.checked.push(check::Checked {
                what: check::What::Webhook {
                    destination: String::new(),
                    status: None,
                    signed: false,
                },
                verdict: Verdict::Failed,
                problem: Some(e.to_string()),
            });
            None
        }
    };

    match common::open_with_progress(false, secrets, paths)? {
        Session::Open(app) => {
            let watched = watched_from(None, configured.as_ref());
            check::with_a_session(&app, secrets, &watched, &mut report).await;
        }
        Session::Missing => report.checked.push(check::Checked {
            what: check::What::Session {
                viewer: None,
                backend: secrets.backend().as_str(),
            },
            verdict: Verdict::Failed,
            problem: Some("no session is stored; run \"snob login\"".to_string()),
        }),
    }

    if let Some(line) = not_posted(
        delivery
            .as_ref()
            .map(|d| (d.destination.as_str(), d.signed)),
        args.no_webhook,
    ) {
        // Not posted is not the same as nowhere to post, and with no line at
        // all the two were the same report.
        report.checked.push(line);
    } else if let Some(delivery) = delivery.as_ref() {
        let id = run_id(now, 0);
        let body = serde_json::to_string(&preflight_body(&id, now))?;
        report.checked.push(
            check::webhook_of(
                &delivery.client,
                delivery.destination.clone(),
                delivery.signed,
                &id,
                &body,
            )
            .await,
        );
    }

    Ok(report)
}

/// The webhook line for a run that is deliberately not posting one.
///
/// `None` when there is nothing to say — no webhook configured at all, or a run
/// that is about to post and will report what came back.
///
/// With `--no-webhook` nothing was pushed at all, so `check --json` had no
/// webhook object and the terminal no webhook line: byte for byte the report a
/// machine with no `[webhook]` produces. That is the one flag somebody reaches
/// for to check everything else without disturbing a receiver, and it made "is a
/// receiver configured?" unanswerable from the probe's own output. The
/// validation that did happen went with it — by the time this is reached
/// `delivery_from` has parsed the address and put every configured header
/// through `webhook::check`, and a failure there is already its own `Failed`
/// line, so what is left is a check that passed and was thrown away.
///
/// The flag is an argument rather than a condition at the call site, because the
/// defect is the decision not to push a line: a helper that only built one would
/// leave the revert green.
fn not_posted(
    webhook: Option<(&str, bool)>,
    no_webhook: bool,
) -> Option<crate::engine::check::Checked> {
    use crate::engine::check::{Checked, Verdict, What};

    let (destination, signed) = webhook.filter(|_| no_webhook)?;
    Some(Checked {
        what: What::Webhook {
            destination: destination.to_string(),
            status: None,
            signed,
        },
        verdict: Verdict::Ok,
        problem: Some(
            "--no-webhook, so nothing was posted; the address and the headers were still \
             checked"
                .to_string(),
        ),
    })
}

/// Walks the configured accounts once, so there is something to compare
/// against.
///
/// One ordinary run with no webhook: a baseline has nothing to report, so there
/// is nothing to send, and going through the same path as every other run is
/// what stops this being a second way of laying one down.
pub(super) async fn baseline_now(
    configured: Option<&WatchConfig>,
    secrets: &SecretStore,
    paths: &AppPaths,
) -> Result<()> {
    let watched = watched_from(None, configured);
    open_and_run(&WatchRunArgs::default(), &watched, None, secrets, paths).await
}

/// One line per check, in the order they were made.
pub(super) fn describe_check(report: &crate::engine::check::CheckReport) -> Vec<String> {
    use crate::engine::check::{Verdict, What};

    let mut lines = Vec::new();
    for checked in &report.checked {
        let (label, detail) = match &checked.what {
            What::NotConfigured => ("config".to_string(), String::new()),
            What::Schedule { next } => (
                "schedule".to_string(),
                next.iter()
                    .map(|at| report::stored_on(*at))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            What::Session { viewer, backend } => (
                "session".to_string(),
                match viewer {
                    Some(name) => format!("@{} ({backend})", printable(name)),
                    None => format!("({backend})"),
                },
            ),
            What::Account {
                target,
                followers,
                following,
                ..
            } => (
                crate::app::target_label(target.as_deref()),
                match (followers, following) {
                    (Some(a), Some(b)) => format!("{a} followers, {b} following"),
                    _ => String::new(),
                },
            ),
            What::Webhook {
                destination,
                status,
                signed,
            } => (
                "webhook".to_string(),
                match status {
                    Some(code) => format!(
                        "{destination} answered {code}{}",
                        if *signed { ", signed" } else { "" }
                    ),
                    None => destination.clone(),
                },
            ),
            What::Baseline { taken_at } => (
                "baseline".to_string(),
                match taken_at.first() {
                    Some((_, at)) => format!("stored on {}", report::stored_on(*at)),
                    None => "nothing stored yet".to_string(),
                },
            ),
        };

        let mark = match checked.verdict {
            Verdict::Ok => "ok  ",
            Verdict::Warned => "note",
            Verdict::Failed => "FAIL",
        };
        // Trimmed, because a check with nothing to say in the detail column
        // would otherwise pad to it and leave the line ending in spaces.
        let mut line = format!("{mark}  {label:<16}{detail}")
            .trim_end()
            .to_string();
        if let Some(problem) = &checked.problem {
            // Under the detail column rather than at column zero, so a reason
            // reads as belonging to the line above it. Named rather than
            // written inline: a run of spaces inside a string literal is what
            // the layout guard in `tests/language.rs` looks for, and it is
            // right to — this is the one shape it cannot tell from a typo.
            const UNDER_THE_LABEL: &str = "            ";
            line.push('\n');
            line.push_str(UNDER_THE_LABEL);
            line.push_str(&printable(problem));
        }
        lines.push(line);
    }
    lines
}

fn check_json(report: &crate::engine::check::CheckReport) -> serde_json::Value {
    use crate::engine::check::What;

    serde_json::json!({
        "verdict": report.verdict().as_str(),
        "checks": report.checked.iter().map(|checked| {
            let (what, detail) = match &checked.what {
                What::NotConfigured => ("config", serde_json::json!(null)),
                What::Schedule { next } => ("schedule", serde_json::json!({ "next": next })),
                What::Session { viewer, backend } => (
                    "session",
                    serde_json::json!({ "viewer": viewer, "storage": backend }),
                ),
                What::Account { target, pk, followers, following, may_run_unattended } => (
                    "account",
                    serde_json::json!({
                        "target": target,
                        "pk": pk,
                        "followers": followers,
                        "following": following,
                        "may_run_unattended": may_run_unattended,
                    }),
                ),
                What::Webhook { destination, status, signed } => (
                    "webhook",
                    serde_json::json!({
                        "destination": destination,
                        "status": status,
                        "signed": signed,
                    }),
                ),
                What::Baseline { taken_at } => (
                    "baseline",
                    serde_json::json!({
                        "lists": taken_at.iter()
                            .map(|(kind, at)| serde_json::json!({ "kind": kind.as_str(), "taken_at": at }))
                            .collect::<Vec<_>>(),
                    }),
                ),
            };
            serde_json::json!({
                "what": what,
                "verdict": checked.verdict.as_str(),
                "detail": detail,
                "problem": checked.problem,
            })
        }).collect::<Vec<_>>(),
    })
}

/// Why a list was not compared, said in a sentence.
///
/// `engine` handed over what happened and nothing else; which words that
/// deserves is this module's question, which is why the tokens are matched
/// here rather than carried as strings.
fn refusal_line(kind: ListKind, skipped: Skipped) -> String {
    match skipped {
        Skipped::NobodyLooked(provenance) => format!(
            "the {kind} list was served from storage and nothing checked whether it is still \
             true{}, so it was not compared and the monitor did not move on",
            match provenance {
                Provenance::Cooldown => " (the account is in cooldown)",
                Provenance::PollFailed => " (the check failed)",
                _ => "",
            }
        ),
        // The same half-sentence the summary uses for the same situation.
        // Writing a second one here is how two commands end up describing one
        // event in two ways.
        Skipped::Incomplete(reason, _) => format!(
            "the {kind} list could not be read in full ({}), so it was not compared: the \
             accounts missing from it would have been reported as people who left",
            report::why_incomplete(reason).unwrap_or("it stopped early")
        ),
    }
}

/// Shows what changed, and changes nothing.
///
/// The marks are deliberately left where they are. This command is a question,
/// and a question whose answer is different the second time it is asked is one
/// nobody can check: somebody who runs it, reads three departures and runs it
/// again to copy the names must get the same three. Advancing the marks is a
/// thing `snob watch` does when it has actually reported them somewhere.
fn diff(args: WatchDiffArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    // Nothing is fetched here, so there is no bar to draw.
    let Session::Open(app) = common::open_with_progress(false, &secrets, paths)? else {
        return Ok(ExitCode::NoSession);
    };

    let report = crate::engine::watch::from_store(&app, args.target.as_deref())?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&as_json(&report))?);
        return Ok(ExitCode::Ok);
    }

    for line in describe(&report, false) {
        println!("{line}");
    }
    Ok(ExitCode::Ok)
}

/// The machine-readable answer.
///
/// Hand-built rather than derived from the report, because this is a contract
/// with whatever is reading it and the struct behind it is not: renaming a
/// field in `ListReport` must not silently rename a key here.
fn as_json(report: &WatchReport) -> serde_json::Value {
    serde_json::json!({
        "account": {
            "pk": report.account_pk,
            // The true value, unfiltered. `printable` is for terminals; a
            // machine format has to carry the name that identifies the account,
            // and `serde_json` escapes what it emits. This is what the rest of
            // the tool's JSON already does.
            "username": report.username,
            "is_self": report.is_self,
        },
        "lists": {
            "followers": list_json(report.followers.as_ref()),
            "following": list_json(report.following.as_ref()),
        },
        "changes": {
            "followers": diff_json(report.followers.as_ref()),
            "following": diff_json(report.following.as_ref()),
            "renamed": report.renamed.iter().map(rename_json).collect::<Vec<_>>(),
        },
        "counts": {
            "followers_gained": count(report.followers.as_ref(), |d| d.gained.len()),
            "followers_lost":   count(report.followers.as_ref(), |d| d.lost.len()),
            "following_gained": count(report.following.as_ref(), |d| d.gained.len()),
            "following_lost":   count(report.following.as_ref(), |d| d.lost.len()),
            "renamed": report.renamed.len(),
        },
    })
}

/// A tick's answer, which is the report plus what the run itself did.
///
/// `looked` is the field an automation branches on and the one that cannot be
/// derived from the arrays: empty changes mean "nothing happened" when the run
/// looked and "I could not see" when it did not, and something watching for
/// silence reads those as the same thing.
///
/// **`schema` is here for the same reason it is on the wire.** The README's own
/// recipe is `snob watch --json >> events.ndjson`, which makes this file a data
/// feed with readers of its own, and it was the one output of the three with no
/// version on it: a receiver could version-check a webhook body and a preflight
/// and not the file it was told to append to. The number is the same one, and
/// it moves with the same rule, because it describes the same report -- the two
/// differ in what the run says about itself, not in what a change looks like.
fn tick_json(tick: &TickReport) -> serde_json::Value {
    let mut out = as_json(&tick.report);
    out["schema"] = serde_json::json!(SCHEMA);
    out["run"] = serde_json::json!({
        "at": tick.at(),
        "looked": tick.looked(),
        "requests": tick.requests,
        "lists": run_lists_json(tick),
    });
    out
}

/// Which lists this run read, and which it refused.
///
/// One builder for both streams. The webhook body used to say nothing about
/// this at all: a list served during a cooldown, after a failed poll or cut
/// short is dropped before the comparison, so it reaches [`payload`] as `None`
/// and serializes to `null` — the same `null` an account with no capture of
/// that list produces, with `counts.following_lost` at `0` either way. A run
/// where the following walk met the truncation wall and a run where nothing
/// happened were byte-identical, so `{{ $json.counts.following_lost > 0 }}`
/// routed to "quiet" for as long as the wall lasted. The refusal was said out
/// loud on standard error, where no receiver hears it.
fn run_lists_json(tick: &TickReport) -> serde_json::Value {
    tick.lists
        .iter()
        .map(|l| {
            serde_json::json!({
                "kind": l.kind.as_str(),
                "skipped": l.skipped.map(skipped_token),
            })
        })
        .collect::<Vec<_>>()
        .into()
}

/// The line a tick that failed leaves in the stream.
///
/// Written after the `?` in `tick_one`, the JSON line was not written at all: a
/// tick that could not resolve an account, or met a mid-walk cooldown, or could
/// not read `history_head`, put its whole account on standard error as an
/// English paragraph and left the file with no line for that interval. On a
/// recipe the README offers as a complete way to use the tool.
///
/// It carries the same `run` object a successful line carries, so one reader
/// can take `run.at` off every line without asking which kind it is, and
/// `error` is what tells the two apart. `error.code` is the vocabulary of the
/// README's exit table and of `watch_runs.outcome`, which is the field worth
/// branching on; `error.message` is the chain, for a person reading the file.
///
/// The name and the message are the true values, unfiltered, for the reason
/// `as_json` gives about a username: `printable` is for terminals, a machine
/// format has to carry what identifies the account, and `serde_json` escapes
/// what it emits. Everything drawn at a person still goes through
/// `report::print_error`.
fn failed_tick_json(
    watched: &Watched,
    error: &anyhow::Error,
    at: i64,
    requests: u32,
) -> serde_json::Value {
    serde_json::json!({
        // The failure line carries it too. README says every message this tool
        // emits carries `schema`, "including every line of the `--json`
        // stream", and this was the one line without it — so a reader that
        // branches on `msg["schema"] == 1`, which is what the version is for,
        // threw on exactly the ticks it most needed to handle.
        "schema": SCHEMA,
        "account": {
            "username": watched.name(),
            "is_self": watched.name().is_none(),
        },
        "run": {
            "at": at,
            "looked": false,
            "requests": requests,
            "lists": [],
        },
        "error": {
            "code": ExitCode::from_chain(error).unwrap_or(ExitCode::Error).as_str(),
            "message": format!("{error:#}"),
        },
    })
}

/// One JSON line. One object, one line, in both modes.
///
/// `once` used to lay its object out to be read, on the grounds that somebody
/// is looking at it — and that holds right up to the moment `watch.toml`
/// carries a second `[[account]]`, which is what `snob watch setup` writes as
/// soon as anybody answers yes to watching somebody else. `run_accounts` prints
/// one object per account with nothing wrapping them, so two pretty-printed
/// objects came out back to back: `json.load` stops at `Extra data`, and
/// PowerShell's `ConvertFrom-Json` refuses it outright. The bytes were in no
/// documented format at all — neither one document nor NDJSON — for the mode
/// whose own help says it is meant for cron.
///
/// So the layout is not a mode's decision any more. `--json` is a stream of
/// lines, which is what the CHANGELOG already promised and what
/// `snob watch once --json >> events.ndjson` has to mean. What the two modes
/// still differ on is whether a tick with no news is printed at all, and that
/// is [`Printing::watching`], where it belongs.
///
/// The fallback is `Value::to_string` rather than a `?` because one call site
/// is reached from the arm already handling a failure: a serializer error on a
/// `Value` built here is not a second failure worth returning instead of the
/// first.
fn json_line(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

/// The version of the shape every message this tool emits has.
///
/// One number, in one place, because there are three emitters and they were
/// three literals: the webhook body, the preflight, and the `--json` stream.
/// Two spellings of one condition is how a receiver comes to be told a report
/// is schema 1 and a preflight schema 2 for the same release, and this file
/// already carries that lesson about `event`.
///
/// It moves when a field is removed or its meaning changes, and not when one is
/// added: additive is what lets a receiver keep working, and `run.lists`
/// arriving beside `counts` did not move it.
const SCHEMA: u32 = 1;

/// What goes on the wire.
///
/// A contract with whatever is on the other end, so it is built here by hand
/// and asserted in a test: this is the one output of the tool that a stranger's
/// automation branches on, and a field renamed by accident breaks a workflow
/// somebody built months ago.
///
/// Four decisions worth knowing about, all of them about what an n8n node
/// actually needs:
///
/// - **`counts` is separate from `changes`**, and redundant with the array
///   lengths on purpose. `{{ $json.counts.followers_lost > 0 }}` is the
///   condition people write, and it is far less fragile than an expression over
///   `.length` on a field that may be absent.
/// - **`schema` and `event` are at the top**, so fields can be added later
///   without breaking anybody and a Switch node can tell a heartbeat from a
///   report without looking inside.
/// - **`looked` is not derivable from the arrays.** Empty changes mean "nothing
///   happened" when the run could see and "I could not look" when it could not,
///   and something watching for silence reads those as the same thing.
/// - **`run.lists` is `looked` at the granularity a receiver needs.** `looked`
///   is true when *either* list was read, so a run that walked followers and
///   was refused following says `true` while `lists.following` is `null` and
///   `counts.following_lost` is `0` — indistinguishable from a quiet run, and
///   from an account with no following capture at all. The token is per list
///   and additive, so `schema` stays 1. `lists.<kind>` deliberately stays
///   `null` rather than becoming an object: a receiver testing it against
///   `null` is the shape this shipped with, and there is no version to warn
///   them by.
fn payload(tick: &TickReport, run_id: &str, event: &str) -> serde_json::Value {
    let report = &tick.report;
    let changes = report.changes();

    serde_json::json!({
        "schema": SCHEMA,
        "event": event,
        "run": {
            "id": run_id,
            // The tick's own moment, not a third reading of the clock. It is
            // what `commit_report` files the mark at and what the `--json` line
            // carries, so one event has one time in all three places.
            "at": tick.at(),
            "looked": tick.looked(),
            "requests": tick.requests,
            "lists": run_lists_json(tick),
            "tool": { "name": "snob", "version": env!("CARGO_PKG_VERSION") },
        },
        "account": {
            "pk": report.account_pk,
            "username": report.username,
            "is_self": report.is_self,
        },
        "lists": {
            "followers": list_json(report.followers.as_ref()),
            "following": list_json(report.following.as_ref()),
        },
        "counts": {
            "followers_gained": changes.followers.gained.len(),
            "followers_lost":   changes.followers.lost.len(),
            "following_gained": changes.following.gained.len(),
            "following_lost":   changes.following.lost.len(),
            "renamed": changes.renamed.len(),
            "total": changes.len(),
        },
        "events": {
            // The accounts are the same `User` that `snob followers --format
            // json` already emits, plus the address: whoever receives this is
            // usually about to put it in a message, and rebuilding the URL at
            // the other end is exactly where somebody pastes a name without
            // encoding it.
            "followers_gained": changes.followers.gained.iter().map(account_json).collect::<Vec<_>>(),
            "followers_lost":   changes.followers.lost.iter().map(account_json).collect::<Vec<_>>(),
            "following_gained": changes.following.gained.iter().map(account_json).collect::<Vec<_>>(),
            "following_lost":   changes.following.lost.iter().map(account_json).collect::<Vec<_>>(),
            "renamed": changes.renamed.iter().map(rename_json).collect::<Vec<_>>(),
        },
    })
}

/// What `snob watch check` posts.
///
/// The same skeleton [`payload`] uses, and it did not used to be. Three events
/// exist; two go through `payload` and this one did not, so the preflight was
/// the only message with no `schema` — the one a receiver cannot version-check
/// — and it carried the id and the moment at the top level while every other
/// message carries them under `run`. `webhook_of`'s own doc claimed "a receiver
/// can branch on it exactly as it branches on the rest".
///
/// **Breaking**, for anybody reading `$json.run_id` or `$json.at` on a
/// preflight, and bounded: a preflight is never queued, so no stored body has
/// the old shape and there is nothing to migrate. `schema` stays 1 rather than
/// becoming 2 — the preflight is joining the family, and bumping it would make
/// every report receiver re-check a version over a message that did not change.
///
/// `looked`, `requests` and `lists` are deliberately absent: this run looked at
/// nothing and spent nothing on the account, and a `false` there would read as
/// a report that could not see rather than as a message that is not a report.
/// The `note` says so in words, for whoever opens one by hand.
fn preflight_body(run_id: &str, at: i64) -> serde_json::Value {
    serde_json::json!({
        "schema": SCHEMA,
        "event": crate::engine::check::PREFLIGHT_EVENT,
        "run": {
            "id": run_id,
            "at": at,
            "tool": { "name": "snob", "version": env!("CARGO_PKG_VERSION") },
        },
        "note": "snob watch check: this is not a report, and nothing is queued",
    })
}

/// One account, as an automation wants it.
fn account_json(user: &User) -> serde_json::Value {
    let mut value = serde_json::to_value(user).unwrap_or(serde_json::Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "profile_url".to_string(),
            serde_json::Value::String(user.profile_url()),
        );
    }
    value
}

/// The stable name of why a list was left out.
fn skipped_token(skipped: Skipped) -> &'static str {
    match skipped {
        Skipped::NobodyLooked(_) => "not_verified",
        Skipped::Incomplete(..) => "incomplete",
    }
}

fn count(report: Option<&ListReport>, of: impl Fn(&ListDiff) -> usize) -> usize {
    report.map(|r| of(&r.diff)).unwrap_or_default()
}

fn list_json(report: Option<&ListReport>) -> serde_json::Value {
    let Some(report) = report else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        // A stable token, so a caller can tell "nothing changed" from "this is
        // the first look" without reading a sentence.
        "basis": basis_token(report.basis),
        "since": report.since,
        "until": report.until,
        "count": report.total,
    })
}

fn diff_json(report: Option<&ListReport>) -> serde_json::Value {
    let Some(report) = report else {
        return serde_json::json!({ "gained": [], "lost": [] });
    };
    serde_json::json!({
        "gained": report.diff.gained,
        "lost": report.diff.lost,
    })
}

fn rename_json(rename: &Rename) -> serde_json::Value {
    serde_json::json!({
        "pk": rename.pk,
        "from": rename.from,
        "to": rename.to,
        "changed_at": rename.at,
    })
}

/// The stable name of what a list's report is. Written out here rather than on
/// [`Basis`] because it is vocabulary aimed at a caller, and the domain does
/// not decide how it is spelled.
fn basis_token(basis: Basis) -> &'static str {
    match basis {
        Basis::Baseline { .. } => "baseline",
        Basis::Unchanged { .. } => "unchanged",
        Basis::Compare { .. } => "compared",
    }
}

/// The answer as a person reads it.
///
/// Says that reports were abandoned, because nothing else will.
///
/// The sweep marks a report `expired` once it is too old to be news, and that
/// is the moment a set of arrivals and departures stops existing: `due` has
/// already been refusing to hand it back, so the retry ladder never reaches the
/// sentence in `send_one` that was written for exactly this. Until this line,
/// the whole event was a row changing state in silence -- the run printed
/// nothing, `status` counts only `pending` and so showed nothing, and the
/// health verdict went from `warning` to `ok` at the instant the news was lost.
fn say_what_was_given_up(given_up: usize) {
    if given_up == 0 {
        return;
    }
    let (subject, what) = if given_up == 1 {
        ("report was", "what it said is")
    } else {
        ("reports were", "what they said is")
    };
    eprintln!(
        "warning: {given_up} {subject} given up on for being too old to be news; \
         {what} not reported a second time."
    );
}

/// Returned as lines rather than printed, so a test can read them without
/// capturing standard output.
fn describe(report: &WatchReport, refused: bool) -> Vec<String> {
    let who = crate::app::label(report.account_pk, report.username.as_deref());

    if !report.has_anything_stored() {
        // A run in which every list was refused concluded nothing, and that is
        // not the same as an account nothing has ever been walked for. It used
        // to print "Nothing has been walked for @me yet -- run \"snob
        // followers\" once" two lines above the warnings saying both lists had
        // just been served from storage during a cooldown, with two complete
        // captures on disk. The reason comes from the refusal lines the caller
        // prints next, so this only has to stop claiming the opposite.
        if refused {
            return vec![format!("Nothing could be looked at for {who} this time.")];
        }
        return vec![format!(
            "Nothing has been walked for {who} yet, so there is nothing to compare.\n\
             Run \"snob followers\" once and this will have something to say from then on."
        )];
    }

    // Said before anything else, because everything after it would otherwise
    // read as "nothing happened" when what it means is "this is the first look".
    let baselines: Vec<ListKind> = [report.followers.as_ref(), report.following.as_ref()]
        .into_iter()
        .flatten()
        .filter(|r| matches!(r.basis, Basis::Baseline { .. }))
        .map(|r| r.kind)
        .collect();

    let mut lines = Vec::new();
    if !baselines.is_empty() {
        let which = baselines
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>()
            .join(" and ");
        // Both lists are the common case — a first look almost always finds
        // two — so the sentence has to read for two as well as for one.
        let (noun, verb) = if baselines.len() > 1 {
            ("lists have", "them")
        } else {
            ("list has", "it")
        };
        lines.push(format!(
            "The {which} {noun} never been reported on, so there is no earlier capture to \
             compare {verb} against. The next run is the first that can say anything."
        ));
    }

    let changes = report.changes();
    if changes.is_empty() {
        if baselines.is_empty() {
            // **Say what was done, rather than asserting the negative.** Three
            // different runs reached this one sentence and only one of them had
            // earned it.
            //
            // A list whose counter had not moved is served from storage without
            // being read, which is where nearly all of this tool's savings come
            // from and is worth keeping — walking three hundred accounts costs
            // fourteen requests and asking whether they changed costs one. But
            // a counter cannot see a swap: one departure and one arrival leave
            // it identical, and a rename does not move it at all. So a monitor
            // ticking every half hour printed "nothing has changed" over and
            // over, for up to the freshness window, while somebody had in fact
            // left. Nothing is lost — the walk happens once the capture ages
            // out and the comparison is against the last *reported* capture, so
            // the departure is reported in full then — but for those hours the
            // tool was stating a fact it had not checked.
            //
            // And a run where one list was refused and the other had no news
            // printed it too, because the refusal branch above is gated on
            // there being nothing stored at all and one surviving list gets
            // past it. On an account permanently behind the truncation wall
            // that is every run, about a list snob has never once read.
            //
            // The distinction is already in the domain type:
            // `Basis::Unchanged` is documented as "this one did not have to
            // look", against a `Compare` that read both and found nothing.
            let nobody_looked = [report.followers.as_ref(), report.following.as_ref()]
                .into_iter()
                .flatten()
                .all(|r| matches!(r.basis, Basis::Unchanged { .. }));

            lines.push(format!(
                "Nothing has changed for {who} since the last report."
            ));
            if refused {
                lines.push(
                    "One of the lists could not be read this time, so this speaks only for \
                     the other one."
                        .to_string(),
                );
            } else if nobody_looked {
                lines.push(
                    "Their counters had not moved, so the lists were not read again.".to_string(),
                );
            }
            lines.push(since_line(report));
        }
        return lines;
    }

    lines.push(format!("Changes for {who}{}", period(report)));
    lines.push(String::new());

    for (kind, diff) in [
        (ListKind::Followers, &changes.followers),
        (ListKind::Following, &changes.following),
    ] {
        lines.extend(list_lines(kind, diff));
    }

    if !changes.renamed.is_empty() {
        // The verb agrees, because one rename is the common case: it is what
        // `once`, `diff` and the loop print most often, and the line above the
        // names read "1 now go by another name". The baseline sentence a screen
        // up already branches this way for the same reason.
        let renamed = changes.renamed.len();
        lines.push(format!(
            "  {renamed} now {} by another name",
            if renamed == 1 { "goes" } else { "go" }
        ));
        for r in &changes.renamed {
            lines.push(format!(
                "    @{} is now @{}",
                printable(&r.from),
                printable(&r.to)
            ));
        }
    }

    lines
}

fn list_lines(kind: ListKind, diff: &ListDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for (verb, who) in [("gained", &diff.gained), ("lost", &diff.lost)] {
        if who.is_empty() {
            continue;
        }
        lines.push(format!("  {kind} {verb}: {}", who.len()));
        for user in who {
            lines.push(format!("    {}", name_of(user)));
        }
    }
    lines
}

/// A name as it is drawn, filtered because it came off Instagram.
fn name_of(user: &User) -> String {
    match user.full_name.as_deref().filter(|n| !n.trim().is_empty()) {
        Some(full) => format!("@{} ({})", user.safe_username(), printable(full)),
        None => format!("@{}", user.safe_username()),
    }
}

/// " since 14/08 at 09:12", when there is a moment to name.
fn period(report: &WatchReport) -> String {
    match earliest_since(report) {
        Some(since) => format!(" since {}", report::stored_on(since)),
        None => String::new(),
    }
}

fn since_line(report: &WatchReport) -> String {
    match earliest_since(report) {
        Some(since) => format!("The last report was on {}.", report::stored_on(since)),
        None => "Nothing has been reported yet.".to_string(),
    }
}

/// The older of the two receipts.
///
/// The two lists are marked apart and can be reported at different moments, so
/// the interval the user is being shown starts at whichever was reported first
/// — saying the later one would claim a window shorter than the one the numbers
/// actually cover.
fn earliest_since(report: &WatchReport) -> Option<i64> {
    [report.followers.as_ref(), report.following.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|r| r.since)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::watch::TickList;

    /// What both halves of this module build their reports out of.
    ///
    /// Gathered here rather than left loose in one test module, because
    /// `delivery` has a test module of its own now and a fixture private to
    /// a sibling is a fixture that gets written twice. `pub(in ...watch)` and
    /// no further: they are shapes for tests, and nothing outside this
    /// module has any business with them.
    pub(super) mod fixtures {
        use super::*;

        pub(in crate::commands::watch) const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
        pub(in crate::commands::watch) const SID: &str = "42%3AAbCdEfGh%3A20";

        pub(in crate::commands::watch) fn user(pk: Pk, name: &str) -> User {
            User {
                pk,
                username: name.into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            }
        }

        pub(in crate::commands::watch) fn report_with(
            followers: Option<ListReport>,
            renamed: Vec<Rename>,
        ) -> WatchReport {
            WatchReport {
                account_pk: 42,
                username: Some("me".into()),
                is_self: true,
                followers,
                following: None,
                renamed,
            }
        }

        pub(in crate::commands::watch) fn list(
            basis: Basis,
            diff: ListDiff,
            since: Option<i64>,
        ) -> ListReport {
            ListReport {
                kind: ListKind::Followers,
                basis,
                since,
                until: 2_000,
                diff,
                total: 10,
            }
        }

        /// An app and a webhook pointed at the same mock server.
        pub(in crate::commands::watch) fn app_posting_to(
            server: &wiremock::MockServer,
        ) -> (crate::app::App, Delivery) {
            let session = snob_core::session::Session::from_sessionid(
                SID,
                UA,
                snob_core::session::SessionOrigin::Paste,
            )
            .unwrap();
            let client = snob_ig::client::IgClient::new(session, snob_ig::pace::Pacer::unlimited())
                .unwrap()
                .with_base_url(Url::parse(&server.uri()).unwrap());

            let db = snob_core::store::Store::in_memory().unwrap();
            snob_core::store::users::upsert(db.conn(), &user(42, "me")).unwrap();
            snob_core::store::accounts::upsert(db.conn(), 42, true).unwrap();

            let app = crate::app::App::for_test(
                client,
                db,
                crate::app::Viewer {
                    pk: 42,
                    username: Some("me".into()),
                },
            );

            let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
            let delivery = Delivery {
                destination: super::delivery::destination_of(&url),
                signed: false,
                client: WebhookClient::new(Webhook {
                    url,
                    headers: vec![],
                    key: None,
                })
                .unwrap(),
                heartbeat: false,
            };
            (app, delivery)
        }

        /// A `watch.toml` as the tool would read one.
        pub(in crate::commands::watch) fn watch_toml(body: &str) -> WatchConfig {
            config::parse(body, std::path::Path::new("watch.toml")).expect("the fixture parses")
        }
    }

    use fixtures::{app_posting_to, list, report_with, user, watch_toml};

    /// An interval measures from the first start, not from this one.
    ///
    /// The seeded instant lived in a local variable, and `last_started` reads
    /// `watch_runs`, whose only writer is a tick that finished — so while that
    /// log stayed empty, every process start measured the interval from itself.
    /// A monitor with `--every 1d` on a laptop up eight hours a day is due at
    /// hour twenty-four and shut down at hour eight, every day, for ever, while
    /// `status` says "it has not run yet".
    #[test]
    fn an_interval_measures_from_the_first_start_and_not_from_this_one() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snob_core::paths::AppPaths::rooted_at(tmp.path());
        let every = Schedule::every(std::time::Duration::from_secs(24 * 3_600)).unwrap();

        let first = seed_for(&paths, &every, 1_700_000_000).unwrap();
        assert_eq!(first, Some(1_700_000_000));

        // A second start two hours later, with the run log still empty.
        let second = seed_for(&paths, &every, 1_700_007_200).unwrap();
        assert_eq!(
            second, first,
            "a restart must not put the interval back to zero"
        );
    }

    /// And a calendar still seeds nothing, because inventing a run for one
    /// steps over every moment in the next quarter of an hour.
    #[test]
    fn a_calendar_is_not_seeded() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snob_core::paths::AppPaths::rooted_at(tmp.path());
        let at_nine = Schedule::calendar(&[], &[schedule::parse_time("09:00").unwrap()]).unwrap();

        assert_eq!(seed_for(&paths, &at_nine, 1_700_000_000).unwrap(), None);
    }

    /// A tick that failed is still a tick that happened, and it still spent.
    ///
    /// `record_run` is reached only through `commit`, the last statement of a
    /// *successful* `tick_one`, so any `Err` out of `tick` wrote no row at all.
    /// `status` went on printing "@friend last ran on <the last good date> and
    /// found nothing", `health` saw a stale `ok`, and `prune` keeps the newest
    /// row per account for ever so it never aged into "it has not run yet".
    ///
    /// The count comes from the pacer around the tick rather than out of the
    /// report, because `TickReport.requests` is worked out at the end of `tick`
    /// and any `?` on the way out throws it away — after the pacer has already
    /// been charged.
    #[tokio::test]
    async fn a_run_that_failed_is_recorded_and_counts_what_it_spent() {
        let server = wiremock::MockServer::start().await;
        // The profile poll is refused, so `target::resolve` fails and the `?`
        // carries out of `tick` — one request spent, no report, no commit.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let (mut app, _delivery) = app_posting_to(&server);
        // The account has to be known locally, or the foreign key has nothing
        // to point at — which is its own gap, written down at the call site.
        snob_core::store::users::upsert(app.db().conn(), &user(99, "friend")).unwrap();
        snob_core::store::accounts::upsert(app.db().conn(), 99, false).unwrap();

        let watched = [Watched::consented(
            "friend".into(),
            crate::engine::watch::Consent,
        )];
        // Driven through `run_accounts` rather than `run_one`, because the
        // count `once` prints is what it returns and `run_one` discards it.
        let outcome = run_accounts(&mut app, &watched, None, Printing::unattended(false)).await;

        assert!(
            outcome.failed.is_some(),
            "the account could not be resolved"
        );
        assert!(
            outcome.spent >= 1,
            "the poll was charged, so the run has to say it spent it: {}",
            outcome.spent
        );

        let runs = snob_core::store::watch::last_runs(app.db().conn()).unwrap();
        let recorded = runs
            .iter()
            .find(|r| r.account_pk == 99)
            .expect("a tick that failed is a tick that happened");
        assert_ne!(
            recorded.outcome.as_deref(),
            Some("ok"),
            "and it must not read as a run that worked"
        );
        assert!(
            recorded.requests >= 1,
            "the poll was spent and charged, so it has to be counted: {recorded:?}"
        );
    }

    /// Each failure reaches the journal once, and the run still carries one.
    ///
    /// `once` and the scheduled loop both looped over the accounts, and the
    /// copies had drifted: `once` took the print-when-nobody-else-will half and
    /// not the guard on the return, so a multi-account run printed every
    /// failing account's whole block and then handed one back for `main` to
    /// print again. The guard itself was no better — it bought "printed once"
    /// by returning `Ok` from a run in which an account had failed.
    #[test]
    fn every_failure_is_printed_once_and_the_run_still_carries_one() {
        let (printed, returned) = to_print_and_to_return(vec![
            anyhow::anyhow!("first"),
            anyhow::anyhow!("second"),
            anyhow::anyhow!("third"),
        ]);
        assert_eq!(
            printed.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["first", "second"],
            "in the order they happened"
        );
        assert_eq!(returned.unwrap().to_string(), "third");

        // One watched account is the default, and the case that used to print
        // twice: nothing prints here, so the caller's print is the only one.
        let (printed, returned) = to_print_and_to_return(vec![anyhow::anyhow!("only")]);
        assert!(printed.is_empty());
        assert_eq!(returned.unwrap().to_string(), "only");

        let (printed, returned) = to_print_and_to_return(Vec::new());
        assert!(printed.is_empty() && returned.is_none());
    }

    /// The run's code is the first reason, and there is no worst to take.
    ///
    /// The field said "the worst verdict a successful tick reported" over a type
    /// with no ordering. The fix that reading invites is deriving `Ord` on
    /// `ExitCode` and taking the maximum -- and the discriminants are the shell
    /// convention, so that order puts `Interrupted` (130) above `NoSession` (3)
    /// and a run somebody stopped outranks a session that has gone. This is what
    /// `once` hands back as its process exit code, and the codes exist so a
    /// caller on a timer can tell "wait a while" from "log in again" without
    /// parsing text.
    #[test]
    fn the_run_answers_with_the_first_reason_not_the_worst() {
        assert_eq!(
            first_reason(ExitCode::Ok, ExitCode::RateLimited),
            ExitCode::RateLimited,
            "the first account with something to say is what the run says"
        );
        assert_eq!(
            first_reason(ExitCode::RateLimited, ExitCode::NoSession),
            ExitCode::RateLimited,
            "a later account does not overwrite an earlier reason"
        );
        assert_eq!(
            first_reason(ExitCode::Interrupted, ExitCode::Error),
            ExitCode::Interrupted,
            "and not by being higher or lower than it"
        );
        assert_eq!(
            first_reason(ExitCode::Ok, ExitCode::Ok),
            ExitCode::Ok,
            "a run where every account was fine is fine"
        );
    }

    /// Both slots of the refusal take the same name, so both must be filtered.
    ///
    /// The account comes from `watch.toml`, which nothing validates, and the
    /// refusal is the one place the monitor prints it before any client
    /// exists — the earliest a hostile name can reach a terminal.
    #[test]
    fn the_unattended_refusal_filters_the_name_in_both_slots() {
        let message = refuse_unattended("gh\u{1b}[2K\u{1b}[A").to_string();

        assert!(
            !message.chars().any(|c| c.is_control() && c != '\n'),
            "{message:?} is printed to a terminal and written to the journal"
        );
        assert_eq!(
            message.matches("gh[2K[A").count(),
            2,
            "the account and the command that answers for it name the same person: {message}"
        );
    }

    /// A report queued longer ago than it can be news for, in a store at
    /// `paths`. Nothing but a settle can move it: `due` will not hand back an
    /// over-age row, and only a failed attempt expires one.
    fn owed_long_ago(paths: &snob_core::paths::AppPaths, now: i64) -> i64 {
        let db = snob_core::store::Store::open(paths).unwrap();
        snob_core::store::users::upsert(db.conn(), &user(42, "me")).unwrap();
        snob_core::store::accounts::upsert(db.conn(), 42, true).unwrap();
        deliveries::enqueue(
            db.conn(),
            "run-old",
            42,
            "{}",
            now - deliveries::MAX_AGE_SECS - 1,
            Some("https://receiver.example"),
        )
        .unwrap()
    }

    /// A run with no session still settles the queue.
    ///
    /// `settle` is the only caller of `store::prune` and both of its call sites
    /// are behind doors this run never reaches -- `run_accounts` needs an `App`,
    /// and this one returns the moment `common::open` says there is no session.
    /// So `snob watch` under a unit after `snob logout`, or on a morning when
    /// the keyring is locked, expires nothing at all for as long as that lasts:
    /// not the owed reports, not old captures, not the run log. `status` goes on
    /// counting a report `due` will never hand back and promising the next run
    /// will try it.
    #[tokio::test]
    async fn a_scheduled_run_with_no_session_still_settles_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snob_core::paths::AppPaths::rooted_at(tmp.path());
        let now = snob_core::store::now();
        let id = owed_long_ago(&paths, now);

        let secrets = snob_core::secrets::SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-settle-{}", std::process::id()));
        let args = WatchRunArgs {
            no_progress: true,
            ..WatchRunArgs::default()
        };

        open_and_run(&args, &[], None, &secrets, &paths)
            .await
            .expect("no session is not a failure; the loop keeps going");

        let db = snob_core::store::Store::open(&paths).unwrap();
        assert_eq!(
            deliveries::state(db.conn(), id).unwrap().as_deref(),
            Some("expired"),
            "the run had no session, and retention still has to happen"
        );
    }

    /// And neither does a webhook the run refuses.
    ///
    /// `delivery_from` ends in `webhook::check`, and both modes call it before
    /// anything is opened -- deliberately, so an address that could never work
    /// costs nothing to find out about. The cost was that a refused address took
    /// retention with it. A header this tool already sends is a natural thing to
    /// write into `[webhook.headers]`, since every header it sends starts
    /// `X-Snob-`, and it stopped every run of both modes before either settle
    /// site -- with `status` still saying the next run would try what was
    /// queued.
    #[tokio::test]
    async fn a_webhook_the_run_refuses_still_lets_the_queue_settle() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snob_core::paths::AppPaths::rooted_at(tmp.path());
        let now = snob_core::store::now();
        let id = owed_long_ago(&paths, now);

        let secrets = snob_core::secrets::SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-refused-{}", std::process::id()));
        let args = WatchOnceArgs {
            delivery: crate::cli::WebhookArgs {
                webhook: Some("https://receiver.example/hook".to_string()),
                header: vec!["X-Snob-Source: homelab".to_string()],
                sign_with: None,
                heartbeat: false,
            },
            target: None,
            json: false,
            no_progress: true,
        };

        let refused = once(args, secrets, &paths).await;
        assert!(
            refused.is_err(),
            "that header is part of what snob sends, so the address is refused"
        );

        let db = snob_core::store::Store::open(&paths).unwrap();
        assert_eq!(
            deliveries::state(db.conn(), id).unwrap().as_deref(),
            Some("expired"),
            "the address was refused, and the database still has to be tidied"
        );
    }

    /// A stranger is asked unless the file records that somebody answered.
    ///
    /// `with_recorded_consent` is the only thing standing between `--target`
    /// and a scheduled run enumerating somebody else's lists: make its `None`
    /// arm hand back a `Watched::consented` and an unattended run reads a
    /// stranger on nobody's say-so. Nothing went through it. The test that
    /// looks like it covers this asks `may_run_unattended` of three values
    /// built by hand, so it never reaches the function that decides which of
    /// the three you get.
    #[test]
    fn a_stranger_is_asked_unless_the_file_says_somebody_answered() {
        let file = watch_toml(
            r#"
every = "6h"

[[account]]
target = "self"

[[account]]
target = "friend"
consent = { agreed_at = 1700 }

[[account]]
target = "acquaintance"
"#,
        );

        let asked = |name: &str, config: Option<&WatchConfig>| {
            let watched = watched_from(Some(name.to_string()), config);
            assert_eq!(watched.len(), 1);
            !watched[0].may_run_unattended()
        };

        assert!(
            !asked("friend", Some(&file)),
            "the file records an answer for them"
        );
        assert!(
            asked("stranger", Some(&file)),
            "nobody ever agreed to this one being read"
        );
        assert!(
            asked("acquaintance", Some(&file)),
            "listed is not the same as consented -- the answer is the `consent` table"
        );
        assert!(
            asked("friend", None),
            "with no file there is nowhere an answer could have been recorded"
        );

        // Instagram's spelling and the typed one need not agree in case.
        assert!(!asked("FRIEND", Some(&file)));

        // `self` on the command line is not the `[[account]] target = "self"`
        // line: that one is your own account, which needs nobody's permission,
        // and matching it would hand a stranger named `self` a consent.
        assert!(asked("self", Some(&file)));
    }

    /// The at sign a person types does not change which account is watched.
    ///
    /// Both spellings mean the same account and README.md says so without
    /// qualification, but `Watched` was built from the raw string. Typing
    /// `snob watch "@friend"` against a file recording an answer for
    /// `friend` found no answer, so a correctly configured, correctly consented
    /// service died at startup quoting a consent that is written in the file it
    /// had just read -- and it died before the first tick, so it looked like a
    /// configuration error rather than a spelling one. The name also travelled
    /// on: `engine::check` sent it as `username=@friend` and called a working
    /// monitor broken.
    #[test]
    fn an_at_sign_does_not_change_which_account_is_watched() {
        let file = watch_toml(
            r#"
every = "6h"

[[account]]
target = "friend"
consent = { agreed_at = 1700 }
"#,
        );

        let typed = watched_from(Some("@friend".to_string()), Some(&file));
        assert_eq!(typed.len(), 1);
        assert_eq!(
            typed[0].name(),
            Some("friend"),
            "what reaches the profile endpoint is a username, and the sign is not part of one"
        );
        assert!(
            typed[0].may_run_unattended(),
            "the file records an answer for this account, however it was spelled"
        );

        // And from the other side: the file is documented as safe to hand-edit,
        // so the sign can be in it instead.
        let edited = watch_toml(
            r#"
every = "6h"

[[account]]
target = "@friend"
consent = { agreed_at = 1700 }
"#,
        );
        let listed = watched_from(None, Some(&edited));
        assert_eq!(
            listed.iter().map(|w| w.name()).collect::<Vec<_>>(),
            vec![Some("friend")]
        );
        assert!(listed[0].may_run_unattended());

        // `@self` is the same line as `self`, and it is your own account. Read
        // as a stranger it would send a scheduled run looking for confirmation
        // to read an account it owns.
        let own = watch_toml("every = \"6h\"\n\n[[account]]\ntarget = \"@self\"\n");
        let listed = watched_from(None, Some(&own));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name(), None, "your own account names nobody");
        assert!(listed[0].may_run_unattended());
    }

    /// Every account the file lists is watched, and a file that lists none
    /// means your own.
    #[test]
    fn the_accounts_watched_are_the_ones_the_file_names() {
        let file = watch_toml(
            r#"
every = "6h"

[[account]]
target = "self"

[[account]]
target = "friend"
consent = { agreed_at = 1700 }
"#,
        );

        let watched = watched_from(None, Some(&file));
        let names: Vec<Option<&str>> = watched.iter().map(|w| w.name()).collect();
        assert_eq!(names, [None, Some("friend")]);
        assert!(watched.iter().all(|w| w.may_run_unattended()));

        // A schedule with no `[[account]]` at all means the obvious thing.
        let bare = watch_toml("every = \"6h\"\n");
        let watched = watched_from(None, Some(&bare));
        assert_eq!(watched.len(), 1);
        assert_eq!(watched[0].name(), None);
    }

    /// Not posting is not the same as having nowhere to post.
    ///
    /// With `--no-webhook` no entry was pushed at all, so `check --json` had no
    /// webhook object and the terminal no webhook line -- indistinguishable from
    /// a machine with no `[webhook]` in its file. That is the flag somebody uses
    /// to check everything else without disturbing a receiver, so it is exactly
    /// when the question "is a receiver configured?" is being asked. The address
    /// and every configured header have been through `webhook::check` by then,
    /// so there is a validation that passed to report rather than nothing.
    #[test]
    fn not_posting_is_not_the_same_as_having_nowhere_to_post() {
        use crate::engine::check::{CheckReport, Verdict, What};

        const WHERE_TO: &str = "https://n8n.local/webhook/snob";

        let checked =
            not_posted(Some((WHERE_TO, true)), true).expect("the address was checked and not used");
        assert!(
            matches!(&checked.what, What::Webhook { destination, status: None, signed: true }
                if destination == WHERE_TO),
            "the address it did not post to is the answer: {:?}",
            checked.what
        );
        assert_eq!(checked.verdict, Verdict::Ok);
        assert!(
            checked
                .problem
                .as_deref()
                .is_some_and(|p| p.contains("--no-webhook")),
            "and the line has to say why nothing was posted: {:?}",
            checked.problem
        );

        // A run that is going to post reports what came back instead, and an
        // address nobody configured has nothing to say either way.
        assert!(not_posted(Some((WHERE_TO, true)), false).is_none());
        assert!(not_posted(None, true).is_none());

        // The whole point is that it reaches both readers.
        let report = CheckReport {
            checked: vec![checked],
        };
        assert!(
            describe_check(&report)
                .iter()
                .any(|l| l.contains("webhook")),
            "a probe cannot tell a receiver that was not posted to from no receiver"
        );
        assert_eq!(check_json(&report)["checks"][0]["what"], "webhook");
    }

    /// `snob watch once` watches what the file says to watch.
    ///
    /// It read `watch.toml` for the webhook address and then built the watched
    /// set from the command line alone, so somebody who ran `watch setup`, added
    /// @friend and followed the README's "one run, for cron or a systemd timer"
    /// never had @friend walked, marked or reported. Typing the name instead was
    /// no answer either: `Watched::asking` carries no consent, and an unattended
    /// run accepts only a recorded one.
    ///
    /// Both modes go through `watched_from` now, so there is one answer to
    /// "which accounts" rather than two that disagree.
    ///
    /// What this pins is that answer. That `once` asks for it rather than
    /// building its own is not reachable from here — it would take an
    /// integration test that opens a session and a keyring to drive the command
    /// — so it is said in the doc-comment on `once` instead, where somebody
    /// changing it will read it.
    #[test]
    fn once_watches_the_accounts_the_file_lists() {
        let file = WatchConfig {
            schema: 1,
            every: Some(std::time::Duration::from_secs(6 * 3600)),
            at: vec![],
            on: vec![],
            cron: None,
            jitter: None,
            webhook: None,
            accounts: vec![
                snob_core::watch::config::AccountConfig {
                    target: "self".to_string(),
                    consent: None,
                },
                snob_core::watch::config::AccountConfig {
                    target: "friend".to_string(),
                    consent: Some(snob_core::watch::config::ConsentConfig {
                        agreed_at: 1_700_000_000,
                    }),
                },
            ],
        };

        let watched = watched_from(None, Some(&file));
        let names: Vec<Option<&str>> = watched.iter().map(|w| w.name()).collect();
        assert_eq!(
            names,
            vec![None, Some("friend")],
            "the file's own account and the one it lists"
        );
        assert!(
            watched.iter().all(|w| w.may_run_unattended()),
            "the recorded consent is what makes the second one legal on a timer"
        );

        // And a name typed on the command line still picks up the answer on
        // record, rather than discarding it and failing every unattended run.
        let typed = watched_from(Some("friend".to_string()), Some(&file));
        assert_eq!(typed.len(), 1);
        assert!(typed[0].may_run_unattended());
    }

    /// `--jitter` is not one of the schedule halves, and is not dropped with
    /// them.
    ///
    /// The halves come as a set so a flag cannot merge into a configured
    /// calendar and produce a schedule neither source names. The jitter is not
    /// part of that: with the schedule in the file, `snob watch --jitter 0`
    /// threw the flag away and the banner printed "Each run is pushed up to
    /// fifteen minutes later" over the value that had just been typed. The help
    /// and the CHANGELOG both say "0 turns it off", unqualified.
    #[test]
    fn a_typed_jitter_survives_a_schedule_that_came_from_the_file() {
        let file = WatchConfig {
            schema: 1,
            every: None,
            at: vec!["09:00".to_string()],
            on: vec![],
            cron: None,
            jitter: Some(std::time::Duration::from_secs(600)),
            webhook: None,
            accounts: vec![],
        };

        let args = WatchRunArgs {
            jitter: Some(std::time::Duration::ZERO),
            ..Default::default()
        };
        let when = when_from(&args, Some(&file));
        assert_eq!(
            when.jitter,
            Some(std::time::Duration::ZERO),
            "the flag is what the user just typed"
        );
        assert_eq!(
            when.at,
            vec!["09:00".to_string()],
            "and the schedule still comes from the file"
        );

        // Without the flag, the file's own value stands.
        let bare = when_from(&WatchRunArgs::default(), Some(&file));
        assert_eq!(bare.jitter, Some(std::time::Duration::from_secs(600)));
    }

    /// A jitter in the file survives a schedule typed on the command line.
    ///
    /// The other direction of the same rule. `when_from` returns early as soon
    /// as any schedule flag is given, carrying `args.jitter` alone — so a
    /// file's `jitter = "0"`, documented unqualified as turning jitter off,
    /// became the built-in default the moment somebody typed `--every`. The
    /// halves come as a set; the jitter is not one of them.
    #[test]
    fn a_configured_jitter_survives_a_schedule_that_was_typed() {
        let file = WatchConfig {
            schema: 1,
            every: None,
            at: vec!["09:00".to_string()],
            on: vec![],
            cron: None,
            jitter: Some(std::time::Duration::ZERO),
            webhook: None,
            accounts: vec![],
        };

        let typed = WatchRunArgs {
            every: Some(std::time::Duration::from_secs(7200)),
            ..Default::default()
        };
        let when = when_from(&typed, Some(&file));

        assert_eq!(
            when.jitter,
            Some(std::time::Duration::ZERO),
            "the file said to turn it off, and nothing here said otherwise"
        );
        assert_eq!(
            when.every,
            Some(std::time::Duration::from_secs(7200)),
            "and the schedule halves still come as a set"
        );
        assert!(when.at.is_empty(), "the file's calendar does not merge in");
    }

    /// A fresh install does not step over the first moment its calendar names.
    ///
    /// The seed used to be `Some(now)` whatever the schedule, which is a claim
    /// that a run happened this second — so the floor became `now +
    /// MIN_GAP_SECS` and every moment in the next quarter of an hour was skipped.
    /// `snob watch --at 09:00` started at 08:50 said "Running at 09:00." and
    /// then slept for a day and ten minutes.
    #[test]
    fn a_fresh_install_keeps_the_first_moment_its_calendar_names() {
        // Midnight UTC on a Monday, so the wall clock is checkable by hand.
        const MONDAY_0000: i64 = 1_786_924_800;
        let at = |secs: i64| MONDAY_0000 + secs;

        let calendar = Schedule::cron("0 9 * * *").unwrap();
        let ten_to_nine = at(8 * 3600 + 50 * 60);

        assert_eq!(
            seed_last_run(None, &calendar, ten_to_nine),
            None,
            "a calendar has its own moments and needs no invented run"
        );
        assert_eq!(
            schedule::due(&calendar, None, ten_to_nine, &chrono::Utc),
            schedule::Due::At(at(9 * 3600)),
            "nine o'clock is ten minutes away, which is inside the minimum gap"
        );

        // An interval is the other way round: with no past to measure from it
        // would be due immediately, and a fresh install must not walk the second
        // it is set up.
        let interval = Schedule::every(std::time::Duration::from_secs(6 * 3600)).unwrap();
        assert_eq!(
            seed_last_run(None, &interval, ten_to_nine),
            Some(ten_to_nine)
        );

        // And a recorded run always wins over both.
        assert_eq!(
            seed_last_run(Some(at(0)), &calendar, ten_to_nine),
            Some(at(0))
        );
        assert_eq!(
            seed_last_run(Some(at(0)), &interval, ten_to_nine),
            Some(at(0))
        );
    }

    /// `--every 2w --on mon` is a schedule, and it is the one the README names.
    ///
    /// Days with no time of day were routed to `Schedule::calendar`, which is a
    /// set of minutes and refuses one that names none -- so the interval was
    /// never looked at and the answer was "a calendar needs a time of day".
    /// README.md, CHANGELOG.md and AGENTS.md all print this shape as the thing
    /// cron cannot express, and a user following the README was turned away and
    /// pointed at a flag they had deliberately not typed. Through `watch.toml`
    /// it is worse: `config::parse` accepts `every` beside `on`, so an installed
    /// service refused at startup on every run over a file it had accepted.
    #[test]
    fn one_monday_in_every_two_is_what_the_readme_says_it_is() {
        // Midnight UTC on Monday 2026-08-17, so the weekday arithmetic is
        // checkable by hand.
        const MONDAY_0000: i64 = 1_786_924_800;
        const A_FORTNIGHT: i64 = 14 * 24 * 3600;

        let typed = WatchRunArgs {
            every: Some(std::time::Duration::from_secs(A_FORTNIGHT as u64)),
            on: vec!["mon".to_string()],
            ..Default::default()
        };
        let schedule = schedule_from(&typed, None).expect("the README prints this one");

        // Run on the Monday at nine: the next moment is a Monday, and it is the
        // one a fortnight later rather than the one a week later. That is the
        // conjunction the shape exists for -- the interval is the floor and the
        // days are the grid.
        let ran_at = MONDAY_0000 + 9 * 3600;
        assert_eq!(
            schedule::next_moment(&schedule, Some(ran_at), ran_at + 60, &chrono::Utc),
            Some(ran_at + A_FORTNIGHT),
            "a weekly grid with a fortnightly floor is one Monday in every two"
        );

        // And the file says it the same way, which is where it really arrives
        // from: nothing in `config::parse` stands between a hand-edit and this.
        let file = watch_toml("every = \"2w\"\non = [\"mon\"]\n");
        assert_eq!(
            schedule_from(&WatchRunArgs::default(), Some(&file))
                .map(|s| format!("{s:?}"))
                .map_err(|e| e.to_string()),
            Ok(format!("{schedule:?}")),
            "typed and configured have to be the same schedule"
        );
    }

    /// Every schedule field either source has reaches the schedule that runs.
    ///
    /// The five fields were spelled three times in `when_from` and copied out by
    /// hand in two arms, and the jitter fell through that copy in both
    /// directions. It was repaired by special-casing the field that had been
    /// forgotten, which leaves the next field added to `watch.toml` dropped the
    /// same way, in silence, with the banner announcing a default nobody chose.
    ///
    /// Every field is given a value nothing else here has, so a field that is
    /// dropped fails and a field that is copied into its neighbour fails too --
    /// `at` and `on` are both `Vec<String>` and `every` and `jitter` are both
    /// `Option<Duration>`, and a swap between either pair compiles.
    ///
    /// `cron` beside `at` is a shape `config::parse` refuses and `cli.rs`
    /// declares as conflicting. It is built here anyway, because `When` only
    /// carries the fields and the thing under test is the carrying: a copy has
    /// to be checked over every field at once or it is checked over none.
    #[test]
    fn no_schedule_field_is_lost_between_a_source_and_the_run() {
        use std::time::Duration;

        let file = WatchConfig {
            schema: 1,
            every: Some(Duration::from_secs(3_600)),
            at: vec!["09:00".to_string()],
            on: vec!["mon".to_string()],
            cron: Some("0 9 * * 1,4".to_string()),
            jitter: Some(Duration::from_secs(120)),
            webhook: None,
            accounts: vec![],
        };
        let from_file = when_from(&WatchRunArgs::default(), Some(&file));
        assert_eq!(from_file.every, file.every, "the file's interval");
        assert_eq!(from_file.at, file.at, "the file's times of day");
        assert_eq!(from_file.on, file.on, "the file's days");
        assert_eq!(
            from_file.cron, file.cron,
            "the file's cron expression did not reach the run"
        );
        assert_eq!(from_file.jitter, file.jitter, "the file's jitter");

        let typed = WatchRunArgs {
            every: Some(Duration::from_secs(7_200)),
            at: vec!["21:30".to_string()],
            on: vec!["thu".to_string()],
            cron: Some("0 21 * * 4".to_string()),
            jitter: Some(Duration::from_secs(300)),
            ..Default::default()
        };
        let from_flags = when_from(&typed, Some(&file));
        assert_eq!(from_flags.every, typed.every, "the typed interval");
        assert_eq!(from_flags.at, typed.at, "the typed times of day");
        assert_eq!(from_flags.on, typed.on, "the typed days");
        assert_eq!(
            from_flags.cron, typed.cron,
            "the typed cron expression did not reach the run"
        );
        assert_eq!(from_flags.jitter, typed.jitter, "the typed jitter");

        // And with neither source there is nothing to carry, which is the third
        // arm and the one that had its own hand-written copy of the field list.
        assert_eq!(when_from(&WatchRunArgs::default(), None), When::default());
    }

    /// Somebody who has never run the tool is told to run it, not told that
    /// nothing changed — which would be true and useless.
    #[test]
    fn an_account_with_nothing_stored_is_told_what_to_run() {
        let lines = describe(&report_with(None, vec![]), false);
        assert!(lines.join("\n").contains("snob followers"), "{lines:?}");
    }

    /// A run that could not look at either list is not an account nothing has
    /// been walked for.
    ///
    /// Both are "no report to show", and they were printed the same way — so a
    /// tick during a cooldown, with two complete captures on disk, said "Nothing
    /// has been walked for @me yet" and told the reader to run `snob followers`,
    /// two lines above the warnings saying both lists had just been served from
    /// storage.
    #[test]
    fn a_run_that_could_not_look_does_not_claim_the_account_is_unknown() {
        let lines = describe(&report_with(None, vec![]), true).join("\n");
        assert!(!lines.contains("snob followers"), "{lines}");
        assert!(lines.contains("could be looked at"), "{lines}");
    }

    /// The worst thing this feature could print. A first look has no earlier
    /// capture, so it must say so rather than report an empty diff as calm.
    #[test]
    fn a_first_look_says_so_instead_of_saying_nothing_changed() {
        let lines = describe(
            &report_with(
                Some(list(
                    Basis::Baseline { snapshot_id: 1 },
                    ListDiff::default(),
                    None,
                )),
                vec![],
            ),
            false,
        );
        let text = lines.join("\n");
        assert!(text.contains("never been reported"), "{text}");
        assert!(
            !text.contains("Nothing has changed"),
            "a baseline is not a quiet account: {text}"
        );
    }

    /// A first look almost always finds both lists, so the common case is the
    /// plural one — and it read "the followers and following list has" until
    /// somebody ran it.
    #[test]
    fn a_first_look_at_both_lists_says_so_in_the_plural() {
        let baseline = |kind| ListReport {
            kind,
            basis: Basis::Baseline { snapshot_id: 1 },
            since: None,
            until: 2_000,
            diff: ListDiff::default(),
            total: 309,
        };
        let report = WatchReport {
            account_pk: 42,
            username: Some("me".into()),
            is_self: true,
            followers: Some(baseline(ListKind::Followers)),
            following: Some(baseline(ListKind::Following)),
            renamed: vec![],
        };

        let text = describe(&report, false).join("\n");
        assert!(text.contains("lists have never been reported"), "{text}");
        assert!(!text.contains("list has never"), "{text}");
    }

    /// A run that did not read the lists must not assert that nothing changed.
    ///
    /// `Basis::Unchanged` is documented as "this one did not have to look": the
    /// counter had not moved, so the stored capture was served without being
    /// re-read. A counter cannot see a swap — one departure and one arrival
    /// leave it identical — so a monitor printed "nothing has changed" for
    /// hours while somebody had left. The saving is right and stays; the
    /// unhedged sentence was not.
    #[test]
    fn a_run_that_did_not_look_says_so() {
        let unread = describe(
            &report_with(
                Some(list(
                    Basis::Unchanged { snapshot_id: 7 },
                    ListDiff::default(),
                    Some(1_000),
                )),
                vec![],
            ),
            false,
        )
        .join("\n");
        assert!(
            unread.contains("were not read again"),
            "a counter poll is not a look: {unread}"
        );

        // And a run that really did read both and found nothing keeps the
        // plain sentence, because there it is true.
        let read = describe(
            &report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff::default(),
                    Some(1_000),
                )),
                vec![],
            ),
            false,
        )
        .join("\n");
        assert!(read.contains("Nothing has changed"), "{read}");
        assert!(
            !read.contains("were not read again"),
            "this one did look: {read}"
        );

        // A refused list is a third case, and it used to print the same
        // sentence as the other two: the refusal branch is reached only when
        // *nothing* is stored, so one surviving list got past it.
        let partial = describe(
            &report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff::default(),
                    Some(1_000),
                )),
                vec![],
            ),
            true,
        )
        .join("\n");
        assert!(
            partial.contains("could not be read this time"),
            "a refused list must not be reported as quiet: {partial}"
        );
    }

    #[test]
    fn an_arrival_and_a_departure_are_both_named() {
        let diff = ListDiff {
            gained: vec![user(1, "arrived")],
            lost: vec![user(2, "left")],
        };
        let lines = describe(
            &report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    diff,
                    Some(1_000),
                )),
                vec![],
            ),
            false,
        );

        let text = lines.join("\n");
        assert!(text.contains("@arrived"), "{text}");
        assert!(text.contains("@left"), "{text}");
        assert!(text.contains("followers gained: 1"), "{text}");
        assert!(text.contains("followers lost: 1"), "{text}");
    }

    /// A rename on its own is news. It used to be possible for the empty-diff
    /// check to swallow a run whose only change was somebody's name.
    #[test]
    fn a_rename_on_its_own_is_still_reported() {
        let lines = describe(
            &report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff::default(),
                    Some(1_000),
                )),
                vec![Rename {
                    pk: 7,
                    history_id: 7,
                    from: "before".into(),
                    to: "after".into(),
                    at: 1_500,
                }],
            ),
            false,
        );

        let text = lines.join("\n");
        assert!(text.contains("@before is now @after"), "{text}");
        assert!(!text.contains("Nothing has changed"), "{text}");
    }

    /// And one rename is counted as one.
    ///
    /// The test above builds exactly one `Rename` and asserts only the line
    /// *below* the count, so it passes with that line deleted and passed with it
    /// reading "1 now go by another name". One is the common case here: it is
    /// what `once`, `diff` and the loop print most often.
    #[test]
    fn one_rename_is_counted_as_one() {
        let renamed = |names: &[(&str, &str)]| {
            describe(
                &report_with(
                    Some(list(
                        Basis::Compare {
                            before: 1,
                            after: 2,
                        },
                        ListDiff::default(),
                        Some(1_000),
                    )),
                    names
                        .iter()
                        .enumerate()
                        .map(|(n, (from, to))| Rename {
                            pk: n as Pk,
                            history_id: n as i64,
                            from: (*from).into(),
                            to: (*to).into(),
                            at: 1_500,
                        })
                        .collect(),
                ),
                false,
            )
            .join("\n")
        };

        let one = renamed(&[("before", "after")]);
        assert!(one.contains("1 now goes by another name"), "{one}");

        let two = renamed(&[("before", "after"), ("other", "later")]);
        assert!(two.contains("2 now go by another name"), "{two}");
    }

    /// The monitor's own sentence about its gaps reads as a sentence for one.
    ///
    /// One format string with no branch and `1` an ordinary value in it: "1
    /// scheduled runs were missed while this was not running. They are reported
    /// as one." Ungrammatical, and self-contradictory in the case it prints most
    /// often -- a laptop shut overnight on a daily schedule misses exactly one,
    /// and `missed_since` takes one off the end because the moment being served
    /// is itself in the past.
    #[test]
    fn the_missed_warning_reads_as_a_sentence_for_one() {
        let one = missed_warning(1);
        assert!(one.contains("1 scheduled run was missed"), "{one}");
        assert!(!one.contains("runs were"), "{one}");
        assert!(!one.contains("They are"), "{one}");

        let several = missed_warning(4);
        assert!(
            several.contains("4 scheduled runs were missed"),
            "{several}"
        );

        // Both have to say the part that matters, which is that they are not
        // replayed: firing twelve to catch up is the burst the pacing exists to
        // prevent, and they would all report the same present state anyway.
        for text in [one, several] {
            assert!(text.contains("nothing to catch up on"), "{text}");
        }
    }

    /// The tokens are what a caller branches on, so they are asserted rather
    /// than left to whatever the enum happens to be called.
    #[test]
    fn the_json_carries_stable_tokens_for_each_basis() {
        for (basis, token) in [
            (Basis::Baseline { snapshot_id: 1 }, "baseline"),
            (Basis::Unchanged { snapshot_id: 1 }, "unchanged"),
            (
                Basis::Compare {
                    before: 1,
                    after: 2,
                },
                "compared",
            ),
        ] {
            assert_eq!(basis_token(basis), token);
        }
    }

    /// The README publishes the body, so it has to be the body.
    ///
    /// The example was the **stdout** shape minus its lists: `run` carried
    /// `looked` and `requests` and neither `id` nor `at`. Those are the two
    /// fields the surrounding prose depends on — the id is what "queued and
    /// retried", at-least-once and `X-Snob-Delivery` are all about, and the
    /// moment is the only timestamp in the message — so a receiver written from
    /// the document had neither. `counts` showed three of six keys, and nothing
    /// marked the object as abbreviated.
    ///
    /// It compares keys and not values, because the block is an example and
    /// abbreviates the arrays. What it may not do is name a key the payload does
    /// not emit, or leave one out of `run` — which is the object the drift
    /// happened in.
    ///
    /// The version inside `tool` is deliberately not asserted: doing that makes
    /// every release a README edit.
    #[test]
    fn the_readme_publishes_the_body_that_goes_out() {
        // Walks up from this crate until a `Cargo.lock` shows up, the way the
        // source-reading guards in `snob-core` do, and answers nothing from a
        // packaged build where there is no repository to read.
        let Some(root) = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|d| d.join("Cargo.lock").is_file())
        else {
            return;
        };

        let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
        assert_eq!(
            readme.matches("```json").count(),
            1,
            "the payload is the only JSON block, and this test takes the first"
        );
        let block = readme
            .split("```json")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("the README shows the payload");
        let published: serde_json::Value =
            serde_json::from_str(block).expect("the README's example has to be JSON");

        let real = payload(
            &TickReport::for_test(report_with(None, vec![]), 14, 1_700_000_000),
            "run-1",
            "watch.changes",
        );

        let keys = |value: &serde_json::Value| {
            let mut names: Vec<String> = value
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();
            names
        };
        assert_eq!(
            keys(&published["run"]),
            keys(&real["run"]),
            "the `run` object is published in full or not at all"
        );

        // One direction only: the example abbreviates, and it is allowed to.
        // What it may not do is publish a field nothing sends.
        fn only_real_keys(published: &serde_json::Value, real: &serde_json::Value, path: &str) {
            let (Some(published), Some(real)) = (published.as_object(), real.as_object()) else {
                return;
            };
            for (key, value) in published {
                let here = format!("{path}.{key}");
                let counterpart = real.get(key).unwrap_or_else(|| {
                    panic!("the README publishes \"{here}\", which is not sent")
                });
                only_real_keys(value, counterpart, &here);
            }
        }
        only_real_keys(&published, &real, "");
    }

    /// Every message this tool emits can be version-checked, and the two that
    /// go to a receiver put the id and the moment in the same place.
    ///
    /// Three emitters exist and they were three literal `1`s and, for a while,
    /// two shapes. The preflight was the only body with no `schema` at all —
    /// the one message a receiver cannot version-check — with `run_id` and `at`
    /// at the top level while the other two carried them under `run`;
    /// `webhook_of`'s doc claimed the opposite in as many words.
    ///
    /// The `--json` stream was the third, and it had no version either. The
    /// README's own recipe is `snob watch --json >> events.ndjson`, which makes
    /// that file a data feed with readers of its own, so a receiver could
    /// version-check a webhook body and a preflight and not the file it was told
    /// to append to. It carries no `run.id`, and that is right rather than an
    /// omission: an id exists to deduplicate an at-least-once delivery, and a
    /// line written once to a local file is not one.
    #[test]
    fn every_message_carries_the_schema() {
        let report = payload(
            &TickReport::for_test(report_with(None, vec![]), 0, 1_700_000_000),
            "run-1",
            "watch.changes",
        );
        let preflight = preflight_body("run-2", 1_700_000_000);
        let streamed = tick_json(&TickReport::for_test(
            report_with(None, vec![]),
            0,
            1_700_000_000,
        ));

        for (which, message) in [
            ("report", &report),
            ("preflight", &preflight),
            ("stream line", &streamed),
        ] {
            assert_eq!(message["schema"], 1, "{which} cannot be version-checked");
            assert_eq!(
                message["run"]["at"], 1_700_000_000,
                "{which} does not say when"
            );
            assert!(
                message.get("at").is_none(),
                "{which} still has the moment at the top level"
            );
        }

        for (which, message) in [("report", &report), ("preflight", &preflight)] {
            assert!(
                message["event"].as_str().is_some(),
                "{which} has nothing for a Switch node to read"
            );
            assert!(
                message["run"]["id"].as_str().is_some(),
                "{which} does not say which run it is"
            );
            assert!(
                message.get("run_id").is_none(),
                "{which} still has the id at the top level"
            );
        }

        // And the name is one constant, so the header and the body cannot come
        // to disagree the way they did over heartbeats.
        assert_eq!(preflight["event"], crate::engine::check::PREFLIGHT_EVENT);
    }

    /// A refused list is not a quiet one, and the body has to say which it was.
    ///
    /// `tick` drops a list it could not verify before the comparison, so it
    /// reaches `payload` as `None` and `list_json` turns it into `null` — the
    /// same `null` an account with no capture of that list produces, with
    /// `counts.following_lost` at `0` in both. The two bodies were byte
    /// identical, so `{{ $json.counts.following_lost > 0 }}` routed a run that
    /// could not see to "nothing happened" for as long as the wall lasted, and
    /// the only place the refusal was said out loud was standard error.
    ///
    /// `run.looked` cannot resolve it, and it is asserted equal here to say so:
    /// it is `any`, not `all`, so a run that read followers and was refused
    /// following reports `true`. Which list is the question.
    #[test]
    fn a_refused_list_is_not_reported_as_a_quiet_one() {
        let body = |skipped| {
            let mut tick = TickReport::for_test(
                report_with(
                    Some(list(
                        Basis::Compare {
                            before: 1,
                            after: 2,
                        },
                        ListDiff {
                            gained: vec![user(1, "arrived")],
                            lost: vec![],
                        },
                        Some(1_000),
                    )),
                    vec![],
                ),
                7,
                1_700_000_000,
            );
            tick.lists = vec![
                TickList {
                    kind: ListKind::Followers,
                    skipped: None,
                },
                TickList {
                    kind: ListKind::Following,
                    skipped,
                },
            ];
            payload(&tick, "run-1", "watch.changes")
        };

        let refused = body(Some(Skipped::NobodyLooked(Provenance::PollFailed)));
        let read = body(None);

        // Everything a receiver had to go on before, and it is the same in
        // both: the arrays cannot tell them apart and neither can the counts.
        assert_eq!(refused["lists"], read["lists"]);
        assert_eq!(refused["counts"], read["counts"]);
        assert_eq!(
            refused["run"]["looked"], read["run"]["looked"],
            "`looked` is `any`, so it says `true` for both"
        );

        assert_ne!(
            refused["run"]["lists"], read["run"]["lists"],
            "a receiver has no field to read the refusal from"
        );
        assert_eq!(refused["run"]["lists"][1]["kind"], "following");
        assert_eq!(refused["run"]["lists"][1]["skipped"], "not_verified");
        assert_eq!(
            read["run"]["lists"][1]["skipped"],
            serde_json::Value::Null,
            "a list that was read carries no refusal"
        );
    }

    /// An event line says when it happened.
    ///
    /// The README puts `snob watch --json >> events.ndjson` forward as a
    /// complete way to use the tool, and the line carried no time of its own.
    /// The only epoch fields belonged to the lists — the mark's moment and the
    /// capture's — and they go away with the list when it is refused. One 429
    /// opens a cooldown, both lists are served from storage for the next half
    /// hour, and every line in that window is byte-identical while the wire
    /// bodies for the same ticks differ at `run.at`. A file like that cannot be
    /// queried by time, windowed or deduplicated.
    #[test]
    fn an_event_line_says_when_it_happened() {
        let quiet_run_at = |at| {
            let mut tick = TickReport::for_test(report_with(None, vec![]), 0, at);
            tick.lists = vec![TickList {
                kind: ListKind::Followers,
                skipped: Some(Skipped::NobodyLooked(Provenance::Cooldown)),
            }];
            tick_json(&tick)
        };

        let first = quiet_run_at(1_700_000_000);
        let second = quiet_run_at(1_700_021_600);

        assert_eq!(first["run"]["at"], 1_700_000_000);
        assert_ne!(
            first, second,
            "six hours apart and the same bytes: nothing in the file can date a run"
        );

        // The moment is the tick's own, so the file and whatever the webhook
        // delivered can be joined on it.
        let tick = TickReport::for_test(report_with(None, vec![]), 0, 1_700_000_000);
        assert_eq!(
            tick_json(&tick)["run"]["at"],
            payload(&tick, "run-1", "watch.changes")["run"]["at"],
            "one event, one moment"
        );
    }

    /// A failed tick leaves a line in the stream.
    ///
    /// The JSON line is written after the `?` in `tick_one`, so a tick that
    /// failed printed nothing on standard output at all — @friend goes private,
    /// or DNS goes away for a named target, and `events.ndjson` simply has no
    /// line for that interval. The failure went to standard error as an English
    /// `error:` / `caused by:` / `hint:` paragraph, which is the one shape a
    /// consumer of the file is not reading.
    ///
    /// The line has to be readable by the same reader the successful lines
    /// have, which is why `run` is the same object. `error` is what tells them
    /// apart, and a successful line must not have one.
    #[test]
    fn a_failed_tick_leaves_a_line_in_the_stream() {
        let error = anyhow::anyhow!("@friend's account is private");
        let line = failed_tick_json(
            &Watched::consented("friend".into(), crate::engine::watch::Consent),
            &error,
            1_700_000_000,
            1,
        );

        assert_eq!(
            line["run"]["at"], 1_700_000_000,
            "the gap in the file has to be datable, which is the whole of it"
        );
        assert_eq!(line["run"]["looked"], false);
        assert_eq!(line["run"]["requests"], 1, "the poll was charged");
        assert_eq!(line["account"]["username"], "friend");
        assert_eq!(
            line["error"]["code"], "error",
            "the vocabulary of the exit table, not free text"
        );

        // And the two kinds of line are told apart by the field itself, not by
        // what is missing from the rest of the object.
        let ok = tick_json(&TickReport::for_test(
            report_with(None, vec![]),
            0,
            1_700_000_000,
        ));
        assert!(
            ok.get("error").is_none(),
            "a run that worked must not look like one that failed"
        );

        // One object, one line, in both modes. `run_accounts` prints one per
        // account with nothing wrapping them, so a line laid out over several
        // of them stops parsing the moment a second account is watched.
        assert!(!json_line(&line).contains('\n'));
        assert!(
            !json_line(&ok).contains('\n'),
            "the successful line is a line too"
        );
    }

    /// The shape a stranger's automation branches on, pinned to a literal.
    ///
    /// This is the one output of the tool that somebody else's workflow reads,
    /// and a key renamed by accident breaks something built months ago with no
    /// error anywhere. Comparing against a literal means a change to the
    /// contract has to be a change somebody made on purpose.
    ///
    /// Both of the fields that move in production are arguments here: `run.id`
    /// is random and `run.at` is the tick's own moment, so the literal can
    /// carry them rather than the comparison having to skip them.
    #[test]
    fn the_payload_has_the_shape_a_receiver_was_promised() {
        let tick = TickReport::for_test(
            report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff {
                        gained: vec![user(1, "arrived")],
                        lost: vec![user(2, "left")],
                    },
                    Some(1_000),
                )),
                vec![Rename {
                    pk: 7,
                    history_id: 7,
                    from: "before".into(),
                    to: "after".into(),
                    at: 1_500,
                }],
            ),
            14,
            1_700_000_000,
        );

        let payload = payload(&tick, "run-1", "watch.changes");

        assert_eq!(
            payload,
            serde_json::json!({
                "schema": 1,
                "event": "watch.changes",
                "run": {
                    "id": "run-1",
                    "at": 1_700_000_000,
                    "looked": false,
                    "requests": 14,
                    "lists": [],
                    "tool": { "name": "snob", "version": env!("CARGO_PKG_VERSION") },
                },
                "account": { "pk": 42, "username": "me", "is_self": true },
                "lists": {
                    "followers": {
                        "basis": "compared",
                        "since": 1_000,
                        "until": 2_000,
                        "count": 10,
                    },
                    "following": null,
                },
                "counts": {
                    "followers_gained": 1,
                    "followers_lost": 1,
                    "following_gained": 0,
                    "following_lost": 0,
                    "renamed": 1,
                    "total": 3,
                },
                "events": {
                    "followers_gained": [{
                        "pk": 1,
                        "username": "arrived",
                        "profile_url": "https://www.instagram.com/arrived/",
                    }],
                    "followers_lost": [{
                        "pk": 2,
                        "username": "left",
                        "profile_url": "https://www.instagram.com/left/",
                    }],
                    "following_gained": [],
                    "following_lost": [],
                    "renamed": [{
                        "pk": 7,
                        "from": "before",
                        "to": "after",
                        "changed_at": 1_500,
                    }],
                },
            })
        );
    }

    /// A control character in a name reaches a terminal through this command
    /// like any other, so it goes through the same filter.
    #[test]
    fn a_name_is_filtered_before_it_is_drawn() {
        let lines = describe(
            &report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff {
                        gained: vec![user(1, "bad\u{202e}name")],
                        lost: vec![],
                    },
                    Some(1_000),
                )),
                vec![],
            ),
            false,
        );
        assert!(
            !lines.join("\n").contains('\u{202e}'),
            "a bidi override reached the terminal"
        );
    }
}
