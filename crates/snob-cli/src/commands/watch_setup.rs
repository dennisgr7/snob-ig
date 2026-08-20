//! `snob watch setup` and `snob watch status`.
//!
//! The half of the monitor that is about not having to remember flags. `setup`
//! asks the questions and writes a file; `status` reads back what is configured
//! and what has happened, which is the only way to tell a monitor that found
//! nothing from one that quietly stopped.
//!
//! Split from `commands::watch` because it shares almost nothing with it: that
//! module is about reports, this one is about a file and a keyring entry.

use anyhow::{Context, Result, bail};
use snob_core::model::{ListKind, printable};
use snob_core::paths::AppPaths;
use snob_core::secret::Secret;
use snob_core::secrets::{Kind, SecretStore};
use snob_core::store::{Store, deliveries, watch as watch_store};
use snob_core::watch::config::{self, AccountConfig, WatchConfig};
use snob_core::{duration, watch::schedule};

use crate::cli::{WatchSetupArgs, WatchStatusArgs};
use crate::engine::check::Verdict;
use crate::exit::{ExitCode, ExitError};
use crate::{report, ui};

/// Walks somebody through configuring the monitor.
pub async fn setup(
    args: WatchSetupArgs,
    secrets: SecretStore,
    paths: &AppPaths,
) -> Result<ExitCode> {
    // Every question here needs an answer, and the failure mode of asking with
    // nobody there is a service configured by whatever the defaults happened to
    // be. Said once, up front.
    if !ui::can_show_a_menu() {
        return Err(ExitError::new(
            ExitCode::Interrupted,
            "\"snob watch setup\" asks questions and there is no terminal to ask at.\n\
             Write the file by hand, or run this where you can answer."
                .to_string(),
        )
        .into());
    }

    if let Some(existing) = existing_to_replace(paths, args.dry_run)? {
        ui::info(&format!(
            "There is already a configuration at {}.",
            config::path(paths).display()
        ));
        describe_config(&existing)
            .iter()
            .for_each(|l| eprintln!("  {l}"));
        if !ui::confirm("Replace it?", false)? {
            return Err(
                ExitError::new(ExitCode::Interrupted, "nothing was changed".to_string()).into(),
            );
        }
    }

    let schedule_line = ask_schedule()?;
    let (webhook, heartbeat, headers, signing_key, token) = ask_webhook()?;
    let accounts = ask_accounts()?;

    let text = config::template(
        &schedule_line,
        None,
        webhook.as_deref(),
        heartbeat,
        &headers,
        &accounts,
        signing_key.is_some(),
    );

    // Parsed before it is written, and before any secret is stored. A file the
    // tool wrote and cannot read is the one failure a person cannot debug, and
    // a keyring entry left behind for a configuration that was never saved is
    // litter with a token in it.
    config::parse(&text, &config::path(paths))
        .context("the configuration this produced could not be read back; this is a bug")?;

    if args.dry_run {
        println!("{text}");
        ui::info("Nothing was written.");
        return Ok(ExitCode::Ok);
    }

    // The file first, then the secrets. The comment above says a keyring entry
    // left behind for a configuration that was never saved is litter with a
    // token in it — and doing the secrets first is exactly how that state is
    // reached, because `config::write` can fail on a read-only or full disk
    // after both entries are already stored. A credential belonging to no
    // configured address is the worse half to be left holding: it is what the
    // origin check in `plan` has to defend against, and what `purge` has to
    // remember. This way round, a failure leaves an address with no credential,
    // which announces itself at the first run instead of sitting there.
    let written = config::write(paths, &text)?;
    store_secret(&secrets, Kind::WatchSigningKey, signing_key)?;
    store_secret(&secrets, Kind::WatchToken, token)?;

    ui::info(&format!("Written to {}.", written.display()));

    // Everything above is a claim about a machine, a session and somebody
    // else's server, and none of it had been tried. Trying it here is the whole
    // point: a name with a typo, a token the receiver rejects or a session that
    // has gone are all cheap to fix now and expensive to discover from an
    // unattended run's log a week later.
    println!();
    let report = super::watch::preflight(&Default::default(), &secrets, paths).await?;
    for line in super::watch::describe_check(&report) {
        println!("{line}");
    }

    offer_the_baseline(&report, &secrets, paths).await?;

    ui::info("Start it with \"snob watch\", or put \"snob watch once\" on a timer.");
    Ok(ExitCode::Ok)
}

/// The configuration this run would replace, if it would replace one.
///
/// The flag is an argument rather than a second condition on the read, because
/// in a let-chain the scrutinee is evaluated first: `config::load(paths)?` fired
/// before `!args.dry_run` could short-circuit, so the flag documented as "print
/// what would be written and write nothing" died on a file it had already
/// decided not to touch. One mistyped key does it — `evry = "6h"` — and the
/// message is a good one, naming the file, the line and the key. What is wrong
/// is that the command whose whole job is to show you what a correct file looks
/// like is the one that refuses to run when yours is not.
///
/// A function rather than two lines inline so that the order can be tested at
/// all: `setup` refuses without a terminal several statements before it reaches
/// here, and nothing a test can call would otherwise ever get this far.
fn existing_to_replace(paths: &AppPaths, dry_run: bool) -> Result<Option<WatchConfig>> {
    if dry_run {
        return Ok(None);
    }
    Ok(config::load(paths)?)
}

/// About how many accounts one request brings back.
///
/// Instagram serves roughly this many whatever `per_page` asks for, which is
/// the settled note in AGENTS.md. It is here to turn a follower count into a
/// number of requests for the sentence below, and nothing depends on it being
/// exact — it is an estimate offered to a person, labelled as one.
const ACCOUNTS_PER_REQUEST: u64 = 25;

/// Offers to take the first capture, saying what it costs.
///
/// The first scheduled run is a `Basis::Baseline`: it reports nothing, by
/// design, because there is nothing to compare against yet. Somebody who has
/// just finished configuring a monitor reads that as broken, and the fix is
/// either to explain it afterwards or to take the capture now — which also
/// means the first scheduled report is a real one.
///
/// **Offered, not taken.** Walking two lists is the heaviest thing this tool
/// does, and doing it unasked at the end of a wizard would be the one place
/// requests are spent without the person having agreed to them. The estimate
/// comes from counters the preflight already read, so working it out costs
/// nothing.
///
/// It lives here rather than in `check`, and that is deliberate: `check` is
/// meant to be safe to repeat and to point a monitoring system at, and a probe
/// that walks two lists every time it is polled is worse than no probe.
async fn offer_the_baseline(
    report: &crate::engine::check::CheckReport,
    secrets: &SecretStore,
    paths: &AppPaths,
) -> Result<()> {
    use crate::engine::check::What;

    // Nothing to offer if a run could not happen anyway, or if there is
    // already something to compare against.
    if report.verdict() == Verdict::Failed {
        return Ok(());
    }
    let missing = report
        .checked
        .iter()
        .any(|c| matches!(&c.what, What::Baseline { taken_at } if taken_at.len() < 2));
    if !missing || !ui::can_show_a_menu() {
        return Ok(());
    }

    let (followers, following) = report
        .checked
        .iter()
        .find_map(|c| match &c.what {
            What::Account {
                followers: Some(a),
                following: Some(b),
                ..
            } => Some((*a, *b)),
            _ => None,
        })
        .unwrap_or((0, 0));
    let requests =
        followers.div_ceil(ACCOUNTS_PER_REQUEST) + following.div_ceil(ACCOUNTS_PER_REQUEST);

    println!();
    ui::info(&format!(
        "There is no capture to compare against yet, so the first scheduled run \
         will lay one down and report nothing.\n\
         Taking it now means walking {followers} followers and {following} following: \
         roughly {requests} requests, a few minutes."
    ));
    if !ui::confirm("Take the first capture now?", false)? {
        ui::info("Left for the first scheduled run, which will report nothing and say so.");
        return Ok(());
    }

    let configured = config::load(paths)?;
    super::watch::baseline_now(configured.as_ref(), secrets, paths).await
}

/// Stores a secret, or clears whatever was there when this run has none.
///
/// Clearing matters: somebody running `setup` again to remove a token expects
/// it gone, and a stale one left in the keyring would keep being sent.
fn store_secret(secrets: &SecretStore, kind: Kind, value: Option<Secret>) -> Result<()> {
    match value {
        Some(secret) => secrets.save_secret(kind, &secret).with_context(|| {
            "the secret could not be stored in the keyring. Pass it on the command line \
             instead -- \"--sign-with\" or \"--header\" -- where a systemd unit can supply it \
             from an environment file."
        })?,
        None => secrets.forget_secret(kind)?,
    }
    Ok(())
}

fn ask_schedule() -> Result<String> {
    let choice = ui::choose(
        "How often should it look?",
        &[
            "Every so often (6h, 2d, 2w)",
            "On certain days, at certain times",
            "A cron expression I already have",
        ],
    )?
    .ok_or_else(|| ExitError::new(ExitCode::Interrupted, "nothing was changed".to_string()))?;

    match choice {
        0 => interval_line(&ui::prompt_line("How often? (for example 6h)")?),
        1 => {
            let days = ui::prompt_line("Which days? (mon,thu -- or blank for every day)")?;
            let times = ui::prompt_line("At what times? (09:00 or 09:00,21:00)")?;
            calendar_line(&days, &times)
        }
        _ => cron_line(&ui::prompt_line(
            "The expression? (for example 0 9 * * 1,4)",
        )?),
    }
}

// The three below are the whole of what `ask_schedule` decides, split out from
// the prompting so a test can reach them.
//
// Nothing could. The validation lives on this side of `dialoguer`, and the one
// test that claimed to cover it — `an_interval_below_the_floor_is_refused_at_setup`
// — asserted `Schedule::every(300).is_err()`, which is a fact about the schedule
// module and says nothing about whether `setup` asks it. Deleting the
// `Schedule::every(every)?` line left the suite green while `every = "5m"` was
// written into a file the scheduler refuses at every run afterwards, with the
// person who typed it long gone. `config::parse` cannot catch it either: it
// checks the TOML and the schema number, not what the values mean.

/// The `every = "..."` line, or what is wrong with the interval.
fn interval_line(text: &str) -> Result<String> {
    let every = duration::parse(text).map_err(|e| anyhow::anyhow!(e))?;
    // Validated here rather than at the first run, so "5m" is refused while the
    // person who typed it is still reading.
    schedule::Schedule::every(every)?;
    Ok(format!("every = \"{}\"", duration::format(every)))
}

/// The `on = [...]` and `at = [...]` lines, or what is wrong with the calendar.
fn calendar_line(days: &str, times: &str) -> Result<String> {
    fn listed(text: &str) -> Vec<&str> {
        text.split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .collect()
    }

    let days = listed(days);
    let times = listed(times);

    // Through the very function a run parses the file with, so the wizard
    // cannot accept a calendar the scheduler would then refuse.
    super::watch::calendar_from(&days, &times)?;

    let mut line = String::new();
    if !days.is_empty() {
        line.push_str(&format!("on = [{}]\n", quoted_list(&days)));
    }
    line.push_str(&format!("at = [{}]", quoted_list(&times)));
    Ok(line)
}

/// The `cron = "..."` line, or what is wrong with the expression.
fn cron_line(expression: &str) -> Result<String> {
    schedule::Schedule::cron(expression)?;
    Ok(format!("cron = \"{}\"", expression.trim()))
}

type WebhookAnswers = (
    Option<String>,
    bool,
    Vec<(String, String)>,
    Option<Secret>,
    Option<Secret>,
);

fn ask_webhook() -> Result<WebhookAnswers> {
    if !ui::confirm("Send each report to a webhook?", true)? {
        ui::info(
            "Nothing will be sent. \"snob watch --json >> events.ndjson\" is a complete way to \
             use it without one.",
        );
        return Ok((None, false, vec![], None, None));
    }

    let url = ui::prompt_line("Where? (https://n8n.local/webhook/snob)")?;
    let parsed =
        url::Url::parse(url.trim()).with_context(|| format!("\"{url}\" is not an address"))?;

    // Headers whose values are not secret go in the file; the Authorization
    // value goes to the keyring and nothing about it is written here. An empty
    // placeholder in the file would be worse than nothing: it reads as a header
    // that is configured and sends nothing.
    //
    // The question is asked rather than assumed away. This was an empty vector
    // with the comment above it, so `[webhook.headers]` could only ever be
    // written by hand while the wizard implied otherwise — and `X-Api-Key` on an
    // n8n instance is the ordinary case.
    let mut headers = Vec::new();
    if ui::confirm("Does it need any other headers?", false)? {
        loop {
            let line = ui::prompt_line("Name: value (blank when there are no more)")?;
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                bail!(
                    "\"{}\" is not a header; write it as \"Name: value\"",
                    printable(line)
                );
            };
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    // Checked while the person is still here, address and headers together. The
    // alternative is a service that starts, waits six hours, and then fails on
    // something they typed wrong.
    crate::watch::webhook::check(&crate::watch::webhook::Webhook {
        url: parsed,
        headers: headers.clone(),
        key: None,
    })?;

    let mut token = None;
    if ui::confirm("Does it need an Authorization header?", false)? {
        let value = ui::prompt_secret("The header value (for example \"Bearer abc123\")")?;
        if value.trim().is_empty() {
            bail!("an Authorization value cannot be empty");
        }
        token = Some(Secret::new(value.to_string()));
    }

    let signing_key = if ui::confirm(
        "Sign the body, so the receiver can check it came from here?",
        true,
    )? {
        let value = ui::prompt_secret("A shared secret (anything long and random)")?;
        if value.trim().is_empty() {
            bail!("a signing secret cannot be empty");
        }
        Some(Secret::new(value.to_string()))
    } else {
        None
    };

    let heartbeat = ui::confirm(
        "Send a report even when nothing changed, so silence means it stopped?",
        false,
    )?;

    Ok((
        Some(url.trim().to_string()),
        heartbeat,
        headers,
        signing_key,
        token,
    ))
}

fn ask_accounts() -> Result<Vec<(String, Option<i64>)>> {
    let mut accounts = vec![("self".to_string(), None)];

    if ui::confirm("Also watch somebody else's account?", false)? {
        ui::warn(
            "reading somebody else's lists is a heavier request than reading your own, and \
             Instagram is readier to refuse it. A scheduled run cannot ask, so the answer is \
             recorded in the file.",
        );
        loop {
            let name = ui::prompt_line("Whose? (their username, or blank to stop)")?;
            let name = crate::engine::target::clean(name.trim());
            if name.is_empty() {
                break;
            }
            if !ui::confirm(
                &format!(
                    "Record that you agreed to read @{}'s lists?",
                    printable(name)
                ),
                false,
            )? {
                continue;
            }
            accounts.push((name.to_string(), Some(snob_core::store::now())));
        }
    }
    Ok(accounts)
}

fn quoted_list(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reports what is configured and what has happened.
pub fn status(args: WatchStatusArgs, paths: &AppPaths) -> Result<ExitCode> {
    let config = config::load(paths)?;
    // Opened read-only in spirit: `Store::open` creates the schema, which is
    // what every other command does anyway, and nothing here writes.
    let db = Store::open(paths)?;

    let owed = deliveries::pending(db.conn())?;
    let marks = watch_store::all_marks(db.conn())?;
    // Per account, and reported per account: a run covers every configured one,
    // so one unqualified "last ran" line is whichever account happened to be
    // last in the file.
    //
    // Asked of the run log rather than of the marks. A mark only moves when a
    // list was compared, so an account whose every tick was refused — a fresh
    // setup whose first runs met a cooldown, a stranger who went private — has
    // no mark and plenty of runs, and this said "It has not run yet." about a
    // monitor that had been running all week.
    let last_runs = watch_store::last_runs(db.conn())?;

    // Resolved once, here, because the store is open here and `health` must not
    // reach for it. Two facts it cannot work out on its own: what to call each
    // account, and whether the file still names it.
    let watched = watched_pks(&db, config.as_ref())?;
    let mut runs_of = Vec::with_capacity(last_runs.len());
    for run in &last_runs {
        let name = snob_core::store::users::name(db.conn(), run.account_pk)?;
        runs_of.push(RunOf {
            run,
            who: crate::app::label(run.account_pk, name.as_deref()),
            watched: watched
                .as_ref()
                .is_none_or(|pks| pks.contains(&run.account_pk)),
        });
    }

    // Which accounts have had exactly one of their two lists reported on. Built
    // here for the same reason `runs_of` is: the store is open here.
    let mut seen: std::collections::BTreeMap<snob_core::Pk, Vec<ListKind>> = Default::default();
    for mark in &marks {
        seen.entry(mark.account_pk).or_default().push(mark.kind);
    }
    let mut reported = Vec::new();
    for (pk, kinds) in seen {
        let name = snob_core::store::users::name(db.conn(), pk)?;
        reported.push(HalfRead {
            who: crate::app::label(pk, name.as_deref()),
            reported: kinds[0],
            missing: (kinds.len() == 1).then(|| match kinds[0] {
                ListKind::Followers => ListKind::Following,
                ListKind::Following => ListKind::Followers,
            }),
        });
    }

    let health = health(
        config.as_ref(),
        &runs_of,
        &reported,
        owed,
        snob_core::store::now(),
    );

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "configured": config.is_some(),
                "config_path": config::path(paths).display().to_string(),
                "pending_deliveries": owed,
                // The verdict, so a caller reading this does not have to
                // reimplement which combinations of the fields below mean the
                // monitor has stopped doing its job.
                "health": {
                    "verdict": health.verdict.as_str(),
                    "notes": health.notes,
                },
                // Told apart from the marks below on purpose. A run that could
                // not look moves no mark, so without this a monitor sitting in
                // a cooldown is indistinguishable from one that was killed.
                "last_runs": last_runs.iter().map(|run| serde_json::json!({
                    "pk": run.account_pk,
                    "at": run.started_at,
                    "outcome": run.outcome,
                    "requests": run.requests,
                    "changes": run.changes,
                })).collect::<Vec<_>>(),
                "accounts": marks.iter().map(|m| serde_json::json!({
                    "pk": m.account_pk,
                    "kind": m.kind.as_str(),
                    "last_reported_at": m.compared_at,
                    "has_baseline": m.snapshot_id.is_some(),
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(health.verdict.exit_code());
    }

    match &config {
        Some(config) => {
            println!("Configured in {}", config::path(paths).display());
            for line in describe_config(config) {
                println!("  {line}");
            }
        }
        None => println!(
            "Nothing is configured. Run \"snob watch setup\", or pass the schedule on the \
             command line."
        ),
    }

    println!();
    // Said before the marks, because it answers the question somebody opening
    // `status` actually has. A run that could not look moves no mark, so a
    // monitor that has been in a cooldown since Monday looks, from the marks
    // alone, exactly like one that was killed on Monday.
    if last_runs.is_empty() {
        println!("It has not run yet.");
    } else {
        for run in &last_runs {
            let pk = run.account_pk;
            let name = snob_core::store::users::name(db.conn(), pk)?;
            let who = crate::app::label(pk, name.as_deref());

            let mut line = format!("{who} last ran on {}", report::stored_on(run.started_at));
            if let Some(outcome) = &run.outcome
                && outcome != "ok"
            {
                line.push_str(&format!(" and could not look ({outcome})"));
            } else if run.changes == 0 {
                line.push_str(" and found nothing");
            } else {
                line.push_str(&format!(
                    " and found {} change{}",
                    run.changes,
                    if run.changes == 1 { "" } else { "s" }
                ));
            }
            println!("{line}.");
        }
    }

    println!();
    if marks.is_empty() {
        println!("The monitor has not reported on anything yet.");
    } else {
        for mark in &marks {
            let name = snob_core::store::users::name(db.conn(), mark.account_pk)?;
            let who = crate::app::label(mark.account_pk, name.as_deref());
            println!(
                "{who}: {} last reported on {}",
                mark.kind,
                report::stored_on(mark.compared_at)
            );
        }
    }

    // One block, because there used to be two and they contradicted each other:
    // one said a queue with no webhook would never move, the other said the next
    // run would try it, and both printed in that order on the same run. The
    // first also carried a run of literal spaces before its pronoun, which came
    // back when this was rewritten -- and came back longer. A test walks the
    // source for that shape now, because two rounds of reading it did not.
    if owed > 0 {
        println!();
        let (subject, it) = if owed == 1 {
            ("report is", "it")
        } else {
            ("reports are", "them")
        };
        if config.as_ref().and_then(|c| c.webhook.as_ref()).is_none() {
            println!(
                "{owed} {subject} queued, but no webhook is configured, so nothing will send {it}. {} expire on {} own.",
                if owed == 1 { "It" } else { "They" },
                if owed == 1 { "its" } else { "their" }
            );
        } else {
            println!("{owed} {subject} waiting to be delivered; the next run tries {it}.");
        }
    }

    if !health.notes.is_empty() {
        println!();
        println!("Health: {}", health.verdict.as_str());
        for note in &health.notes {
            println!("  {note}");
        }
    }

    // Non-zero when the monitor is not doing what it was configured to do, so
    // this is usable as a probe rather than only as something to read.
    Ok(health.verdict.exit_code())
}

/// Whether the monitor is doing what it was configured to do.
///
/// `status` was a historical report and always exited 0, so the only way to
/// know a monitor had stopped working was to read it — which is the same
/// problem the reports themselves have and which `--json` exists to solve. This
/// is the verdict, made of what `status` already reads.
///
/// Pure, and separate from the printing, because the interesting part is which
/// states count as broken and that is a thing worth pinning.
struct Health {
    verdict: Verdict,
    notes: Vec<String>,
}

/// The accounts the configuration names right now, as ids.
///
/// `None` means it could not be settled, and every run is then treated as
/// watched — the direction that does not fail a probe on a guess. That happens
/// with no configuration at all, and when the file's own account is meant on a
/// machine that has never recorded which one that is.
///
/// The predicate matches `watched_from`'s: an empty `[[account]]` list means the
/// viewer, and `self` names it explicitly.
fn watched_pks(db: &Store, config: Option<&WatchConfig>) -> Result<Option<Vec<snob_core::Pk>>> {
    let conn = db.conn();
    let Some(config) = config else {
        return Ok(None);
    };

    let own = config.accounts.is_empty() || config.accounts.iter().any(AccountConfig::is_own);
    let mut pks = Vec::new();
    if own {
        match snob_core::store::accounts::own(conn)? {
            Some(pk) => pks.push(pk),
            // The file watches this machine's own account and the machine has
            // never recorded which one that is. Nothing here can be settled.
            None => return Ok(None),
        }
    }
    for account in config.accounts.iter().filter(|a| !a.is_own()) {
        if let Some(pk) = snob_core::store::accounts::find_pk_by_username(conn, &account.target)? {
            pks.push(pk);
        }
    }
    Ok(Some(pks))
}

/// One account's newest run, with the two things [`health`] cannot work out for
/// itself: what to call the account, and whether the file still names it.
///
/// Built by `status`, which has the store open and is already resolving both
/// for its own lines.
struct RunOf<'a> {
    run: &'a watch_store::Run,
    /// `@name`, or the id when the name was never learned.
    who: String,
    /// Whether `watch.toml` still lists this account. `true` when it cannot be
    /// settled, which is the direction that does not fail a probe on a guess.
    watched: bool,
}

/// How many scheduled runs may be missed before silence is a failure rather
/// than a warning.
///
/// One missed run is a machine that was asleep, a laptop that was shut, a
/// cooldown that ran long. Three is nobody coming back.
const MISSED_BEFORE_FAILED: i64 = 3;

/// How long this configuration means the monitor should be silent for.
///
/// Asked of the schedule rather than guessed at, by taking the distance between
/// the next two moments it names: six hours for `--every 6h`, and a week for
/// `--on mon --at 09:00`, which is the point — a weekly monitor that has not run
/// since Tuesday is not late.
///
/// `None` when the file has no schedule this can build, or names one that never
/// fires. Both of those are their own line elsewhere and neither is a reason to
/// call the monitor late as well.
fn expected_gap(config: &WatchConfig, now: i64) -> Option<i64> {
    let schedule = super::watch::schedule_from(&Default::default(), Some(config)).ok()?;
    let first = schedule::next_moment(&schedule, Some(now), now, &chrono::Local)?;
    let second = schedule::next_moment(&schedule, Some(first), first, &chrono::Local)?;
    (second > first).then_some(second - first)
}

/// An account with exactly one of its two lists ever reported on.
///
/// Built by `status` from the marks it already reads. A mark moves only when a
/// list was actually compared, so a list with none while its sibling has one has
/// never been read — which nothing else in the tool can say.
struct HalfRead {
    who: String,
    reported: ListKind,
    /// The one that never has been, or `None` when both have.
    missing: Option<ListKind>,
}

fn health(
    config: Option<&WatchConfig>,
    runs: &[RunOf<'_>],
    reported: &[HalfRead],
    owed: usize,
    now: i64,
) -> Health {
    let mut notes = Vec::new();
    let mut verdict = Verdict::Ok;
    let mut at_least = |level: Verdict| verdict = verdict.max(level);

    if config.is_none() {
        at_least(Verdict::Warned);
        notes.push(
            "nothing is configured, so a bare \"snob watch\" has no schedule to run on".to_string(),
        );
    }

    if runs.is_empty() {
        at_least(Verdict::Warned);
        notes.push("it has not run yet".to_string());
    }

    // **One list being read while the other never is.**
    //
    // `watch_runs.outcome` is one column for a tick that covers two lists, and
    // `TickReport::looked()` asks `any`, not `all` — so a run where followers
    // completed and following was refused records `ok`, `status` takes the
    // "found nothing" branch, and the exit code is 0. Every run, forever, on
    // the account AGENTS.md files under "Known walls": `following` meets the
    // truncation wall on every walk while `followers` completes. There was no
    // probe anywhere that could tell that from a quiet account.
    //
    // Asked of the marks rather than of the run log, because the marks are
    // where the durable answer already is: a mark moves only when a list was
    // actually compared, so a list with none while its sibling has one has
    // never once been read. That needs no new column and no threshold, and it
    // catches the permanent case, which is the one that matters. A single
    // half-blind run is the payload's question, not this one.
    for half in reported.iter().filter(|r| r.missing.is_some()) {
        let (read, never) = (half.reported, half.missing.expect("filtered"));
        at_least(Verdict::Warned);
        notes.push(format!(
            "{}: {read} has been reported on and {never} never has, so one of the two \
             lists is not being read",
            half.who
        ));
    }

    // A schedule the scheduler refuses is a monitor that cannot start.
    //
    // `describe_config` prints the clauses without evaluating anything, so
    // `status` said "Runs every 5m" and exited 0 about a file that kills every
    // invocation at `schedule_from`. Nothing here built a schedule at all.
    if let Some(config) = config
        && let Err(e) = super::watch::schedule_from(&Default::default(), Some(config))
    {
        at_least(Verdict::Failed);
        notes.push(format!("the configured schedule cannot be built: {e}"));
    }

    // **Whether it is still running at all**, which nothing here used to ask.
    //
    // Runs were read for their `outcome` and nothing else, so as long as the
    // newest row per account had succeeded the verdict was `Ok` however old it
    // was — and `prune` keeps the newest row per account whatever its age,
    // precisely so this can read it, so it never aged into the "it has not run
    // yet" warning either. A unit that was disabled, a container nobody
    // restarted, a process the kernel killed: three weeks silent, verdict `Ok`,
    // exit 0, and the Health block not printed at all because `notes` was
    // empty. `002_watch.sql` says `watch_runs` exists because "a monitor that
    // quietly stopped looks exactly like a quiet account", and this is the
    // reader that was supposed to tell them apart.
    //
    // The gap comes from the schedule rather than from a guess, so a weekly
    // monitor is not called late after two days.
    if let Some(gap) = config.and_then(|c| expected_gap(c, now))
        && let Some(newest) = runs.iter().map(|r| r.run.started_at).max()
    {
        let silent = now - newest;
        let missed = silent / gap.max(1);
        if missed >= MISSED_BEFORE_FAILED {
            at_least(Verdict::Failed);
            notes.push(format!(
                "it has not run since {}, which is {missed} scheduled runs ago",
                report::stored_on(newest)
            ));
        } else if missed >= 1 {
            at_least(Verdict::Warned);
            notes.push(format!(
                "it has not run since {}, and one was due by now",
                report::stored_on(newest)
            ));
        }
    }

    // What stopped the last run of each account. A cooldown lifts on its own
    // and is worth saying rather than alarming about; a session that has gone
    // will not come back without somebody logging in, and every run until then
    // does nothing.
    //
    // **Only for accounts the file still names.** `last_runs` answers about
    // every account that has ever run, and it was never compared against the
    // configuration — so one old failed run for a stranger since removed from
    // `watch.toml` pinned the verdict at `Failed` for good, on a monitor with
    // nothing wrong with it. That is how people learn to ignore a probe. It is
    // still worth a line, because a row nobody watches is worth explaining, and
    // the line names the account: the note used to omit it, so several accounts
    // printed the identical sentence N times.
    for RunOf { run, who, watched } in runs {
        let Some(code) = run.outcome.as_deref().filter(|c| *c != "ok") else {
            continue;
        };
        if !watched {
            notes.push(format!(
                "{who} last ended in {code}, and the configuration no longer names it"
            ));
            continue;
        }
        match code {
            "rate_limited" | "interrupted" => {
                at_least(Verdict::Warned);
                notes.push(format!("{who}'s last run ended in {code}"));
            }
            _ => {
                at_least(Verdict::Failed);
                notes.push(format!("{who}'s last run ended in {code}"));
            }
        }
    }

    if owed > 0 {
        // Queued with nowhere to go is the worse half: those reports expire,
        // and the changes in them are already marked as reported, so they are
        // the only copy.
        if config.and_then(|c| c.webhook.as_ref()).is_none() {
            at_least(Verdict::Failed);
            notes.push(format!(
                "{owed} queued report(s) with no webhook configured to send them to"
            ));
        } else {
            at_least(Verdict::Warned);
            notes.push(format!("{owed} report(s) still waiting to be delivered"));
        }
    }

    Health { verdict, notes }
}

/// The configuration in a few lines, for `status` and for the confirmation
/// `setup` shows before replacing a file.
fn describe_config(config: &WatchConfig) -> Vec<String> {
    let mut lines = Vec::new();

    let when =
        report::schedule_clauses(config.every, &config.on, &config.at, config.cron.as_deref());
    lines.push(if when.is_empty() {
        "no schedule: it will not run until one is set".to_string()
    } else {
        format!("Runs {}", when.join(", "))
    });

    // What the file says, not what a `Schedule` would clamp it to. This is the
    // place a person reads their configuration back, and a hand-edited
    // `jitter = "1h"` reached no line of it at all — the one setting you could
    // write into the file and never see again.
    if let Some(jitter) = config.jitter
        && let Some(sentence) = report::jitter_sentence(jitter)
    {
        lines.push(sentence);
    }

    match &config.webhook {
        Some(webhook) => {
            lines.push(format!("Reports to {}", printable(&webhook.url)));
            if webhook.heartbeat {
                lines.push("Sends a report even when nothing changed".to_string());
            }
        }
        None => lines.push("Sends nothing: the report goes to standard output".to_string()),
    }

    lines.push(watching_line(config));
    lines
}

/// Who a run over this file would actually walk.
///
/// Derived from the same predicate `watched_from` runs on, because the two used
/// to disagree. This counted the strangers and then said "your account and N
/// other" regardless, while `watched_from` falls back to the viewer **only when
/// the list is empty** — so a hand-edited file naming one stranger is walked as
/// that stranger alone, and both of the two places a person reads the
/// configuration back said otherwise.
fn watching_line(config: &WatchConfig) -> String {
    let own = config.accounts.is_empty() || config.accounts.iter().any(AccountConfig::is_own);
    let others = config.accounts.iter().filter(|a| !a.is_own()).count();

    match (own, others) {
        (true, 0) => "Watches your account".to_string(),
        (true, n) => format!("Watches your account and {n} other{}", plural(n)),
        (false, n) => format!("Watches {n} account{}, and not your own", plural(n)),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> WatchConfig {
        config::parse(text, std::path::Path::new("watch.toml")).unwrap()
    }

    /// `--dry-run` does not read the file it has already decided not to touch.
    ///
    /// The read was the scrutinee of a let-chain, so its `?` fired before the
    /// flag could short-circuit, and `snob watch setup --dry-run` -- documented
    /// as "print what would be written and write nothing" -- died on an existing
    /// `watch.toml` with a typo in it. That is the file somebody runs
    /// `--dry-run` to compare against.
    ///
    /// Both directions, because only one of them is the defect: a real run has
    /// to keep reading, or it would replace a configuration without showing what
    /// it was replacing.
    #[test]
    fn a_dry_run_does_not_read_the_file_it_will_not_touch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        config::write(
            &paths,
            "schema = 1
evry = \"6h\"
",
        )
        .unwrap();

        assert!(
            existing_to_replace(&paths, true).unwrap().is_none(),
            "a dry run writes nothing, so it has nothing to ask about replacing"
        );
        assert!(
            existing_to_replace(&paths, false).is_err(),
            "a real run is about to overwrite it, so it still has to read it"
        );
    }

    #[test]
    fn it_describes_an_interval_and_a_webhook() {
        let lines = describe_config(&config(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"https://n8n.local/hook\"\nheartbeat = true\n",
        ));
        let text = lines.join("\n");
        assert!(text.contains("every 6h"), "{text}");
        assert!(text.contains("https://n8n.local/hook"), "{text}");
        assert!(text.contains("even when nothing changed"), "{text}");
    }

    /// A file with no webhook is a whole configuration, and saying so is what
    /// stops somebody thinking the delivery is broken.
    #[test]
    fn it_says_when_nothing_is_sent_anywhere() {
        let lines = describe_config(&config("schema = 1\nevery = \"6h\"\n"));
        assert!(lines.join("\n").contains("standard output"));
    }

    /// A hand-edited file can have neither half of a schedule, and then the
    /// monitor never runs. Saying "Runs" with nothing after it would read as
    /// though it were fine.
    #[test]
    fn a_file_with_no_schedule_says_it_will_not_run() {
        let lines = describe_config(&config("schema = 1\n"));
        assert!(lines[0].contains("will not run"), "{lines:?}");
    }

    #[test]
    fn it_counts_the_other_accounts() {
        let lines = describe_config(&config(
            "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"self\"\n\n\
             [[account]]\ntarget = \"someone\"\n[account.consent]\nagreed_at = 1\n",
        ));
        assert!(
            lines.join("\n").contains("your account and 1 other"),
            "{lines:?}"
        );
    }

    /// A file that names one stranger is walked as that stranger alone:
    /// `watched_from` falls back to the viewer only when the list is empty. Both
    /// of the two places a person reads the configuration back said otherwise,
    /// because this counted the strangers and then claimed the viewer anyway.
    #[test]
    fn it_does_not_claim_your_account_when_the_file_does_not_list_it() {
        let lines = describe_config(&config(
            "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"friend\"\n\
             [account.consent]\nagreed_at = 1\n",
        ));
        let text = lines.join("\n");

        assert!(!text.contains("your account"), "{text}");
        assert!(text.contains("1 account, and not your own"), "{text}");
    }

    /// A file with no `[[account]]` at all means the obvious thing, and this is
    /// the one shape where the old wording happened to be right by accident --
    /// it printed nothing.
    #[test]
    fn a_file_naming_nobody_watches_the_viewer() {
        let lines = describe_config(&config("schema = 1\nevery = \"6h\"\n"));
        assert!(
            lines.join("\n").contains("Watches your account"),
            "{lines:?}"
        );
    }

    /// The one setting somebody could write into the file and never see again.
    ///
    /// `status` and the replace-this-file confirmation are where a person reads
    /// their configuration back, and `WatchConfig.jitter` reached neither.
    #[test]
    fn a_configured_jitter_is_read_back() {
        let lines = describe_config(&config("schema = 1\nevery = \"6h\"\njitter = \"1h\"\n"));
        assert!(
            lines.join("\n").contains("pushed up to 1h later"),
            "{lines:?}"
        );

        let none = describe_config(&config("schema = 1\nevery = \"6h\"\njitter = \"0\"\n"));
        assert!(
            !none.join("\n").contains("pushed up to"),
            "a jitter that was turned off has nothing to say: {none:?}"
        );
    }

    /// The line `setup` writes has to be one the parser reads back. It is built
    /// as text rather than serialized, so nothing but this stops it drifting.
    #[test]
    fn the_schedule_lines_setup_writes_are_readable() {
        for line in [
            "every = \"6h\"",
            "on = [\"mon\", \"thu\"]\nat = [\"09:00\"]",
            "at = [\"09:00\", \"21:00\"]",
            "cron = \"0 9 * * 1,4\"",
        ] {
            let text = config::template(line, None, None, false, &[], &[], false);
            let parsed = config::parse(&text, std::path::Path::new("watch.toml"))
                .unwrap_or_else(|e| panic!("{line:?} did not read back: {e}"));
            assert!(
                parsed.every.is_some() || !parsed.at.is_empty() || parsed.cron.is_some(),
                "{line:?} produced a file with no schedule in it"
            );
        }
    }

    #[test]
    fn quoting_a_list_gives_toml_a_parser_accepts() {
        assert_eq!(quoted_list(&["mon", "thu"]), "\"mon\", \"thu\"");
        assert_eq!(quoted_list(&[]), "");
    }

    /// A fixed present, so how old a run is is something these tests state
    /// rather than something they inherit from the wall clock.
    const NOW: i64 = 1_700_000_000;

    /// A run that happened a minute ago.
    ///
    /// This used to be `started_at: 0`, which pinned the *absence* of a time
    /// axis as correct: the suite asserted `Ok` for a run at the epoch under a
    /// six-hourly schedule. Changing it looks like a regression and is the
    /// opposite of one.
    fn ran(outcome: &str) -> watch_store::Run {
        watch_store::Run {
            account_pk: 42,
            started_at: NOW - 60,
            finished_at: Some(NOW - 60),
            requests: 1,
            outcome: Some(outcome.to_string()),
            changes: 0,
        }
    }

    /// A run belonging to an account the file still names.
    fn of(run: &watch_store::Run) -> RunOf<'_> {
        RunOf {
            run,
            who: "@me".to_string(),
            watched: true,
        }
    }

    /// Which states count as a monitor that has stopped doing its job.
    ///
    /// `status` was a historical report that always exited 0, so the only way
    /// to find out was to read it -- which is the same problem the reports
    /// themselves have and which `--json` exists to solve. What is pinned here
    /// is the line between "working" and "not", because that is what an exit
    /// code is.
    #[test]
    fn a_healthy_monitor_is_told_apart_from_one_that_has_stopped_working() {
        let configured = config(
            "schema = 1
every = \"6h\"

[webhook]
url = \"https://n8n.internal/hook\"
",
        );

        let ok = ran("ok");
        assert_eq!(
            health(Some(&configured), &[of(&ok)], &[], 0, NOW).verdict,
            Verdict::Ok,
            "configured, ran just now, nothing owed"
        );

        // A cooldown lifts on its own, and an interrupt was the user. Neither
        // is a monitor that needs attention.
        for lifts in ["rate_limited", "interrupted"] {
            let run = ran(lifts);
            assert_eq!(
                health(Some(&configured), &[of(&run)], &[], 0, NOW).verdict,
                Verdict::Warned,
                "{lifts} passes on its own"
            );
        }

        // A session that has gone will not come back without somebody logging
        // in, and every run until then does nothing at all.
        let dead = ran("no_session");
        assert_eq!(
            health(Some(&configured), &[of(&dead)], &[], 0, NOW).verdict,
            Verdict::Failed
        );

        // Owed reports with somewhere to send them are a wait. With nowhere,
        // they expire -- and the changes in them are already marked as
        // reported, so they are the only copy.
        assert_eq!(
            health(Some(&configured), &[of(&ok)], &[], 2, NOW).verdict,
            Verdict::Warned
        );
        let no_webhook = config(
            "schema = 1
every = \"6h\"
",
        );
        assert_eq!(
            health(Some(&no_webhook), &[of(&ok)], &[], 2, NOW).verdict,
            Verdict::Failed
        );

        // And a machine with nothing configured is not broken, but a bare
        // `snob watch` there has no schedule to run on.
        let nothing = health(None, &[], &[], 0, NOW);
        assert_eq!(nothing.verdict, Verdict::Warned);
        assert_eq!(nothing.notes.len(), 2, "{:?}", nothing.notes);
    }

    /// A file whose schedule the scheduler refuses is a monitor that cannot
    /// start, and both read-only probes said it was fine.
    ///
    /// `check` discarded the error with `.ok()` and pushed a schedule line only
    /// on `Some`, so the report carried no schedule line at all and
    /// `verdict()` — `max().unwrap_or(Ok)` — exited 0. `health` never built a
    /// schedule, and `describe_config` prints the clauses without evaluating
    /// anything, so `status` printed "Runs every 5m" and exited 0 too.
    ///
    /// Every one of these gets into the file through a hand-edit, which the
    /// first line of `watch.toml` says is fine, and every one passes
    /// `config::parse` — which reads TOML, the schema number and one key clash.
    #[test]
    fn a_schedule_the_scheduler_refuses_is_not_healthy() {
        for refused in [
            "schema = 1\nevery = \"5m\"\n",
            "schema = 1\ncron = \"0 9 * *\"\n",
            "schema = 1\nat = [\"25:00\"]\n",
            "schema = 1\nevery = \"2w\"\non = [\"mon\"]\n",
        ] {
            let configured = config(refused);
            let ok = ran("ok");
            let health = health(Some(&configured), &[of(&ok)], &[], 0, NOW);
            assert_eq!(
                health.verdict,
                Verdict::Failed,
                "{refused:?} kills every run: {:?}",
                health.notes
            );
        }

        // And one that builds is still fine.
        let good = config("schema = 1\nevery = \"6h\"\n");
        let ok = ran("ok");
        assert_eq!(
            health(Some(&good), &[of(&ok)], &[], 0, NOW).verdict,
            Verdict::Ok
        );
    }

    /// One list being read while the other never is.
    ///
    /// `watch_runs.outcome` is one column for a tick covering two lists, and
    /// `TickReport::looked()` asks `any` rather than `all` — so a run where
    /// followers completed and following was refused records `ok`, `status`
    /// takes the "found nothing" branch, and the exit code is 0. Every run,
    /// forever, on the account AGENTS.md files under "Known walls". There was
    /// no probe anywhere that could tell that from a quiet account.
    ///
    /// Asked of the marks, because that is where the durable answer already
    /// is: a mark moves only when a list was actually compared.
    #[test]
    fn one_list_never_being_read_is_not_a_healthy_monitor() {
        let configured = config("schema = 1\nevery = \"6h\"\n");
        let ok = ran("ok");

        let half = HalfRead {
            who: "@me".to_string(),
            reported: ListKind::Followers,
            missing: Some(ListKind::Following),
        };
        let blind = health(Some(&configured), &[of(&ok)], &[half], 0, NOW);
        assert_eq!(blind.verdict, Verdict::Warned, "{:?}", blind.notes);
        assert!(
            blind
                .notes
                .iter()
                .any(|n| n.contains("following") && n.contains("@me")),
            "the line has to name the list and the account: {:?}",
            blind.notes
        );

        // Both read is the ordinary case and says nothing.
        let whole = HalfRead {
            who: "@me".to_string(),
            reported: ListKind::Followers,
            missing: None,
        };
        assert_eq!(
            health(Some(&configured), &[of(&ok)], &[whole], 0, NOW).verdict,
            Verdict::Ok
        );
    }

    /// A monitor that stopped is not a healthy monitor.
    ///
    /// Runs were read for their `outcome` and nothing else, so a successful run
    /// three weeks old came back `Ok` — and `prune` keeps the newest row per
    /// account whatever its age, precisely so this can read it, so it never
    /// aged into "it has not run yet" either. The Health block only prints when
    /// there are notes, so the text output said nothing at all. That is the
    /// likeliest real failure of an unattended service: a unit disabled, a
    /// container nobody restarted, a process the kernel killed.
    #[test]
    fn a_monitor_that_stopped_is_not_healthy() {
        let configured = config("schema = 1\nevery = \"6h\"\n");

        let recent = ran("ok");
        assert_eq!(
            health(Some(&configured), &[of(&recent)], &[], 0, NOW).verdict,
            Verdict::Ok
        );

        // One six-hour gap missed is a laptop that was shut.
        let late = watch_store::Run {
            started_at: NOW - 7 * 3_600,
            ..ran("ok")
        };
        let one = health(Some(&configured), &[of(&late)], &[], 0, NOW);
        assert_eq!(one.verdict, Verdict::Warned, "{:?}", one.notes);

        // Three weeks is nobody coming back.
        let gone = watch_store::Run {
            started_at: NOW - 21 * 86_400,
            ..ran("ok")
        };
        let stopped = health(Some(&configured), &[of(&gone)], &[], 0, NOW);
        assert_eq!(stopped.verdict, Verdict::Failed, "{:?}", stopped.notes);
        assert!(
            stopped
                .notes
                .iter()
                .any(|n| n.contains("has not run since")),
            "and it has to say so: {:?}",
            stopped.notes
        );
    }

    /// How late is late comes from the schedule, so a weekly monitor is not
    /// called late after two days.
    #[test]
    fn how_late_is_late_depends_on_the_schedule() {
        let two_days_ago = watch_store::Run {
            started_at: NOW - 2 * 86_400,
            ..ran("ok")
        };

        let weekly = config("schema = 1\non = [\"mon\"]\nat = [\"09:00\"]\n");
        assert_eq!(
            health(Some(&weekly), &[of(&two_days_ago)], &[], 0, NOW).verdict,
            Verdict::Ok,
            "two days is not late for a weekly schedule"
        );

        let six_hourly = config("schema = 1\nevery = \"6h\"\n");
        assert_ne!(
            health(Some(&six_hourly), &[of(&two_days_ago)], &[], 0, NOW).verdict,
            Verdict::Ok,
            "and it very much is for a six-hourly one"
        );
    }

    /// A run belonging to an account the file no longer names is worth a line,
    /// not a verdict.
    ///
    /// `last_runs` answers about every account that has ever run and was never
    /// compared against the configuration, so one old failure for a stranger
    /// since removed pinned the verdict at `Failed` for good — on a monitor
    /// with nothing wrong with it, which is how people learn to ignore a probe.
    /// The note omitted the account too, so several printed the identical
    /// sentence N times.
    #[test]
    fn a_run_for_an_account_nobody_watches_any_more_does_not_fail_the_verdict() {
        let configured = config("schema = 1\nevery = \"6h\"\n");
        let failed = ran("no_session");

        let orphan = RunOf {
            run: &failed,
            who: "@stranger".to_string(),
            watched: false,
        };
        let health = health(Some(&configured), &[orphan], &[], 0, NOW);

        assert_eq!(health.verdict, Verdict::Ok, "{:?}", health.notes);
        assert!(
            health.notes.iter().any(|n| n.contains("@stranger")),
            "the line has to say which account: {:?}",
            health.notes
        );
    }

    /// Setup refuses a schedule the scheduler would refuse anyway, but it does
    /// it while the person who typed it is still reading.
    ///
    /// This used to be one line asserting `Schedule::every(300).is_err()`, which
    /// is a fact about the schedule module and says nothing about whether
    /// `setup` asks it. The validation was unreachable behind `dialoguer`, so
    /// deleting it left the suite green while `every = "5m"` went into a file
    /// the scheduler then refuses at every run — and `config::parse` cannot
    /// catch that, because it checks the TOML and the schema number, not what
    /// the values mean.
    ///
    /// All three shapes, because the floor binds all three and only one of them
    /// was ever mentioned here.
    #[test]
    fn a_schedule_below_the_floor_is_refused_at_setup() {
        assert!(interval_line("5m").is_err(), "five minutes is too often");
        assert!(
            cron_line("*/5 * * * *").is_err(),
            "the same five minutes, written the other way"
        );
        assert!(
            calendar_line("", "09:00,09:05").is_err(),
            "and again, as two moments five minutes apart"
        );

        // And the lines a good answer produces, since the file is written from
        // them: TOML a parser accepts, in the keys `config::parse` reads.
        assert_eq!(interval_line("6h").unwrap(), "every = \"6h\"");
        assert_eq!(
            cron_line(" 0 9 * * 1,4 ").unwrap(),
            "cron = \"0 9 * * 1,4\""
        );
        assert_eq!(
            calendar_line("mon, thu", "09:00, 21:00").unwrap(),
            "on = [\"mon\", \"thu\"]\nat = [\"09:00\", \"21:00\"]"
        );
        assert_eq!(
            calendar_line("", "09:00").unwrap(),
            "at = [\"09:00\"]",
            "no days means every day, and no `on` key at all"
        );
    }

    /// The wizard and a run read a calendar with the same function now, so what
    /// one accepts the other accepts, and the refusals are word for word.
    ///
    /// They were two implementations of one contract: the same two maps, the
    /// same `Schedule::calendar`, down to the sentence that says what a day
    /// looks like. A contract with two implementations is one that can be
    /// half-changed, and this is the half where the person who could fix it is
    /// still at the keyboard.
    #[test]
    fn the_wizard_reads_a_calendar_the_way_a_run_does() {
        for (days, times) in [
            ("mon,thu", "09:00"),
            ("", "09:00,21:00"),
            ("tues", "09:00"),
            ("mon", "25:00"),
            ("", "09:00,09:05"),
        ] {
            let wizard = calendar_line(days, times);
            let run = super::super::watch::calendar_from(
                &days
                    .split(',')
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .collect::<Vec<_>>(),
                &times
                    .split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>(),
            );

            assert_eq!(
                wizard.is_ok(),
                run.is_ok(),
                "{days:?} {times:?}: the wizard and a run disagree"
            );
            if let (Err(a), Err(b)) = (&wizard, &run) {
                assert_eq!(a.to_string(), b.to_string(), "{days:?} {times:?}");
            }
        }
    }

    /// What comes back from those lines has to parse as the file it is going
    /// into, or `setup` writes something the monitor cannot read.
    #[test]
    fn the_lines_setup_writes_are_a_configuration_it_can_read_back() {
        for line in [
            interval_line("6h").unwrap(),
            cron_line("0 9 * * 1,4").unwrap(),
            calendar_line("mon", "09:00,21:00").unwrap(),
        ] {
            let text = format!("schema = 1\n{line}\n");
            config::parse(&text, std::path::Path::new("watch.toml"))
                .unwrap_or_else(|e| panic!("{line:?} does not parse back: {e}"));
        }
    }
}
