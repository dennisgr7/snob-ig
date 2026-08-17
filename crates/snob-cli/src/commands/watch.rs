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

use anyhow::{Context, Result};
use snob_core::model::{ListKind, User, printable};
use snob_core::paths::AppPaths;
use snob_core::secret::Secret;
use snob_core::secrets::{Kind, SecretStore};
use snob_core::store::deliveries;
use snob_core::watch::config::{self, WatchConfig};
use snob_core::watch::schedule::{self, Due, Schedule, Weekday};
use snob_core::watch::{Basis, ListDiff, Rename};
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
    let watched = watched_from(args.target.clone(), configured.as_ref())?;
    // Built once and reused, so a service that runs for months holds one
    // connection pool rather than building a TLS stack every few hours. Checked
    // here for the same reason the schedule is: a bad address should stop this
    // at the moment somebody is watching it start.
    let delivery = delivery_from(&args.delivery, configured.as_ref(), &secrets)?;

    // Refused here rather than at the first tick. A service that starts, waits
    // six hours and then exits because it was never allowed to read that
    // account is a service that looked healthy all afternoon.
    if !watched.may_run_unattended() {
        return Err(ExitError::new(
            ExitCode::Interrupted,
            format!(
                "reading {}'s lists needs confirmation, and a scheduled run has nobody to ask.\n\
                 Use \"snob watch once {}\" while you are here to answer it.",
                target_label(args.target.as_deref()),
                args.target.as_deref().unwrap_or_default(),
            ),
        )
        .into());
    }

    ui::info(&format!(
        "Watching {}. {} Stop with Ctrl+C.",
        target_label(args.target.as_deref()),
        describe_schedule(&schedule, args.now),
    ));

    // Installed once for the process, which is what lets this open an `App` per
    // run without leaving a signal listener behind on each one.
    let cancel = crate::interrupt::install();

    // `None` means "nothing has run", which is what makes the first run
    // immediate. Without `--now` the schedule decides the first one instead.
    let mut last_run: Option<i64> = if args.now {
        None
    } else {
        Some(snob_core::store::now())
    };

    // The moment this is waiting for, and how many scheduled runs it stands
    // for. Held across iterations rather than recomputed, because the loop
    // wakes every minute to re-check the wall clock and rolling the jitter each
    // time would make the wake-up wander instead of settling on one instant.
    let mut waiting_for: Option<(i64, u32)> = None;
    // `--now` means now. Jitter is there so a *schedule* does not land on the
    // same second every day; delaying the run somebody just asked for by up to
    // a quarter of an hour would only look broken.
    let mut skip_jitter = args.now;

    loop {
        let now = snob_core::store::now();

        let (wake_at, missed) = match waiting_for {
            Some(pending) => pending,
            None => {
                let (due_at, missed) = match schedule::due(&schedule, last_run, now, &chrono::Local)
                {
                    Due::Now { missed } => (now, missed),
                    Due::At(i64::MAX) => {
                        return Err(anyhow::anyhow!(
                            "this schedule can never come round: nothing matches it"
                        ));
                    }
                    Due::At(at) => (at, 0),
                };

                // Rolled once per due moment. The roll is made here rather than
                // inside `with_jitter` so that function reads no randomness and
                // its bounds stay testable.
                let wake_at = if skip_jitter {
                    due_at
                } else {
                    schedule::with_jitter(due_at, schedule.jitter(), fastrand::f64())
                };
                let pending = (wake_at, missed);
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
            skip_jitter = false;

            // A scheduled service does not exit because one run failed. A
            // cooldown lifts, a network comes back, and a session that is gone
            // gets reported every time until somebody fixes it — which is the
            // point of something that watches.
            if let Err(e) = run_one(&args, &watched, delivery.as_ref(), &secrets, paths).await {
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

/// One run inside the loop: open, tick, print, close.
///
/// A fresh `App` per run rather than one held open for weeks. It picks up a
/// session that was replaced and a refreshed User-Agent, and — the reason that
/// matters on Windows — it holds no SQLite connection while the loop sleeps, so
/// `snob purge` in another terminal is not blocked by a file this has open.
async fn run_one(
    args: &WatchRunArgs,
    watched: &Watched,
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

    let tick = crate::engine::watch::tick(&mut app, watched).await;
    app.progress().finish();
    let tick = tick?;

    // One line per run down a pipe, so `snob watch >> events.ndjson` is a
    // complete way to use this without a webhook.
    if args.json {
        println!("{}", serde_json::to_string(&tick_json(&tick))?);
    } else if !tick.report.changes().is_empty() {
        for line in describe(&tick.report) {
            println!("{line}");
        }
    }

    for (kind, skipped) in tick
        .lists
        .iter()
        .filter_map(|l| l.skipped.map(|s| (l.kind, s)))
    {
        ui::warn(&refusal_line(kind, skipped));
    }

    deliver(&mut app, &tick, delivery).await
}

/// Which account a scheduled run watches, and whether it may.
///
/// A name on the command line is checked against the file, because that is the
/// only place a consent can have been recorded — and an unattended run that
/// could be pointed at a stranger by an argument would make the recording
/// pointless.
fn watched_from(target: Option<String>, configured: Option<&WatchConfig>) -> Result<Watched> {
    let Some(name) = target else {
        return Ok(Watched::own());
    };

    let recorded = configured
        .into_iter()
        .flat_map(|c| c.accounts.iter())
        .find(|account| !account.is_own() && account.target.eq_ignore_ascii_case(&name))
        .and_then(|account| account.consent);

    Ok(match recorded {
        Some(consent) => Watched::consented(
            name,
            crate::engine::watch::Consent {
                given_at: consent.agreed_at,
            },
        ),
        None => Watched::asking(name),
    })
}

fn target_label(target: Option<&str>) -> String {
    match target {
        Some(name) => format!("@{}", printable(name)),
        None => "your account".to_string(),
    }
}

/// Builds the schedule from the flags, or the file, or explains what is
/// missing.
///
/// **A flag replaces the schedule rather than merging with it.** Half from the
/// file and half from the command line is a schedule nobody can read back: the
/// only honest reading of `--every 6h` against a configured `--on mon` is the
/// one the person typing meant, and there is no way to know which.
fn schedule_from(args: &WatchRunArgs, configured: Option<&WatchConfig>) -> Result<Schedule> {
    let given =
        args.cron.is_some() || !args.at.is_empty() || !args.on.is_empty() || args.every.is_some();

    let (cron, at, on, every, jitter) = if given {
        (
            args.cron.clone(),
            args.at.clone(),
            args.on.clone(),
            args.every,
            args.jitter,
        )
    } else {
        match configured {
            Some(c) => (
                c.cron.clone(),
                c.at.clone(),
                c.on.clone(),
                c.every,
                c.jitter,
            ),
            None => (None, vec![], vec![], None, None),
        }
    };

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
fn describe_schedule(schedule: &Schedule, now: bool) -> String {
    let jitter = schedule.jitter();
    let mut line = if jitter.is_zero() {
        "Running exactly on schedule.".to_string()
    } else {
        format!(
            "Each run is pushed up to {} later, so it does not land on the same second every time.",
            snob_core::duration::format(jitter)
        )
    };
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
        for line in describe(&tick.report) {
            println!("{line}");
        }
    }

    // What was refused goes to standard error, so it does not land in the
    // middle of a report something else is parsing — and it is said even in
    // JSON, where a caller reading `looked` would otherwise have to guess why.
    for skipped in tick
        .lists
        .iter()
        .filter_map(|l| l.skipped.map(|s| (l.kind, s)))
    {
        ui::warn(&refusal_line(skipped.0, skipped.1));
    }

    deliver(&mut app, &tick, delivery.as_ref()).await?;

    ui::info(&format!(
        "{} - {}",
        report::stored_on(snob_core::store::now()),
        report::requests(tick.requests)
    ));

    Ok(ExitCode::Ok)
}

/// Where reports go, once the arguments have been checked.
struct Delivery {
    client: WebhookClient,
    heartbeat: bool,
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
    let from_file = configured.and_then(|c| c.webhook.as_ref());

    let Some(url) = args
        .webhook
        .clone()
        .or_else(|| from_file.map(|w| w.url.clone()))
    else {
        if !args.header.is_empty() || args.sign_with.is_some() || args.heartbeat {
            ui::warn("there is no webhook, so nothing is sent and those options do nothing");
        }
        return Ok(None);
    };

    // Headers from the file first, then the ones typed, so a flag can override
    // a configured one of the same name — the last one wins at the request.
    let mut headers: Vec<(String, String)> = from_file
        .map(|w| {
            w.headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();

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
    if !args
        .header
        .iter()
        .any(|h| h.to_ascii_lowercase().starts_with("authorization"))
        && let Some(token) = secrets.load_secret(Kind::WatchToken)?
    {
        headers.push(("Authorization".to_string(), token.expose().to_string()));
    }

    let key = match args.sign_with.clone() {
        Some(given) => Some(Secret::from(given)),
        None => secrets.load_secret(Kind::WatchSigningKey)?,
    };

    let webhook = Webhook {
        url: Url::parse(&url).with_context(|| format!("\"{url}\" is not an address"))?,
        headers,
        key,
    };
    webhook::check(&webhook)?;

    Ok(Some(Delivery {
        client: WebhookClient::new(webhook)?,
        heartbeat: args.heartbeat || from_file.is_some_and(|w| w.heartbeat),
    }))
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
    let body = match delivery {
        Some(d) if !changes.is_empty() || d.heartbeat => {
            let run_id = run_id(tick);
            // Serialized once, here, and stored as the string that goes on the
            // wire. `serde_json` may render one value two ways, and the
            // signature covers bytes — so a retry that rendered it again could
            // be rejected after the first attempt was accepted.
            let event = if changes.is_empty() {
                "watch.heartbeat"
            } else {
                "watch.changes"
            };
            Some((
                run_id.clone(),
                serde_json::to_string(&payload(tick, &run_id, event))?,
            ))
        }
        _ => None,
    };

    let queued = crate::engine::watch::commit(
        app,
        tick,
        body.as_ref().map(|(run_id, body)| Queued { run_id, body }),
    )?;

    let (Some(delivery), Some(id), Some((_, body))) = (delivery, queued, body.as_ref()) else {
        return Ok(());
    };

    send_one(app, delivery, id, body, 1).await;
    // Whatever is left owed from earlier runs. After this one, so the newest
    // report is not held up behind a backlog.
    drain(app, delivery).await;
    Ok(())
}

/// Sends one queued report and records what came of it.
async fn send_one(app: &crate::app::App, delivery: &Delivery, id: i64, body: &str, attempt: i64) {
    let now = snob_core::store::now();
    let outcome = delivery.client.post(body, &id.to_string(), attempt).await;

    // Failing to write down what happened is not worth failing the run over:
    // the report either arrived or it did not, and the row is still there.
    let recorded = match &outcome {
        Attempt::Delivered { status } => deliveries::delivered(app.db().conn(), id, *status, now),
        Attempt::Failed { status, error } => {
            deliveries::failed(app.db().conn(), id, *status, error, false, now).map(|_| ())
        }
        Attempt::Refused { status, error } => {
            deliveries::failed(app.db().conn(), id, Some(*status), error, true, now).map(|_| ())
        }
    };
    if let Err(e) = recorded {
        ui::warn(&format!(
            "could not record what happened to the report: {e}"
        ));
    }

    match outcome {
        Attempt::Delivered { .. } => {}
        Attempt::Failed { error, .. } => ui::warn(&format!(
            "the report could not be delivered ({error}); it is queued and will be tried again"
        )),
        Attempt::Refused { error, .. } => ui::warn(&format!(
            "the webhook refused the report ({error}). Waiting will not change that, so it was \
             not queued for another try -- check the address and any token it needs"
        )),
    }
}

/// Tries whatever is owed from earlier runs.
async fn drain(app: &crate::app::App, delivery: &Delivery) {
    let now = snob_core::store::now();
    let owed = match deliveries::due(app.db().conn(), now, DRAIN_LIMIT) {
        Ok(owed) => owed,
        Err(e) => {
            ui::warn(&format!("could not read the queue of owed reports: {e}"));
            return;
        }
    };

    for report in owed {
        send_one(app, delivery, report.id, &report.body, report.attempts + 1).await;
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
fn run_id(tick: &TickReport) -> String {
    format!(
        "{}-{:08x}",
        snob_core::store::now(),
        fastrand::u32(..) ^ (tick.report.account_pk as u32)
    )
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

    let report = crate::engine::watch::from_store(&app, args.target.as_deref(), false)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&as_json(&report))?);
        return Ok(ExitCode::Ok);
    }

    for line in describe(&report) {
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
fn describe(report: &WatchReport) -> Vec<String> {
    let who = match report.username.as_deref() {
        Some(name) => format!("@{}", printable(name)),
        None => format!("account {}", report.account_pk),
    };

    if !report.has_anything_stored() {
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
    use snob_core::Pk;

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
            history_cursor: 0,
            until: 2_000,
            diff,
            total: 10,
        }
    }

    /// Somebody who has never run the tool is told to run it, not told that
    /// nothing changed — which would be true and useless.
    #[test]
    fn an_account_with_nothing_stored_is_told_what_to_run() {
        let lines = describe(&report_with(None, vec![]));
        assert!(lines.join("\n").contains("snob followers"), "{lines:?}");
    }

    /// The worst thing this feature could print. A first look has no earlier
    /// capture, so it must say so rather than report an empty diff as calm.
    #[test]
    fn a_first_look_says_so_instead_of_saying_nothing_changed() {
        let lines = describe(&report_with(
            Some(list(
                Basis::Baseline { snapshot_id: 1 },
                ListDiff::default(),
                None,
            )),
            vec![],
        ));
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
            history_cursor: 0,
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

        let text = describe(&report).join("\n");
        assert!(text.contains("lists have never been reported"), "{text}");
        assert!(!text.contains("list has never"), "{text}");
    }

    #[test]
    fn an_arrival_and_a_departure_are_both_named() {
        let diff = ListDiff {
            gained: vec![user(1, "arrived")],
            lost: vec![user(2, "left")],
        };
        let lines = describe(&report_with(
            Some(list(
                Basis::Compare {
                    before: 1,
                    after: 2,
                },
                diff,
                Some(1_000),
            )),
            vec![],
        ));

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
        let lines = describe(&report_with(
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
        ));

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
        let lines = describe(&report_with(
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
        ));
        assert!(
            !lines.join("\n").contains('\u{202e}'),
            "a bidi override reached the terminal"
        );
    }
}
