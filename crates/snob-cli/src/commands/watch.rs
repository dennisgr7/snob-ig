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

use anyhow::{Context, Result, bail};
use snob_core::Pk;
use snob_core::model::{ListKind, User, printable};
use snob_core::paths::AppPaths;
use snob_core::secret::Secret;
use snob_core::secrets::{Kind, SecretStore};
use snob_core::store::deliveries;
use snob_core::watch::config::{self, WatchConfig, WebhookConfig};
use snob_core::watch::schedule::{self, Due, Schedule, Weekday};
use snob_core::watch::{Basis, Changes, ListDiff, Rename};
use url::Url;

use crate::cli::{
    WatchArgs, WatchCommand, WatchDiffArgs, WatchOnceArgs, WatchRunArgs, WebhookArgs,
};
use crate::commands::common::{self, Session};
use crate::engine::Provenance;
use crate::engine::watch::{ListReport, Queued, Skipped, TickReport, WatchReport, Watched};
use crate::exit::{ExitCode, ExitError};
use crate::report;
use crate::ui;
use crate::watch::webhook::{self, Attempt, Webhook, WebhookClient};

pub async fn run(args: WatchArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    match args.command {
        Some(WatchCommand::Diff(args)) => diff(args, secrets, paths),
        Some(WatchCommand::Once(args)) => once(args, secrets, paths).await,
        Some(WatchCommand::Setup(args)) => super::watch_setup::setup(args, secrets, paths),
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
        let name = unallowed.name().unwrap_or_default();
        return Err(ExitError::new(
            ExitCode::Interrupted,
            format!(
                "reading @{}'s lists needs confirmation, and a scheduled run has nobody to \
                 ask.\nRun \"snob watch setup\" to answer it once, or \"snob watch once {name}\" \
                 while you are here.",
                printable(name),
            ),
        )
        .into());
    }

    ui::info(&format!(
        "Watching {}. {} Stop with Ctrl+C.",
        watching_label(&watched),
        describe_schedule(&when_from(&args, configured.as_ref()), &schedule, args.now),
    ));

    // Installed once for the process, which is what lets this open an `App` per
    // run without leaving a signal listener behind on each one.
    let cancel = crate::interrupt::install();

    // Seeded from the run log, not from this process's start.
    //
    // The clock used to begin again on every start, so `--every 24h` on a
    // machine that is powered on from eight to six, or under a supervisor with
    // `Restart=always` restarting more often than the interval, never reached
    // its first run at all — while `snob watch status`, reading the very same
    // table, said "It has not run yet."
    //
    // `Some(now)` when nothing has ever run: a fresh install waits for its first
    // scheduled moment rather than walking the moment it is set up. `--now` is
    // what asks for a run at start, and it is spent below.
    let mut last_run = last_started(paths)?.or_else(|| Some(snob_core::store::now()));

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
                    Due::At(i64::MAX) => {
                        return Err(anyhow::anyhow!(
                            "this schedule can never come round: nothing matches it"
                        ));
                    }
                    // Rolled once per due moment. The roll is made here rather
                    // than inside `with_jitter` so that function reads no
                    // randomness and its bounds stay testable.
                    Due::At(at) => (
                        schedule::with_jitter(at, schedule.jitter(), fastrand::f64()),
                        0,
                    ),
                };
                waiting_for = Some(pending);
                pending
            }
        };

        if now >= wake_at {
            if missed > 0 {
                ui::warn(&format!(
                    "{missed} scheduled runs were missed while this was not running. They are \
                     reported as one: there is only one present state, so there is nothing to \
                     catch up on"
                ));
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
        return Ok(());
    };

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
    // One `App` for all of them, unlike one per run: they share a session and a
    // request budget, and opening a second would be a second connection to the
    // same database for no reason. A failure on one account does not stop the
    // rest — a private account somebody stopped being allowed to read must not
    // silence the monitor's own.
    let mut failed = None;
    for account in watched {
        if let Err(e) = tick_one(args, app, account, delivery).await {
            report::print_error(&e);
            failed = Some(e);
        }
    }

    // Once, after every account. Whatever is owed from earlier runs goes out
    // here — including when no account had news of its own, which is the common
    // case and the one that used to leave the queue untouched.
    if let Some(delivery) = delivery {
        drain(app, delivery).await;
    }
    // And once whatever the accounts did, for the reason on `settle`.
    crate::engine::watch::settle(app, snob_core::store::now());

    match failed {
        Some(e) if watched.len() == 1 => Err(e),
        // Already printed, and the run as a whole did something.
        _ => Ok(()),
    }
}

/// One account, inside a run that may cover several.
async fn tick_one(
    args: &WatchRunArgs,
    app: &mut crate::app::App,
    watched: &Watched,
    delivery: Option<&Delivery>,
) -> Result<()> {
    let tick = crate::engine::watch::tick(app, watched).await;
    app.progress().finish();
    let tick = tick?;

    // One line per run down a pipe, so `snob watch >> events.ndjson` is a
    // complete way to use this without a webhook.
    if args.json {
        println!("{}", serde_json::to_string(&tick_json(&tick))?);
    } else if !tick.report.changes().is_empty() {
        for line in describe(&tick.report, tick.lists.iter().any(|l| l.skipped.is_some())) {
            println!("{line}");
        }
    }

    warn_about_refusals(&tick);

    deliver(app, &tick, delivery).await
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
fn watched_from(target: Option<String>, configured: Option<&WatchConfig>) -> Vec<Watched> {
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
fn with_recorded_consent(name: &str, configured: Option<&WatchConfig>) -> Watched {
    let recorded = configured
        .into_iter()
        .flat_map(|c| c.accounts.iter())
        .find(|account| !account.is_own() && account.target.eq_ignore_ascii_case(name))
        .and_then(|account| account.consent);

    match recorded {
        Some(consent) => Watched::consented(
            name.to_string(),
            crate::engine::watch::Consent {
                given_at: consent.agreed_at,
            },
        ),
        None => Watched::asking(name.to_string()),
    }
}

fn target_label(target: Option<&str>) -> String {
    match target {
        Some(name) => format!("@{}", printable(name)),
        None => "your account".to_string(),
    }
}

/// What the opening line names, so somebody starting the service can see that
/// it understood which accounts it is for.
fn watching_label(watched: &[Watched]) -> String {
    let names: Vec<String> = watched.iter().map(|w| target_label(w.name())).collect();

    match names.len() {
        0 => "nothing".to_string(),
        1 => names[0].clone(),
        _ => {
            let (last, rest) = names.split_last().expect("more than one");
            format!("{} and {last}", rest.join(", "))
        }
    }
}

/// Builds the schedule from the flags, or the file, or explains what is
/// missing.
///
/// **A flag replaces the schedule rather than merging with it.** Half from the
/// file and half from the command line is a schedule nobody can read back: the
/// only honest reading of `--every 6h` against a configured `--on mon` is the
/// one the person typing meant, and there is no way to know which.
/// The schedule as written, before it is built: whichever of the two sources
/// won, in the words it was given in.
///
/// The flags win as a set rather than field by field, because a schedule half
/// from the file and half from the command line is one nobody can read.
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
fn when_from(args: &WatchRunArgs, configured: Option<&WatchConfig>) -> When {
    let given =
        args.cron.is_some() || !args.at.is_empty() || !args.on.is_empty() || args.every.is_some();

    if given {
        return When {
            cron: args.cron.clone(),
            at: args.at.clone(),
            on: args.on.clone(),
            every: args.every,
            jitter: args.jitter,
        };
    }
    match configured {
        Some(c) => When {
            cron: c.cron.clone(),
            at: c.at.clone(),
            on: c.on.clone(),
            every: c.every,
            jitter: c.jitter,
        },
        None => When {
            cron: None,
            at: vec![],
            on: vec![],
            every: None,
            jitter: None,
        },
    }
}

fn schedule_from(args: &WatchRunArgs, configured: Option<&WatchConfig>) -> Result<Schedule> {
    let When {
        cron,
        at,
        on,
        every,
        jitter,
    } = when_from(args, configured);

    let mut schedule = if let Some(expression) = &cron {
        Schedule::cron(expression)?
    } else if !at.is_empty() || !on.is_empty() {
        let days = on
            .iter()
            .map(|d| {
                Weekday::parse(d)
                    .ok_or_else(|| anyhow::anyhow!("\"{d}\" is not a day (try mon, thu)"))
            })
            .collect::<Result<Vec<_>>>()?;
        let times = at
            .iter()
            .map(|t| schedule::parse_time(t).map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>>>()?;
        Schedule::calendar(&days, &times)?
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
    // `--every 2w --on mon` is "one Monday in every two weeks".
    if let Some(every) = every
        && (cron.is_some() || !at.is_empty() || !on.is_empty())
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
    let mut parts = Vec::new();
    if let Some(every) = when.every {
        parts.push(format!("every {}", snob_core::duration::format(every)));
    }
    if !when.on.is_empty() {
        parts.push(format!("on {}", printable(&when.on.join(", "))));
    }
    if !when.at.is_empty() {
        parts.push(format!("at {}", printable(&when.at.join(", "))));
    }
    if let Some(cron) = &when.cron {
        parts.push(format!("on the schedule \"{}\"", printable(cron)));
    }

    let mut line = format!("Running {}.", parts.join(", "));

    let jitter = schedule.jitter();
    if !jitter.is_zero() {
        line.push_str(&format!(
            " Each run is pushed up to {} later, so it does not land on the same second every \
             time.",
            snob_core::duration::format(jitter)
        ));
    }
    if now {
        line.push_str(" Starting with one now.");
    }
    line
}

/// One run of the monitor: look, report, and remember having reported.
async fn once(args: WatchOnceArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    // Before the session is opened and long before a request is spent, so a
    // webhook address that could never work costs nothing to find out about.
    // The file is read here too: `once` on a timer should need no more
    // arguments than the scheduled mode does.
    let delivery = delivery_from(&args.delivery, config::load(paths)?.as_ref(), &secrets)?;

    let Session::Open(mut app) = common::open_with_progress(!args.no_progress, &secrets, paths)?
    else {
        return Ok(ExitCode::NoSession);
    };

    let watched = match args.target.clone() {
        None => Watched::own(),
        // Asked, not assumed. `engine::list` puts the question the same way it
        // does for every other command, and with no terminal to ask at it
        // refuses — which is the right answer for a cron entry aimed at
        // somebody else's account and no recorded agreement.
        Some(name) => Watched::asking(name),
    };

    let tick = crate::engine::watch::tick(&mut app, &watched).await;
    app.progress().finish();
    let tick = tick?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&tick_json(&tick))?);
    } else {
        for line in describe(&tick.report, tick.lists.iter().any(|l| l.skipped.is_some())) {
            println!("{line}");
        }
    }

    warn_about_refusals(&tick);

    deliver(&mut app, &tick, delivery.as_ref()).await?;
    if let Some(delivery) = delivery.as_ref() {
        drain(&app, delivery).await;
    }
    crate::engine::watch::settle(&app, snob_core::store::now());

    ui::info(&format!(
        "{} - {}",
        report::stored_on(snob_core::store::now()),
        report::requests(tick.requests)
    ));

    // The run's own verdict, which was computed, written into `watch_runs`, and
    // then thrown away in favor of a literal zero. The exit codes exist so a
    // caller on a timer can tell "wait" from "log in again" without parsing
    // text, and `once` is the mode that is put on a timer.
    Ok(tick.outcome())
}

/// Where reports go, once the arguments have been checked.
struct Delivery {
    client: WebhookClient,
    heartbeat: bool,
    /// The origin this run posts to. Written onto every report it queues and used
    /// to filter the outbox, so a queued report can only ever be sent to the
    /// address it was addressed to.
    destination: String,
}

/// What this run would send, worked out without touching the keyring or the
/// network.
///
/// Split out of [`delivery_from`] so the four rules it holds can be reached by a
/// test at all. They could not be: that function takes a `&SecretStore` and
/// hands back a live HTTP client, so nothing in the suite could ask it a
/// question -- and replacing the origin comparison below with `true` left all
/// 656 tests green, on the guard whose own comment records a team's `X-Api-Key`
/// and the stored bearer token arriving at `webhook.site`.
struct Planned {
    webhook: Webhook,
    heartbeat: bool,
}

/// Decides what to send, and what to say about it.
///
/// **The warnings come back rather than being printed**, because a decision that
/// prints is a decision that cannot be checked. They are in the order they have
/// to be said: either the no-webhook one on its own, or the withheld-headers one
/// and then the withheld-token one.
///
/// The two stored secrets are read by the caller rather than in here, which is
/// what makes this reachable without a keyring. One consequence, written down
/// rather than left to be found: a machine whose keyring is broken now reports
/// the keyring's error where it used to report the clearer one about an empty
/// `--sign-with`. The session opened moments later would have reported it anyway.
fn plan(
    args: &WebhookArgs,
    from_file: Option<&WebhookConfig>,
    stored_token: Option<Secret>,
    stored_key: Option<Secret>,
) -> Result<(Option<Planned>, Vec<String>)> {
    let mut warnings: Vec<String> = Vec::new();

    let Some(url) = args
        .webhook
        .clone()
        .or_else(|| from_file.map(|w| w.url.clone()))
    else {
        if !args.header.is_empty() || args.sign_with.is_some() || args.heartbeat {
            warnings.push(
                "there is no webhook, so nothing is sent and those options do nothing".to_string(),
            );
        }
        return Ok((None, warnings));
    };

    let url = Url::parse(&url).with_context(|| format!("\"{url}\" is not an address"))?;

    // Whether this run is posting to the address the configuration was written
    // for.
    //
    // Nothing used to ask. `--webhook` replaced the URL and the file's headers
    // and the keyring token came along regardless, so
    // `snob watch once --webhook https://webhook.site/<id>` -- which is precisely
    // what somebody does to see what the payload looks like -- sent the team's
    // `X-Api-Key` and the bearer token `setup` had stored to a host nobody had
    // configured. Not malice, debugging. `check` was happy because the new
    // address was https.
    //
    // Compared by origin rather than by string, so a path or a query on the same
    // host is still the same destination. The project already has the pattern:
    // `IgClient::check_downloadable` exists so the CDN cannot be handed a
    // credential meant for somewhere else.
    let configured_origin = from_file
        .and_then(|w| Url::parse(&w.url).ok())
        .map(|configured| configured.origin());
    let same_destination = configured_origin
        .as_ref()
        .is_none_or(|origin| *origin == url.origin());

    // Headers from the file first, then the ones typed, so a flag can override
    // a configured one of the same name -- the last one wins at the request.
    let mut headers: Vec<(String, String)> = from_file
        .filter(|_| same_destination)
        .map(|w| {
            w.headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    if !same_destination && from_file.is_some_and(|w| !w.headers.is_empty()) {
        warnings.push(format!(
            "{url} is not the address in the configuration, so the headers configured there are \
             not sent with it. Pass what this one needs with --header."
        ));
    }

    for raw in &args.header {
        let (name, value) = raw.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("\"{raw}\" is not a header; write it as \"Name: value\"")
        })?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    // Stored secrets fill in what was not passed. This is the whole point of
    // `setup` having put them in the keyring: a systemd unit runs `snob watch`
    // with no arguments and the token is not in the unit file, the process
    // table, or anybody's shell history.
    //
    // The guard looks at the **merged** list, not only at the flags. Inspecting
    // `args.header` alone meant a configured `Authorization` and a stored token
    // both went out -- and the second one was the secret `setup` had put away.
    //
    // And only to the address it was stored for. A token is a credential like
    // the session cookie, and the one guard this module is arranged around -- the
    // client cannot carry the session -- said nothing about this one.
    let authorization_given = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
    if !authorization_given && let Some(token) = stored_token {
        if same_destination {
            headers.push(("Authorization".to_string(), token.expose().to_string()));
        } else {
            warnings.push(format!(
                "the stored token was set up for {}, so it is not sent to {url}. Pass one with \
                 --header \"Authorization: ...\" if this address needs it.",
                configured_origin
                    .as_ref()
                    .map(|o| o.ascii_serialization())
                    .unwrap_or_default(),
            ));
        }
    }

    let key = match args.sign_with.clone() {
        // An empty `--sign-with` is not a request to sign with nothing: it is
        // `--sign-with ${SNOB_KEY}` in a unit file where the variable is unset.
        // Taken literally it signs with a zero-length key -- a well-formed
        // signature anybody can forge -- and, because this arm wins over the
        // keyring, it would silently replace a key that was configured
        // correctly. `ask_webhook` already refuses an empty one; the flag has
        // to agree with it.
        Some(given) if given.trim().is_empty() => bail!(
            "--sign-with was given an empty value. If that came from an environment variable \
             that is not set, leave the flag out: the key stored by \"snob watch setup\" is \
             used when it is absent."
        ),
        Some(given) => Some(Secret::from(given)),
        None => stored_key,
    };

    Ok((
        Some(Planned {
            webhook: Webhook { url, headers, key },
            heartbeat: args.heartbeat || from_file.is_some_and(|w| w.heartbeat),
        }),
        warnings,
    ))
}

/// Reads the webhook arguments, or explains what is wrong with them.
///
/// Called before anything is opened or spent. A run that would have shouted a
/// token over plain HTTP fails while somebody is still there to read the
/// message, rather than six hours later into a log.
fn delivery_from(
    args: &WebhookArgs,
    configured: Option<&WatchConfig>,
    secrets: &SecretStore,
) -> Result<Option<Delivery>> {
    let (planned, warnings) = plan(
        args,
        configured.and_then(|c| c.webhook.as_ref()),
        secrets.load_secret(Kind::WatchToken)?,
        secrets.load_secret(Kind::WatchSigningKey)?,
    )?;

    for warning in &warnings {
        ui::warn(warning);
    }

    let Some(planned) = planned else {
        return Ok(None);
    };
    webhook::check(&planned.webhook)?;

    Ok(Some(Delivery {
        // The origin this run posts to, kept beside the client so the outbox can
        // be filtered by it: a queued report belongs to the address it was
        // addressed to, and `--webhook` must not flush a backlog somewhere else.
        destination: destination_of(&planned.webhook.url),
        client: WebhookClient::new(planned.webhook)?,
        heartbeat: planned.heartbeat,
    }))
}

/// The address a report is addressed to, as the outbox records it.
///
/// The origin, not the whole URL: a path that changed is the same destination
/// and the same credential, where a host that changed is neither.
fn destination_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Commits the report and gets it to the webhook, if there is one.
///
/// The order is the whole of it: the report is queued and the marks retire in
/// **one transaction**, and only then is anything sent. A send that fails
/// leaves a row for the next run to retry; a mark that moved without the row
/// would lose the change for good.
async fn deliver(
    app: &mut crate::app::App,
    tick: &TickReport,
    delivery: Option<&Delivery>,
) -> Result<()> {
    let changes = tick.report.changes();

    // Silence when nothing happened, unless somebody asked to hear it anyway.
    // An automation where every message means something is the point of that;
    // a heartbeat is for the opposite case, where the absence of messages is
    // the signal and "quiet" has to be told from "stopped".
    let body = match delivery.and_then(|d| event_for(&changes, d.heartbeat)) {
        Some(event) => {
            let run_id = run_id(snob_core::store::now(), tick.report.account_pk);
            // Serialized once, here, and stored as the string that goes on the
            // wire. `serde_json` may render one value two ways, and the
            // signature covers bytes — so a retry that rendered it again could
            // be rejected after the first attempt was accepted.
            Some((
                run_id.clone(),
                serde_json::to_string(&payload(tick, &run_id, event))?,
            ))
        }
        None => None,
    };

    let queued = crate::engine::watch::commit(
        app,
        tick,
        body.as_ref().map(|(run_id, body)| Queued {
            run_id,
            body,
            destination: delivery.map(|d| d.destination.as_str()),
        }),
    )?;

    let Some(delivery) = delivery else {
        return Ok(());
    };

    // This run's report, when it made one.
    if let (Some(id), Some((run_id, body))) = (queued, body.as_ref()) {
        send_one(app, delivery, id, run_id, body, 1).await;
    }

    // Whatever is still owed from earlier runs is NOT drained here. It is
    // drained once per run, after every watched account, by the caller. Doing
    // it here meant once per account: the backoff is a stored wall-clock
    // moment while a tick takes minutes, so an owed report burned most of its
    // eight attempts inside a single run, and one run could make
    // `accounts * DRAIN_LIMIT` requests at somebody's server at once.
    Ok(())
}

/// Sends one queued report and records what came of it.
/// `run_id` is what the receiver is told to deduplicate on, and it is **not**
/// the row id.
///
/// The row id is a SQLite rowid with no AUTOINCREMENT, so it is reused after
/// `prune` empties the table — which happens on any account quiet for longer
/// than `KEEP_DELIVERIES_FOR_SECS`, the default case. A receiver doing exactly
/// what AGENTS.md, the CHANGELOG and the tests tell it to do would then drop a
/// real report as a repeat. `run_id` is `UNIQUE` in the schema and already
/// inside the body, so it is the one value that means what the header claims.
async fn send_one(
    app: &crate::app::App,
    delivery: &Delivery,
    id: i64,
    run_id: &str,
    body: &str,
    attempt: i64,
) {
    let now = snob_core::store::now();
    let outcome = delivery
        .client
        .post(body, event_of(body), run_id, attempt)
        .await;

    // Failing to write down what happened is not worth failing the run over:
    // the report either arrived or it did not, and the row is still there.
    let recorded = match &outcome {
        Attempt::Delivered { status } => {
            deliveries::delivered(app.db().conn(), id, *status, now).map(|()| None)
        }
        Attempt::Failed { status, error } => {
            deliveries::failed(app.db().conn(), id, *status, error, false, now).map(Some)
        }
        // `permanent` is for a request that could not be sent at all, and
        // nothing else. Every HTTP answer is retried within the attempt and age
        // budget now, because a 4xx used to expire the row after **zero**
        // retries — and the mark had already moved, so the arrivals and
        // departures in that report were gone for good. Many 4xx are transient:
        // n8n answers 404 for a workflow that is not currently registered, a
        // reverse proxy answers 404 or 403 while it reloads, and an expired
        // bearer token answers 401. None of those is a reason to throw the only
        // copy of a change away, and the far end is the user's own server, so
        // knocking again costs nothing that matters.
        Attempt::Refused { status, error } => {
            deliveries::failed(app.db().conn(), id, Some(*status), error, true, now).map(Some)
        }
    };
    let settled = match recorded {
        Ok(settled) => settled,
        Err(e) => {
            ui::warn(&format!(
                "could not record what happened to the report: {e}"
            ));
            None
        }
    };

    // What `failed` decided, said out loud. Its answer used to be dropped and
    // the sentence written from the HTTP result instead, so the eighth failure
    // — the one that throws the report away — was announced as "it is queued
    // and will be tried again", and `status` then showed nothing owed.
    match (&outcome, settled) {
        (Attempt::Delivered { .. }, _) => {}
        (_, Some(deliveries::Outcome::Retrying(at))) => ui::warn(&format!(
            "the report could not be delivered ({}); it is queued and will be tried again {}",
            error_of(&outcome),
            describe_when(at, now)
        )),
        (_, Some(deliveries::Outcome::GaveUp(reason))) => ui::warn(&format!(
            "the report could not be delivered ({}). {} It will not be tried again, and what it \
             said is not reported a second time: the next run compares against what this one \
             already counted.",
            error_of(&outcome),
            match reason {
                deliveries::GaveUp::Refused => "The request could not be sent at all.",
                deliveries::GaveUp::OutOfAttempts => "Every attempt was refused.",
                deliveries::GaveUp::TooOld => "It is too old to be news now.",
            }
        )),
        // `failed` itself would not record. The row is untouched, so the next
        // run tries it again.
        (_, None) => ui::warn(&format!(
            "the report could not be delivered ({})",
            error_of(&outcome)
        )),
    }
}

/// The far end's answer, whatever shape the attempt came back in.
fn error_of(attempt: &Attempt) -> &str {
    match attempt {
        Attempt::Delivered { .. } => "",
        Attempt::Failed { error, .. } | Attempt::Refused { error, .. } => error,
    }
}

/// "in 4m", for a moment in the near future.
fn describe_when(at: i64, now: i64) -> String {
    match at.checked_sub(now) {
        Some(seconds) if seconds > 0 => format!(
            "in {}",
            snob_core::duration::format(std::time::Duration::from_secs(seconds as u64))
        ),
        _ => "on the next run".to_string(),
    }
}

/// Whether this run has anything to say, and what it would be called.
///
/// One function rather than a guard and a name computed from the same fact ten
/// lines apart: `None` means the run is quiet and nothing is queued at all.
/// Collapsing the guard so every run speaks turns a `--every 30m` monitor from
/// a handful of messages a week into forty-eight a day, which is the opposite
/// of what "nothing is sent when nothing changed" promises -- and it is the
/// point of `--heartbeat` that the *absence* of a message means something.
fn event_for(changes: &Changes, heartbeat: bool) -> Option<&'static str> {
    if !changes.is_empty() {
        Some("watch.changes")
    } else if heartbeat {
        Some("watch.heartbeat")
    } else {
        None
    }
}

/// The event name a queued body carries.
///
/// Read back out of the body rather than remembered alongside it, so the header
/// and the body cannot disagree — which they did, the header saying
/// `watch.changes` over a heartbeat. A retry days later reads the same string
/// from the same bytes, so it stays true then too.
fn event_of(body: &str) -> &str {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("event")
                .and_then(|e| e.as_str())
                // The borrow has to outlive the parsed value, so the answer is
                // matched back to one of the names this tool emits rather than
                // returned from inside it. Anything else is a body this version
                // did not write.
                .map(|event| match event {
                    "watch.heartbeat" => "watch.heartbeat",
                    _ => "watch.changes",
                })
        })
        .unwrap_or("watch.changes")
}

/// Tries whatever is owed from earlier runs.
///
/// **Once per run, after every account**, not once per account. The backoff is
/// a stored wall-clock moment while a tick takes minutes, so draining inside
/// the per-account loop burned an owed report's whole retry ladder inside a
/// single run: six accounts was five of the eight attempts spent, nine was
/// `expired` before the run finished. It also made up to `accounts ×
/// DRAIN_LIMIT` POSTs at somebody's server in one go, which is the thing
/// `DRAIN_LIMIT` exists to bound.
async fn drain(app: &crate::app::App, delivery: &Delivery) {
    let now = snob_core::store::now();
    let owed = match deliveries::due(app.db().conn(), now, DRAIN_LIMIT, &delivery.destination) {
        Ok(owed) => owed,
        Err(e) => {
            ui::warn(&format!("could not read the queue of owed reports: {e}"));
            return;
        }
    };

    for report in owed {
        send_one(
            app,
            delivery,
            report.id,
            &report.run_id,
            &report.body,
            report.attempts + 1,
        )
        .await;
    }
}

/// How many owed reports one run will try before leaving the rest.
///
/// Bounded so a queue that built up over a weekend does not turn one run into a
/// hundred requests at somebody's server all at once. The rest go on the next
/// run, and `deliveries::MAX_AGE_SECS` is what stops them lingering forever.
const DRAIN_LIMIT: usize = 10;

/// An id for this report, unique enough for a receiver to deduplicate on.
///
/// The moment and a random suffix rather than a UUID: the column is `UNIQUE`,
/// so a collision is an error rather than a silent overwrite, and this avoids a
/// dependency for a value nothing derives meaning from.
/// `now` is an argument rather than read inside, which is this project's shape
/// for anything with arithmetic in it -- and here it is also what lets the
/// uniqueness be tested without building a whole tick.
fn run_id(now: i64, account_pk: Pk) -> String {
    format!("{}-{:08x}", now, fastrand::u32(..) ^ (account_pk as u32))
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
        Skipped::Incomplete(reason) => format!(
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
fn tick_json(tick: &TickReport) -> serde_json::Value {
    let mut out = as_json(&tick.report);
    out["run"] = serde_json::json!({
        "looked": tick.looked(),
        "requests": tick.requests,
        "lists": tick.lists.iter().map(|l| serde_json::json!({
            "kind": l.kind.as_str(),
            "skipped": l.skipped.map(skipped_token),
        })).collect::<Vec<_>>(),
    });
    out
}

/// What goes on the wire.
///
/// A contract with whatever is on the other end, so it is built here by hand
/// and asserted in a test: this is the one output of the tool that a stranger's
/// automation branches on, and a field renamed by accident breaks a workflow
/// somebody built months ago.
///
/// Three decisions worth knowing about, all of them about what an n8n node
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
fn payload(tick: &TickReport, run_id: &str, event: &str) -> serde_json::Value {
    let report = &tick.report;
    let changes = report.changes();

    serde_json::json!({
        "schema": 1,
        "event": event,
        "run": {
            "id": run_id,
            "at": snob_core::store::now(),
            "looked": tick.looked(),
            "requests": tick.requests,
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
        Skipped::Incomplete(_) => "incomplete",
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
            lines.push(format!(
                "Nothing has changed for {who} since the last report."
            ));
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
        lines.push(format!(
            "  {} now go by another name",
            changes.renamed.len()
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

    const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
    const SID: &str = "42%3AAbCdEfGh%3A20";

    /// Nothing is sent when nothing changed, and `--heartbeat` is what asks
    /// for the opposite.
    ///
    /// The guard and the name used to be computed from the same fact ten lines
    /// apart, and collapsing the guard so every run speaks was invisible: a
    /// `--every 30m` monitor would go from a handful of messages a week to
    /// forty-eight a day, and the automation reading silence as the signal
    /// would never hear it again.
    #[test]
    fn a_quiet_run_says_nothing_unless_a_heartbeat_was_asked_for() {
        let quiet = report_with(None, vec![]).changes();
        let moved = report_with(
            Some(list(
                Basis::Compare {
                    before: 1,
                    after: 2,
                },
                ListDiff {
                    gained: vec![user(7, "newcomer")],
                    lost: vec![],
                },
                Some(1_000),
            )),
            vec![],
        )
        .changes();

        assert_eq!(event_for(&quiet, false), None, "silence means something");
        assert_eq!(event_for(&quiet, true), Some("watch.heartbeat"));
        assert_eq!(event_for(&moved, false), Some("watch.changes"));
        assert_eq!(
            event_for(&moved, true),
            Some("watch.changes"),
            "a run with news is news, not a heartbeat"
        );
    }

    /// The id a receiver deduplicates on is unique, and the column is `UNIQUE`
    /// so a repeat is an error rather than a silent overwrite.
    ///
    /// Reducing it to the second alone survived every test: two accounts
    /// reported in the same second, or one account twice, then collide -- and
    /// the insert fails inside the transaction that queues the report, so the
    /// report **and** the marks roll back together.
    #[test]
    fn two_reports_never_share_an_id() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1_000 {
            assert!(
                seen.insert(run_id(1_700, 42)),
                "the same second and the same account produced one id twice"
            );
        }
        // And two accounts in one second, which is one run of a monitor
        // watching more than one.
        assert_ne!(run_id(1_700, 42), run_id(1_700, 43));
    }

    /// An app and a webhook pointed at the same mock server.
    fn app_posting_to(server: &wiremock::MockServer) -> (crate::app::App, Delivery) {
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
            destination: destination_of(&url),
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

    async fn accepting(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/hook"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(server)
            .await;
    }

    fn owe(app: &crate::app::App, delivery: &Delivery, how_many: usize, at: i64) {
        for n in 0..how_many {
            deliveries::enqueue(
                app.db().conn(),
                &format!("run-{n}"),
                42,
                r#"{"schema":1,"event":"watch.changes"}"#,
                at,
                Some(&delivery.destination),
            )
            .unwrap();
        }
    }

    /// A report owed from an earlier run goes out on a later one.
    ///
    /// The whole reason the outbox exists, and deleting the drain entirely --
    /// at both call sites -- left the suite green. The integration test that
    /// looks like it covers this reimplements the loop by hand, so it proves
    /// the store functions work and nothing about the loop production runs.
    #[tokio::test]
    async fn a_report_owed_from_an_earlier_run_goes_out_on_a_later_one() {
        let server = wiremock::MockServer::start().await;
        accepting(&server).await;
        let (app, delivery) = app_posting_to(&server);
        let now = snob_core::store::now();
        owe(&app, &delivery, 2, now);

        drain(&app, &delivery).await;

        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert_eq!(deliveries::pending(app.db().conn()).unwrap(), 0);
    }

    /// And one run does not empty a weekend's worth of queue at somebody's
    /// server all at once.
    ///
    /// `DRAIN_LIMIT` is the only thing bounding that, and raising it from ten to
    /// a thousand was invisible.
    #[tokio::test]
    async fn one_run_sends_at_most_the_drain_limit() {
        let server = wiremock::MockServer::start().await;
        accepting(&server).await;
        let (app, delivery) = app_posting_to(&server);
        let now = snob_core::store::now();
        // A literal count and a literal ceiling, not `DRAIN_LIMIT + 5` and
        // `DRAIN_LIMIT`: a test written in terms of the constant it is checking
        // passes whatever that constant becomes, which is exactly how this
        // bound came to be unguarded in the first place.
        let owed = 25;
        owe(&app, &delivery, owed, now);

        drain(&app, &delivery).await;

        let sent = server.received_requests().await.unwrap().len();
        assert!(
            sent <= 15,
            "one run made {sent} requests at somebody's server in a row"
        );
        assert_eq!(
            deliveries::pending(app.db().conn()).unwrap() as usize,
            owed - sent,
            "the rest are still owed, for the next run"
        );
    }

    /// A run drains the queue even when it had no news of its own.
    ///
    /// AGENTS.md's rule is that owed reports are retried by **any** run, and the
    /// account loop is not what decides it: a monitor whose counters have not
    /// moved is the common case, and it is exactly the run that used to leave a
    /// backlog untouched. Deleting the drain from `run_one` left the whole
    /// suite green, because the only tests that reached it called `drain`
    /// directly -- they proved the function works and nothing about anybody
    /// calling it.
    ///
    /// No account is watched here on purpose. That removes the network from the
    /// test entirely and asks the one question that is open: does a run that
    /// looked at nothing still send what it owes?
    #[tokio::test]
    async fn a_run_with_nothing_to_look_at_still_sends_what_it_owes() {
        let server = wiremock::MockServer::start().await;
        accepting(&server).await;
        let (mut app, delivery) = app_posting_to(&server);
        owe(&app, &delivery, 1, snob_core::store::now());

        let args = WatchRunArgs {
            no_progress: true,
            ..WatchRunArgs::default()
        };
        run_one(&args, &mut app, &[], Some(&delivery))
            .await
            .unwrap();

        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a run that had nothing to report still owes what it owed"
        );
        assert_eq!(deliveries::pending(app.db().conn()).unwrap(), 0);
    }

    /// A `watch.toml` as the tool would read one.
    fn watch_toml(body: &str) -> WatchConfig {
        config::parse(body, std::path::Path::new("watch.toml")).expect("the fixture parses")
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

    /// The event a queued body carries is read back out of the body.
    ///
    /// It has to be, because a retry days later has only the bytes: remembering
    /// the name alongside them let the two disagree, and the header said
    /// `watch.changes` over every heartbeat -- so exactly the receiver the
    /// header exists for treated each one as a report of changes.
    ///
    /// Nothing tested this. The test that looks like it does hands the name to
    /// the client as an argument, so it pins the client and not the reading.
    #[test]
    fn the_event_is_read_back_out_of_the_body_it_describes() {
        assert_eq!(
            event_of(r#"{"schema":1,"event":"watch.heartbeat"}"#),
            "watch.heartbeat"
        );
        assert_eq!(
            event_of(r#"{"schema":1,"event":"watch.changes"}"#),
            "watch.changes"
        );

        // Anything this version did not write reads as the ordinary case rather
        // than as a heartbeat: a receiver that drops heartbeats must not be
        // handed a report of changes wearing one's name.
        assert_eq!(event_of(r#"{"event":"something.else"}"#), "watch.changes");
        assert_eq!(event_of("not json at all"), "watch.changes");
    }

    /// A `[webhook]` section as `watch.toml` would parse it.
    fn configured(url: &str, headers: &[(&str, &str)]) -> WebhookConfig {
        WebhookConfig {
            url: url.to_string(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            heartbeat: false,
        }
    }

    /// Every value the planned request would send under this name.
    ///
    /// A `Vec`, not an `Option`: "the header went out twice" and "the header
    /// went out once" are different answers, and one of the defects this file
    /// has already had was exactly that.
    fn sent_as<'a>(planned: &'a Planned, name: &str) -> Vec<&'a str> {
        planned
            .webhook
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// A credential set up for one address does not follow `--webhook` to
    /// another.
    ///
    /// Pointing a run at a request bin to see what the payload looks like is the
    /// first thing anybody does, and it used to send the team's `X-Api-Key` and
    /// the bearer token `snob watch setup` had stored to that bin. `check` was
    /// happy, because the new address was https.
    ///
    /// Replacing the origin comparison with `true` left all 656 tests green
    /// before this existed.
    #[test]
    fn a_token_stored_for_one_host_is_not_sent_to_another() {
        let file = configured(
            "https://n8n.internal/webhook/snob",
            &[("X-Api-Key", "team")],
        );
        let args = WebhookArgs {
            webhook: Some("https://bin.example/inspect".into()),
            ..Default::default()
        };

        let (planned, warnings) = plan(
            &args,
            Some(&file),
            Some(Secret::from("Bearer stored".to_string())),
            None,
        )
        .unwrap();
        let planned = planned.expect("there is an address to post to");

        assert_eq!(sent_as(&planned, "Authorization"), Vec::<&str>::new());
        assert_eq!(sent_as(&planned, "X-Api-Key"), Vec::<&str>::new());
        assert_eq!(
            warnings.len(),
            2,
            "both the headers and the token were withheld, so both are said: {warnings:?}"
        );
        assert!(warnings[0].contains("not sent with it"), "{warnings:?}");
        assert!(warnings[1].contains("stored token"), "{warnings:?}");
    }

    /// The same address is the same destination, however the URL was written.
    #[test]
    fn the_stored_token_is_sent_to_the_address_it_was_stored_for() {
        let file = configured("https://n8n.internal/webhook/snob", &[]);
        // A different path on the same host: still where the token belongs.
        let args = WebhookArgs {
            webhook: Some("https://n8n.internal/webhook/other".into()),
            ..Default::default()
        };

        let (planned, warnings) = plan(
            &args,
            Some(&file),
            Some(Secret::from("Bearer stored".to_string())),
            None,
        )
        .unwrap();

        assert_eq!(
            sent_as(&planned.unwrap(), "Authorization"),
            ["Bearer stored"]
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// A configured `Authorization` stops the stored one being added, so the
    /// two never both go out.
    ///
    /// The guard reads the **merged** list rather than the flags: looking only
    /// at `--header` meant a configured one and the keyring's one both
    /// travelled, and the second was the secret `setup` had put away.
    #[test]
    fn a_configured_authorization_stops_the_stored_token_being_added() {
        let file = configured(
            "https://n8n.internal/webhook/snob",
            &[("Authorization", "Bearer configured")],
        );

        let (planned, _) = plan(
            &WebhookArgs::default(),
            Some(&file),
            Some(Secret::from("Bearer stored".to_string())),
            None,
        )
        .unwrap();

        assert_eq!(
            sent_as(&planned.unwrap(), "Authorization"),
            ["Bearer configured"],
            "the stored token was added on top of one that was already there"
        );
    }

    /// An empty `--sign-with` is an unset environment variable, not a request
    /// to sign with nothing.
    ///
    /// Taken literally it signs with a zero-length key -- a well-formed
    /// signature anybody can forge -- and, because the flag wins over the
    /// keyring, it would silently replace a key that was configured correctly.
    #[test]
    fn an_empty_sign_with_is_refused_rather_than_signing_with_nothing() {
        for given in ["", "   "] {
            let args = WebhookArgs {
                webhook: Some("https://n8n.internal/hook".into()),
                sign_with: Some(given.to_string()),
                ..Default::default()
            };
            // Mapped away rather than unwrapped: the error carries a `Planned`,
            // which holds the merged headers, and those hold the token in the
            // clear.
            let refused = plan(&args, None, None, None).map(|_| ()).unwrap_err();
            assert!(refused.to_string().contains("empty value"), "{refused}");
        }
    }

    /// A typed header goes out after a configured one of the same name, which
    /// is what lets the flag override the file: the last one wins at the
    /// request.
    #[test]
    fn a_typed_header_comes_after_the_one_from_the_file() {
        let file = configured("https://n8n.internal/hook", &[("X-Source", "file")]);
        let args = WebhookArgs {
            header: vec!["X-Source: typed".into()],
            ..Default::default()
        };

        let (planned, _) = plan(&args, Some(&file), None, None).unwrap();

        assert_eq!(
            sent_as(&planned.unwrap(), "X-Source"),
            ["file", "typed"],
            "order is the override: `post` inserts them in turn"
        );
    }

    /// Options that need a webhook say so when there is none, rather than doing
    /// nothing quietly.
    #[test]
    fn asking_for_a_signature_with_nowhere_to_send_it_is_said_out_loud() {
        let args = WebhookArgs {
            sign_with: Some("a secret".into()),
            ..Default::default()
        };

        let (planned, warnings) = plan(&args, None, None, None).unwrap();

        assert!(planned.is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no webhook"), "{warnings:?}");
    }

    fn user(pk: Pk, name: &str) -> User {
        User {
            pk,
            username: name.into(),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    fn report_with(followers: Option<ListReport>, renamed: Vec<Rename>) -> WatchReport {
        WatchReport {
            account_pk: 42,
            username: Some("me".into()),
            is_self: true,
            followers,
            following: None,
            renamed,
        }
    }

    fn list(basis: Basis, diff: ListDiff, since: Option<i64>) -> ListReport {
        ListReport {
            kind: ListKind::Followers,
            basis,
            since,
            until: 2_000,
            diff,
            total: 10,
        }
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

    /// The shape a stranger's automation branches on, pinned to a literal.
    ///
    /// This is the one output of the tool that somebody else's workflow reads,
    /// and a key renamed by accident breaks something built months ago with no
    /// error anywhere. Comparing against a literal means a change to the
    /// contract has to be a change somebody made on purpose.
    ///
    /// The two moving fields are left out of the comparison: `run.at` is the
    /// clock and `run.id` is random, which is what they are for.
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
                    from: "before".into(),
                    to: "after".into(),
                    at: 1_500,
                }],
            ),
            14,
        );

        let mut payload = payload(&tick, "run-1", "watch.changes");
        payload["run"]["at"] = serde_json::Value::Null;

        assert_eq!(
            payload,
            serde_json::json!({
                "schema": 1,
                "event": "watch.changes",
                "run": {
                    "id": "run-1",
                    "at": null,
                    "looked": false,
                    "requests": 14,
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
