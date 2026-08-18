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
use snob_core::model::printable;
use snob_core::paths::AppPaths;
use snob_core::secret::Secret;
use snob_core::secrets::{Kind, SecretStore};
use snob_core::store::{Store, deliveries, watch as watch_store};
use snob_core::watch::config::{self, WatchConfig};
use snob_core::{duration, watch::schedule};

use crate::cli::{WatchSetupArgs, WatchStatusArgs};
use crate::exit::{ExitCode, ExitError};
use crate::{report, ui};

/// Walks somebody through configuring the monitor.
pub fn setup(args: WatchSetupArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
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

    if let Some(existing) = config::load(paths)?
        && !args.dry_run
    {
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
    ui::info("Start it with \"snob watch\", or put \"snob watch once\" on a timer.");
    Ok(ExitCode::Ok)
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

    let parsed_days = days
        .iter()
        .map(|d| {
            schedule::Weekday::parse(d)
                .ok_or_else(|| anyhow::anyhow!("\"{d}\" is not a day (try mon, thu)"))
        })
        .collect::<Result<Vec<_>>>()?;
    let parsed_times = times
        .iter()
        .map(|t| schedule::parse_time(t).map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()?;
    schedule::Schedule::calendar(&parsed_days, &parsed_times)?;

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

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "configured": config.is_some(),
                "config_path": config::path(paths).display().to_string(),
                "pending_deliveries": owed,
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
        return Ok(ExitCode::Ok);
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

    Ok(ExitCode::Ok)
}

/// The configuration in a few lines, for `status` and for the confirmation
/// `setup` shows before replacing a file.
fn describe_config(config: &WatchConfig) -> Vec<String> {
    let mut lines = Vec::new();

    let mut when = Vec::new();
    if let Some(every) = config.every {
        when.push(format!("every {}", duration::format(every)));
    }
    if !config.on.is_empty() {
        when.push(format!("on {}", printable(&config.on.join(", "))));
    }
    if !config.at.is_empty() {
        when.push(format!("at {}", printable(&config.at.join(", "))));
    }
    if let Some(cron) = &config.cron {
        when.push(format!("cron \"{}\"", printable(cron)));
    }
    lines.push(if when.is_empty() {
        "no schedule: it will not run until one is set".to_string()
    } else {
        format!("Runs {}", when.join(", "))
    });

    match &config.webhook {
        Some(webhook) => {
            lines.push(format!("Reports to {}", printable(&webhook.url)));
            if webhook.heartbeat {
                lines.push("Sends a report even when nothing changed".to_string());
            }
        }
        None => lines.push("Sends nothing: the report goes to standard output".to_string()),
    }

    let others = config.accounts.iter().filter(|a| !a.is_own()).count();
    if others > 0 {
        lines.push(format!(
            "Watches your account and {others} other{}",
            if others == 1 { "" } else { "s" }
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> WatchConfig {
        config::parse(text, std::path::Path::new("watch.toml")).unwrap()
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
        assert!(lines.join("\n").contains("1 other"), "{lines:?}");
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
