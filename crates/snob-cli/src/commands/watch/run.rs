//! One run of the monitor: the accounts it covers, and the three things that
//! happen once around them.
//!
//! Both modes come through here — `super::scheduled` on a timer and
//! `super::once` by hand — so AGENTS.md's rule that the queue is drained once
//! per run, after every account, is one rule in one place rather than two
//! copies that drift.

use anyhow::Result;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::WatchRunArgs;
use crate::commands::common::{self, Session};
use crate::engine::watch::{TickReport, Watched};
use crate::exit::ExitCode;
use crate::report;
use crate::ui;

use super::delivery::{Delivery, deliver, drain};
use super::say::{describe, refusal_line, say_what_was_given_up};
use super::wire::{failed_tick_json, json_line, tick_json};

/// One run inside the loop: open, tick, print, close.
///
/// A fresh `App` per run rather than one held open for weeks. It picks up a
/// session that was replaced and a refreshed User-Agent, and — the reason that
/// matters on Windows — it holds no SQLite connection while the loop sleeps, so
/// `snob purge` in another terminal is not blocked by a file this has open.
pub(super) async fn open_and_run(
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
            snob_core::clock::now(),
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
pub(super) async fn run_one(
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
pub(super) struct Printing {
    json: bool,
    watching: bool,
}

impl Printing {
    /// How a failure is told, decided by the same flag as the result.
    pub(super) fn wording(self) -> report::Wording {
        if self.json {
            report::Wording::Json
        } else {
            report::Wording::Prose
        }
    }

    pub(super) fn unattended(json: bool) -> Self {
        Self {
            json,
            watching: false,
        }
    }

    pub(super) fn watched(json: bool) -> Self {
        Self {
            json,
            watching: true,
        }
    }
}

/// What a run over several accounts came to.
///
/// Not `snob_core::watch::RunOutcome`, which is what *one* run of *one* account
/// came to and is the vocabulary `watch_runs.outcome` is kept in. This is the
/// summary of a whole pass over the file: the requests it spent, the first
/// reason any of its ticks gave, and the one failure nobody has printed yet.
pub(super) struct RunSummary {
    /// Requests spent by every account that got as far as spending any.
    pub(super) spent: u32,
    /// The first non-`Ok` verdict a tick reported, the accounts being in the
    /// order the file names them. [`first_reason`] is where the choice between
    /// first and worst is made, and why there is no worst to choose.
    pub(super) code: ExitCode,
    /// The last failure, unprinted. Every earlier one has already been printed,
    /// because only one can be handed back and the caller prints the one it
    /// gets — `scheduled` so the service keeps running, `main` so `once` exits
    /// with the right code. Printing here *and* returning is what wrote the
    /// whole `error:` / `caused by:` / `hint:` block twice per failing run.
    pub(super) failed: Option<anyhow::Error>,
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
pub(super) async fn run_accounts(
    app: &mut crate::app::App,
    watched: &[Watched],
    delivery: Option<&Delivery>,
    printing: Printing,
) -> RunSummary {
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
                let at = snob_core::clock::now();
                record_failed_run(app, account, &e, charged, at);
                if printing.json {
                    // The stream gets a line for this interval too. Written
                    // after the `?` in `tick_one`, there was none: the failure
                    // went to standard error as prose and `events.ndjson` had
                    // nothing at all for that run.
                    crate::ui::say!("{}", json_line(&failed_tick_json(account, &e, at, charged)));
                }
                failures.push(e);
            }
        }
    }

    // Once, after every account, and whatever the accounts did. Whatever is
    // owed from earlier runs goes out here — including when no account had news
    // of its own, which is the common case and the one that used to leave the
    // queue untouched. A machine whose hourly run failed all day left rows
    // aging past `MAX_AGE_SECS`, where `due` no longer returns them and
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
        snob_core::clock::now(),
    ));

    let (print_here, failed) = to_print_and_to_return(failures);
    for earlier in print_here {
        report::print_error(&earlier, printing.wording());
    }

    RunSummary {
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
        Some(name) => snob_store::store::accounts::find_pk_by_username(
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
    let record = snob_store::store::watch::record_run(
        app.db().conn(),
        &snob_store::store::watch::Run {
            account_pk,
            started_at: at,
            finished_at: Some(at),
            requests,
            outcome: Some(outcome.into()),
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
/// `RunSummary::failed`, and `once` returns this only when nothing failed.
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
        crate::ui::say!("{}", json_line(&tick_json(&tick)));
    } else if printing.watching || !tick.report.changes().is_empty() {
        for line in describe(&tick.report, tick.lists.iter().any(|l| l.skipped.is_some())) {
            crate::ui::say!("{line}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::fixtures::{app_posting_to, owed_long_ago, user};
    use snob_store::store::deliveries;

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
        snob_store::store::users::upsert(app.db().conn(), &user(99, "friend")).unwrap();
        snob_store::store::accounts::upsert(app.db().conn(), 99, false).unwrap();

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

        let runs = snob_store::store::watch::last_runs(app.db().conn()).unwrap();
        let recorded = runs
            .iter()
            .find(|r| r.account_pk == 99)
            .expect("a tick that failed is a tick that happened");
        assert_ne!(
            recorded.outcome,
            Some(ExitCode::Ok.into()),
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
        let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
        let now = snob_core::clock::now();
        let id = owed_long_ago(&paths, now);

        let secrets = snob_store::secrets::SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-settle-{}", std::process::id()));
        let args = WatchRunArgs {
            no_progress: true,
            ..WatchRunArgs::default()
        };

        open_and_run(&args, &[], None, &secrets, &paths)
            .await
            .expect("no session is not a failure; the loop keeps going");

        let db = snob_store::store::Store::open(&paths).unwrap();
        assert_eq!(
            deliveries::state(db.conn(), id).unwrap().as_deref(),
            Some("expired"),
            "the run had no session, and retention still has to happen"
        );
    }
}
