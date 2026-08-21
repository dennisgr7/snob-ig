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
use snob_core::watch::config::{self, WatchConfig};
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

    // The configuration is built up rather than rendered piece by piece: each
    // question puts back the fields it validated, and the file is written from
    // the same type a run reads. `config::template` says why.
    let mut config = ask_schedule()?;
    let answers = ask_webhook()?;
    // The address goes down with it. What somebody agrees to when they answer
    // for a stranger is read later as the authority for two different acts, and
    // the wizard is holding the second one at the moment it asks.
    let accounts = ask_accounts(answers.url.as_deref())?;

    config.webhook = answers.url.as_ref().map(|url| config::WebhookConfig {
        url: url.clone(),
        headers: answers.headers.iter().cloned().collect(),
        heartbeat: answers.heartbeat,
    });
    config.accounts = accounts
        .into_iter()
        .map(|(target, consent)| config::AccountConfig {
            target,
            consent: consent.map(|agreed_at| config::ConsentConfig { agreed_at }),
        })
        .collect();

    let text = config::template(&config, answers.signing_key.is_some());

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
    answers.store_secrets(&secrets)?;

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
/// exact — it is an estimate offered to a person, labeled as one.
const ACCOUNTS_PER_REQUEST: u64 = 25;

/// What taking the baseline now would walk, over every account it covers.
///
/// The sentence somebody agrees to was built with `find_map`, which stops at the
/// first `What::Account` carrying a pair of counters, while `baseline_now` walks
/// every `[[account]]` in the file. The two accounts the wizard itself writes
/// are enough to part them: `self`, which usually has captures already and so
/// supplies the counters, and the stranger somebody has just added, who has none
/// and is the reason the offer is being made. The screen described one account's
/// two lists in front of a walk over four — and the stranger's pair goes out at
/// `Pace::third_party()`, whose every wait is two to three times the default, so
/// the half left out of the sentence is also the slow half. This is the one
/// place in the tool where hundreds of requests are spent on a bare
/// confirmation, and the number under it has to be about the walk that follows.
///
/// It is an upper bound rather than a forecast, and deliberately so: an account
/// whose counters have not moved since a capture nobody reported on is served
/// out of storage for one request instead of walked. Overstating what a
/// confirmation costs is the safe direction; `unwrap_or((0, 0))` understated it,
/// which is how "roughly 0 requests" came to stand in front of a full walk.
struct BaselineCost {
    /// How many accounts the walk covers.
    accounts: usize,
    followers: u64,
    following: u64,
    /// Roughly how many requests they come to, accumulated per account rather
    /// than by dividing the totals: each account pages separately, so the
    /// rounding belongs to each of them.
    requests: u64,
    /// How many accounts the preflight could not read counters for. An account
    /// it could not ask about — a cooldown standing, or Instagram naming
    /// nobody — is walked all the same, so it is counted and said rather than
    /// folded in as zero.
    uncounted: usize,
}

impl BaselineCost {
    /// The sentence somebody agrees to, naming what it covers.
    fn sentence(&self) -> String {
        let mut line = format!(
            "Taking it now means walking {} followers and {} following across {} account{}: \
             roughly {} requests, a few minutes.",
            self.followers,
            self.following,
            self.accounts,
            plural(self.accounts),
            self.requests
        );
        if self.uncounted > 0 {
            line.push_str(&format!(
                " {} of them could not be counted, so it is longer than that.",
                self.uncounted
            ));
        }
        line
    }
}

/// Adds up what the preflight learned about every account it checked.
fn baseline_cost(report: &crate::engine::check::CheckReport) -> BaselineCost {
    use crate::engine::check::What;

    let mut cost = BaselineCost {
        accounts: 0,
        followers: 0,
        following: 0,
        requests: 0,
        uncounted: 0,
    };
    for checked in &report.checked {
        let What::Account {
            followers,
            following,
            ..
        } = &checked.what
        else {
            continue;
        };
        cost.accounts += 1;
        match (followers, following) {
            (Some(a), Some(b)) => {
                cost.followers += a;
                cost.following += b;
                cost.requests +=
                    a.div_ceil(ACCOUNTS_PER_REQUEST) + b.div_ceil(ACCOUNTS_PER_REQUEST);
            }
            _ => cost.uncounted += 1,
        }
    }
    cost
}

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
    // Nothing to offer if a run could not happen anyway, or if there is already
    // something to compare against.
    //
    // Asked of the report rather than derived here out of the same `Vec` length.
    // The check's line and this offer answer one question between them — the
    // line explains why a first run says nothing, the offer is what stops it
    // happening — and while they were two tests in two layers, the change that
    // made a baseline mean "reported on" rather than "captured" had to be made
    // in both. Made in one, the wizard goes silent about exactly the state the
    // check has just started warning about.
    if report.verdict() == Verdict::Failed {
        return Ok(());
    }
    if !report.wants_a_baseline() || !ui::can_show_a_menu() {
        return Ok(());
    }

    println!();
    ui::info(&format!(
        "There is no capture to compare against yet, so the first scheduled run \
         will lay one down and report nothing.\n{}",
        baseline_cost(report).sentence()
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

/// The schedule questions, as the configuration they settle.
///
/// **The fields, not a rendered line.** The line was `config::template`'s only
/// route to the schedule, so the file it wrote could not be compared against the
/// configuration it stood for; `config::template` has the whole of it.
fn ask_schedule() -> Result<WatchConfig> {
    let choice = ui::choose(
        "How often should it look?",
        &[
            "Every so often (6h, 2d, 2w)",
            "On certain days, at certain times",
            "A cron expression I already have",
        ],
    )?
    .ok_or_else(|| ExitError::new(ExitCode::Interrupted, "nothing was changed".to_string()))?;

    let mut config = WatchConfig {
        schema: config::SCHEMA,
        every: None,
        at: vec![],
        on: vec![],
        cron: None,
        jitter: None,
        webhook: None,
        accounts: vec![],
    };

    match choice {
        0 => {
            config.every = Some(interval_of(&ui::prompt_line(
                "How often? (for example 6h)",
            )?)?)
        }
        1 => {
            let days = ui::prompt_line("Which days? (mon,thu -- or blank for every day)")?;
            let times = ui::prompt_line("At what times? (09:00 or 09:00,21:00)")?;
            let (on, at) = calendar_of(&days, &times)?;
            config.on = on;
            config.at = at;
        }
        _ => {
            config.cron = Some(cron_of(&ui::prompt_line(
                "The expression? (for example 0 9 * * 1,4)",
            )?)?);
        }
    }

    // The schedule a run would build out of what has just been answered, which
    // is what decides how much room a jitter has -- and, on the way, the last
    // gap between what the wizard accepts and what a run accepts: this is the
    // function `snob watch` itself calls, over the configuration about to be
    // written rather than over one answer at a time.
    let schedule = super::watch::schedule_from(&Default::default(), Some(&config))?;
    config.jitter = ask_jitter(&schedule)?;

    Ok(config)
}

/// How far a run may be pushed past its moment, asked against the schedule that
/// was just settled.
///
/// **It is asked because the read side is fully wired and the write side never
/// wrote it.** `WatchConfig::jitter` is read by `when_from`, clamped by
/// `Schedule::with_jitter`, printed by `describe_config` and announced by the
/// banner — and `config::template`'s explanation of it had no caller that could
/// pass a value, so it has never been written into a real file. The only route
/// to the setting was `--help` or the source.
///
/// **And it is validated the way every other answer here is.** The schedule
/// answers go through the very functions a run parses with, so that "5m" is
/// refused while the person who typed it is still reading. Jitter is the one
/// value whose validation can answer zero without saying anything:
/// `Schedule::with_jitter` clamps silently to what the grid can absorb, and the
/// room is nothing at all for `*/15 * * * *` and for `--every 2w --on mon`, both
/// of which this tool advertises. A hand-written `jitter = "10m"` on either
/// produces a file whose `status` reads back "Each run is pushed up to 10m
/// later" while every run lands on its moment.
///
/// `Schedule::with_jitter(Duration::MAX).jitter()` is the room, exactly:
/// `with_jitter` is `jitter.min(room_for_jitter())`, and `room_for_jitter` stays
/// private because nothing outside the schedule should be doing that arithmetic
/// itself.
///
/// `None` is "leave the key out", not "no jitter": the schedule's own default
/// then applies, which is what somebody who pressed Enter meant. `"0"` is how
/// the setting is turned off, and it writes `jitter = "0"`.
fn ask_jitter(schedule: &schedule::Schedule) -> Result<Option<std::time::Duration>> {
    let room = room_for(schedule);
    if room.is_zero() {
        ui::info(
            "This schedule has no room to be pushed later: the gap between two runs is \
             already the smallest one allowed, so every run happens on its moment.",
        );
        return Ok(None);
    }

    let typed = ui::prompt_line(&format!(
        "How far may a run be pushed later, so it does not land on the same second every \
         time? (blank for {}, \"0\" for none, at most {})",
        duration::format(schedule.jitter()),
        duration::format(room)
    ))?;
    if typed.trim().is_empty() {
        return Ok(None);
    }

    let wanted = duration::parse(typed.trim()).map_err(|e| anyhow::anyhow!(e))?;
    // Through the clamp a run applies, rather than a comparison written again
    // here. A value this schedule cannot absorb is refused now instead of
    // quietly becoming a smaller one at every start.
    if schedule.clone().with_jitter(wanted).jitter() < wanted {
        bail!(
            "{} is more room than this schedule has. At most {} can be taken out of the gap \
             between two runs without costing the next one.",
            duration::format(wanted),
            duration::format(room)
        );
    }
    Ok(Some(wanted))
}

/// The most this schedule could absorb, asked of the schedule.
///
/// `room_for_jitter` is private and stays private: a second copy of that
/// arithmetic out here is how the wizard and a run come to disagree.
/// `with_jitter` is `jitter.min(room_for_jitter())`, so the largest jitter it
/// will accept is exactly the room.
fn room_for(schedule: &schedule::Schedule) -> std::time::Duration {
    schedule
        .clone()
        .with_jitter(std::time::Duration::MAX)
        .jitter()
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

/// The `every` field, or what is wrong with the interval.
fn interval_of(text: &str) -> Result<std::time::Duration> {
    let every = duration::parse(text).map_err(|e| anyhow::anyhow!(e))?;
    // Validated here rather than at the first run, so "5m" is refused while the
    // person who typed it is still reading.
    schedule::Schedule::every(every)?;
    Ok(every)
}

/// The `on` and `at` fields, or what is wrong with the calendar.
fn calendar_of(days: &str, times: &str) -> Result<(Vec<String>, Vec<String>)> {
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

    Ok((
        days.iter().map(|d| (*d).to_string()).collect(),
        times.iter().map(|t| (*t).to_string()).collect(),
    ))
}

/// The `cron` field, or what is wrong with the expression.
fn cron_of(expression: &str) -> Result<String> {
    schedule::Schedule::cron(expression)?;
    Ok(expression.trim().to_string())
}

/// What the webhook questions settled.
///
/// **Named fields, because two of them were `Option<Secret>` and the two are not
/// interchangeable.** They were elements four and five of a tuple, told apart by
/// position across three sites, and swapping them compiles: the signing key
/// would be sent to the user's server as the `Authorization` value on every
/// request, and the token — which that server already holds — would become the
/// shared secret every signature is computed with, so a signature would prove
/// nothing to the only party that checks it. Nothing downstream can notice
/// either half. `plan` reads whichever secret is under `Kind::WatchToken` and
/// puts it in a header; `sign` reads whichever is under `Kind::WatchSigningKey`
/// and MACs the body with it. Neither has any way to know what it was given.
///
/// [`WebhookAnswers::store_secrets`] is the only thing that writes them, and it
/// is one function so there is one place to read the pairing off.
struct WebhookAnswers {
    /// Where reports go, exactly as typed. `None` is no webhook at all, and then
    /// everything below is empty.
    url: Option<String>,
    heartbeat: bool,
    /// The extra headers that go in the file.
    headers: Vec<(String, String)>,
    /// The shared secret the body is signed with. Goes to
    /// [`Kind::WatchSigningKey`] and is **never sent anywhere** — only a MAC
    /// computed from it is.
    signing_key: Option<Secret>,
    /// The whole `Authorization` header value. Goes to [`Kind::WatchToken`] and
    /// **is sent**, verbatim, to the user's server on every request.
    token: Option<Secret>,
}

impl WebhookAnswers {
    /// Puts the two secrets in the keyring, each under the `Kind` that says what
    /// it is for.
    ///
    /// One function, and the only one, so the pairing is in one place and a test
    /// can assert it. It was two calls in the middle of `setup`, which is behind
    /// a terminal check and therefore unreachable from a test — so the one line
    /// in the program where a signing key could become a bearer token had
    /// nothing watching it at all.
    fn store_secrets(self, secrets: &SecretStore) -> Result<()> {
        store_secret(secrets, Kind::WatchSigningKey, self.signing_key)?;
        store_secret(secrets, Kind::WatchToken, self.token)?;
        Ok(())
    }
}

fn ask_webhook() -> Result<WebhookAnswers> {
    if !ui::confirm("Send each report to a webhook?", true)? {
        ui::info(
            "Nothing will be sent. \"snob watch --json >> events.ndjson\" is a complete way to \
             use it without one.",
        );
        return Ok(WebhookAnswers {
            url: None,
            heartbeat: false,
            headers: vec![],
            signing_key: None,
            token: None,
        });
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
    let mut typed = Vec::new();
    if ui::confirm("Does it need any other headers?", false)? {
        loop {
            let line = ui::prompt_line(
                "Name: value (blank when there are no more, a name typed twice keeps the last)",
            )?;
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
            typed.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    let headers = last_of_each(typed);

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

    // By name. This was a five-element tuple whose fourth and fifth elements
    // were both `Option<Secret>`, so the one construction site that could put
    // the signing key where the token goes did it by ordering two lines.
    Ok(WebhookAnswers {
        url: Some(url.trim().to_string()),
        heartbeat,
        headers,
        signing_key,
        token,
    })
}

/// The headers a run of typed answers describes, a name given twice keeping the
/// last value.
///
/// The answers were collected straight into a `Vec`, which can hold a name the
/// file format cannot: `[webhook.headers]` is a TOML table, and TOML refuses a
/// duplicate key. So typing `X-Api-Key: a` and then correcting it to
/// `X-Api-Key: b` — the ordinary way somebody fixes a value they mistyped —
/// produced a file `config::parse` could not read. That parse is deliberately
/// the last thing before anything is written and before either secret is
/// stored, so the wizard aborted with "the configuration this produced could not
/// be read back; this is a bug" once every question had been answered and both
/// secrets typed, kept none of it, and told the user it was the tool's fault
/// rather than which line to change. `webhook::check` is no help here: it
/// validates each pair on its own, and each of the two is fine.
///
/// Sorted rather than left in the order they were typed, which costs nothing:
/// `WebhookConfig::headers` is a `BTreeMap`, so that is the order they come back
/// out of the file anyway.
fn last_of_each(typed: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut headers = std::collections::BTreeMap::new();
    for (name, value) in typed {
        headers.insert(name, value);
    }
    headers.into_iter().collect()
}

fn ask_accounts(webhook: Option<&str>) -> Result<Vec<(String, Option<i64>)>> {
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
            if !ui::confirm(&consent_question(name, webhook), false)? {
                continue;
            }
            accounts.push((name.to_string(), Some(snob_core::store::now())));
        }
    }
    Ok(accounts)
}

/// The question the recorded consent is an answer to.
///
/// **It names the webhook, because the answer is used for two things.** What
/// goes into `[account.consent]` is consent to *read* somebody else's lists, and
/// `Watched::may_run_unattended` then reads it, unchanged, as the authority for
/// posting that person's arrivals and departures — `username`, `full_name` and
/// `profile_url`, out of `account_json` — to a third-party server on every run.
/// Both prompts framed the question entirely as risk to the *user's own*
/// account: heavier request, readier refusal. Neither mentioned the other
/// person. The wizard makes it sharpest, because `ask_webhook` runs one line
/// above `ask_accounts` and the address is already in hand while the question is
/// being asked — and `describe_config` then printed "Reports to https://…" and
/// "Watches your account and 1 other" as two unrelated lines.
///
/// Split from the prompting so it can be read. Everything around it is behind
/// `dialoguer`, which answers nothing without a terminal.
///
/// The residue, written down rather than left to be found: an attended
/// `snob watch once <stranger> --webhook …` still asks a question that says
/// nothing about forwarding. That one belongs in `commands::watch::once` and not
/// in `engine::ask_consent_with`, which has no access to the delivery
/// configuration and must not be given one — `ListArgs` carries none, and
/// putting a `WatchConfig` inside `engine::list` is the layer AGENTS.md keeps
/// free of how anything is delivered.
fn consent_question(name: &str, webhook: Option<&str>) -> String {
    match webhook {
        Some(url) => format!(
            "Record that you agreed to read @{}'s lists, and to have what changes in them sent \
             to {}?",
            printable(name),
            printable(url)
        ),
        None => format!(
            "Record that you agreed to read @{}'s lists?",
            printable(name)
        ),
    }
}

/// Reports what is configured and what has happened.
pub fn status(args: WatchStatusArgs, paths: &AppPaths) -> Result<ExitCode> {
    let config = config::load(paths)?;
    // Opened read-only in spirit: `Store::open` creates the schema, which is
    // what every other command does anyway, and nothing here writes.
    let db = Store::open(paths)?;

    // The address this configuration could post to, spelled the way the outbox
    // records it — `status` builds no delivery of its own, so it goes through
    // the same function `delivery_from` does rather than comparing the file's
    // raw string against a normalized URL.
    let destination = config
        .as_ref()
        .and_then(|c| c.webhook.as_ref())
        .and_then(|w| url::Url::parse(&w.url).ok())
        .map(|url| super::watch::delivery::destination_of(&url));
    let owed = deliveries::owed(db.conn(), destination.as_deref())?;
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
                // The whole queue, unchanged, so a probe reading this field
                // still gets what it always did — and beside it the split,
                // because "the next run tries these" and "nothing here can send
                // these" are two facts and this was one number.
                "pending_deliveries": owed.waiting + owed.elsewhere,
                "deliveries": {
                    "waiting": owed.waiting,
                    "elsewhere": owed.elsewhere,
                    // Not owed -- owing has ended. Deliberately outside
                    // `pending_deliveries`, which counts work still to do:
                    // this is work that will never be done, and a probe that
                    // added it to the queue length would report a backlog
                    // that no run can shorten.
                    "given_up": owed.given_up,
                },
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
                && outcome != ExitCode::Ok.as_str()
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

    // Two lines, and this time they are about two different sets of rows.
    //
    // They used to be two claims about the *same* rows and they contradicted
    // each other: one said a queue with no webhook would never move, the other
    // said the next run would try it, and both printed in that order on the same
    // run. Merging them into one sentence was the wrong repair, because the
    // number underneath was `pending` — every row in the state — while only the
    // ones addressed here are ever handed back. So the sentence was true of some
    // of them and false of the rest, and which was which was exactly what the
    // reader needed. The counts are split now, so each line is true of the rows
    // it counts. The first version also carried a run of literal spaces before
    // its pronoun, which came back when it was rewritten -- and came back
    // longer. A test walks the source for that shape now, because two rounds of
    // reading it did not.
    if owed.waiting > 0 || owed.elsewhere > 0 || owed.given_up > 0 {
        println!();
    }
    if owed.given_up > 0 {
        let (subject, what) = if owed.given_up == 1 {
            ("report was", "What it said is")
        } else {
            ("reports were", "What they said is")
        };
        println!(
            "{} {subject} given up on for being too old to be news. {what} not reported \
             a second time.",
            owed.given_up
        );
    }
    if owed.waiting > 0 {
        let (subject, it) = if owed.waiting == 1 {
            ("report is", "it")
        } else {
            ("reports are", "them")
        };
        println!(
            "{} {subject} waiting to be delivered; the next run tries {it}.",
            owed.waiting
        );
    }
    if owed.elsewhere > 0 {
        // The verb is in the tuple with everything else it has to agree with.
        // It was not, so the singular arm read "It expire on its own." — in the
        // output of the command the README tells people to point a monitoring
        // system at, and reachable in the ordinary case of exactly one report
        // left over after the webhook address moved.
        let (subject, they, expire, their, it) = if owed.elsewhere == 1 {
            ("report is", "It", "expires", "its", "it")
        } else {
            ("reports are", "They", "expire", "their", "them")
        };
        println!(
            "{} {subject} addressed to a webhook this configuration does not send to, so \
             nothing here will try {it}. {they} {expire} on {their} own.",
            owed.elsewhere
        );
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
/// **Asked of `watched_from`, which is the function that decides it.** The
/// predicate was written out here as well — an empty `[[account]]` list means
/// the viewer, `self` names it explicitly, and the at sign comes off the name
/// before it is looked up — which made this the third hand-written copy of one
/// rule and the second one that had to be repaired separately when the at sign
/// did. With `target = "@friend"` the lookup found nobody, so every run of that
/// account was scored as belonging to an account the configuration no longer
/// names, which is deliberately a note and never a verdict: a monitor whose one
/// watched account fails every run then exits 0 for ever.
///
/// `None` for the target, because there is no command line here: this is what a
/// scheduled run over this file alone would walk.
fn watched_pks(db: &Store, config: Option<&WatchConfig>) -> Result<Option<Vec<snob_core::Pk>>> {
    let conn = db.conn();
    let Some(config) = config else {
        return Ok(None);
    };

    let watched = super::watch::watched_from(None, Some(config));
    let mut pks = Vec::new();
    if watched.iter().any(|w| w.name().is_none()) {
        match snob_core::store::accounts::own(conn)? {
            Some(pk) => pks.push(pk),
            // The file watches this machine's own account and the machine has
            // never recorded which one that is. Nothing here can be settled.
            None => return Ok(None),
        }
    }
    for named in watched.iter().filter_map(|w| w.name()) {
        if let Some(pk) = snob_core::store::accounts::find_pk_by_username(conn, named)? {
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
    owed: deliveries::Owed,
    now: i64,
) -> Health {
    let mut notes = Vec::new();
    let mut verdict = Verdict::Ok;
    let mut at_least = |level: Verdict| verdict = verdict.max(level);

    if config.is_none() {
        at_least(Verdict::Warned);
        notes.push(crate::report::NOTHING_CONFIGURED.to_string());
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
        let Some(code) = run
            .outcome
            .as_deref()
            .filter(|c| *c != ExitCode::Ok.as_str())
        else {
            continue;
        };
        if !watched {
            notes.push(format!(
                "{who} last ended in {code}, and the configuration no longer names it"
            ));
            continue;
        }
        // **Read as an `ExitCode`, not against three literals spelled again
        // here.** `as_str`'s own doc says the vocabulary exists "so nothing has
        // to invent tokens inline, which is how two spellings of one condition
        // get shipped", and this was the place that invented them. Respell
        // `rate_limited` there and every recorded cooldown lands in the arm
        // below, so `status` exits 1 for a monitor that will resume on its own —
        // and the fixture these tests build their rows from spelled the same
        // literals, so it would have moved with the defect.
        //
        // A token this build does not know is a failure it cannot explain, which
        // is the direction a probe should be wrong in.
        match ExitCode::from_token(code) {
            // A cooldown lifts by itself and an interrupt was the user. Neither
            // is a monitor that needs anybody.
            Some(ExitCode::RateLimited | ExitCode::Interrupted) => {
                at_least(Verdict::Warned);
                notes.push(format!("{who}'s last run ended in {code}"));
            }
            _ => {
                at_least(Verdict::Failed);
                notes.push(format!("{who}'s last run ended in {code}"));
            }
        }
    }

    // **Which queue it is, asked of the queue.** One integer answered two
    // questions and this branch read it wrongly in both directions. The count
    // was `pending` — every row in the state — so a monitor whose `[webhook]
    // url` had moved was scored on rows `due` can never return; and whether
    // those rows were orphans was asked of the *file*, so a run given the
    // address on the command line from a unit with no `watch.toml` — the
    // README's own example — came back `Failed` and exit 1 on every poll, while
    // its rows carried a real address the next run drains. A probe that pages
    // for a healthy service is how people learn to ignore a probe.
    if owed.elsewhere > 0 {
        if config.and_then(|c| c.webhook.as_ref()).is_some() {
            // The file names an address and these are for a different one, so
            // nothing will ever post them: they expire where they are, and the
            // changes in them are already marked as reported, which makes them
            // the only copy.
            at_least(Verdict::Failed);
            notes.push(format!(
                "{} queued report(s) are addressed somewhere this configuration does not send \
                 to, so nothing here will try them",
                owed.elsewhere
            ));
        } else {
            // With no address in the file, a run given `--webhook` and a
            // `[webhook]` somebody deleted look identical from here, and
            // guessing in the alarming direction is what failed the supported
            // one. Worth a line, not a verdict — the direction `watched_pks`
            // already takes when it cannot settle who is watched.
            at_least(Verdict::Warned);
            notes.push(format!(
                "{} queued report(s) are addressed to a webhook this file does not name, so \
                 only a run given --webhook can send them",
                owed.elsewhere
            ));
        }
    }

    if owed.waiting > 0 {
        at_least(Verdict::Warned);
        notes.push(format!(
            "{} report(s) still waiting to be delivered",
            owed.waiting
        ));
    }

    // A report given up on is not a failure of the monitor -- it is usually a
    // receiver that was down for a day -- but it must not be silent, and it
    // must not be the thing that lets the verdict go green. It used to be
    // exactly that: `owed` counted only `pending`, so a row stopped being
    // counted at the instant it stopped being deliverable, and the verdict went
    // from `warning` to `ok` the moment the change was thrown away.
    if owed.given_up > 0 {
        at_least(Verdict::Warned);
        notes.push(format!(
            "{} report(s) were given up on for being too old to be news",
            owed.given_up
        ));
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
            // Redacted, not just filtered. `webhook::check` refuses an address
            // carrying a password and its comment says why: it "would be
            // echoed by `status`". This is `status`, and it echoed it.
            let address = url::Url::parse(&webhook.url).map_or_else(
                |_| webhook.url.clone(),
                |u| crate::watch::webhook::shown(&u),
            );
            lines.push(format!("Reports to {}", printable(&address)));
            if webhook.heartbeat {
                lines.push("Sends a report even when nothing changed".to_string());
            }
        }
        // What the **file** says, which is not the whole of where a report can
        // go. `--webhook` on the command line is a supported way to run this and
        // leaves nothing here to read back, so a flat "sends nothing" described
        // a monitor that delivers on every run as one that does not — the same
        // wrong reading the health verdict made of the same field.
        None => lines.push(
            "No address here: the report goes to standard output unless a run is given \
             --webhook"
                .to_string(),
        ),
    }

    lines.push(watching_line(config));

    // The two facts above are one fact, and they were printed as two unrelated
    // lines: "Reports to https://…" and "Watches your account and 1 other".
    // Somebody else's names leaving this machine on every run is the part of
    // this configuration a person would most want to be reminded of, and it is
    // the part nothing said. "Their" rather than a count, because
    // `watching_line` directly above has just said how many.
    if let Some(webhook) = &config.webhook
        && config.accounts.iter().any(|a| !a.is_own())
    {
        lines.push(format!(
            "Their usernames and names go to {} with every report",
            printable(&webhook.url)
        ));
    }

    lines
}

/// Who a run over this file would actually walk.
///
/// **Asked of `watched_from`, which is the function that decides it.** This doc
/// already said it was derived from that predicate "because the two used to
/// disagree", and then re-derived the predicate by hand a line below — so
/// nothing enforced the agreement, and the shape that made them disagree is
/// still the shape that is hard: a file naming one stranger and not you.
/// `watched_from` falls back to the viewer only when the account list is empty,
/// and this counted strangers and claimed the viewer regardless. Reading it
/// through the same function means the sentence cannot be right about a run that
/// does something else, whatever either of them is changed to next.
///
/// `None` for the target, because there is no command line here: this is what a
/// scheduled run over this file alone would walk.
fn watching_line(config: &WatchConfig) -> String {
    let watched = super::watch::watched_from(None, Some(config));
    let own = watched.iter().any(|w| w.name().is_none());
    let others = watched.iter().filter(|w| w.name().is_some()).count();

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

    /// The offer describes the walk it is about to make.
    ///
    /// `baseline_now` walks every `[[account]]` the file names, and the sentence
    /// under the confirmation was built from the first account line carrying a
    /// pair of counters. The wizard writes two accounts the moment somebody adds
    /// a friend, and the first of them is `self`, which usually has the captures
    /// already -- so the account that made the offer necessary is the one left
    /// out of the sentence, and it is also the slow one, walked at
    /// `Pace::third_party()`. This is the only bare confirmation in the tool
    /// that spends hundreds of requests.
    ///
    /// Asked of `baseline_cost` and not of `offer_the_baseline`: that one prints
    /// and prompts, and returns before the sentence without a terminal, so while
    /// the numbers were worked out inside it nothing could reach them.
    #[test]
    fn the_offer_describes_the_walk_it_is_about_to_make() {
        use crate::engine::check::{CheckReport, Checked, What};

        let account = |followers, following| Checked {
            what: What::Account {
                target: None,
                pk: Some(1),
                followers,
                following,
                may_run_unattended: true,
            },
            verdict: Verdict::Ok,
            problem: None,
        };

        let both = CheckReport {
            checked: vec![account(Some(512), Some(340)), account(Some(88), Some(12))],
        };
        let cost = baseline_cost(&both);
        assert_eq!(
            (cost.accounts, cost.followers, cost.following),
            (2, 600, 352),
            "every account the walk covers, not the first one with counters on it"
        );
        assert_eq!(
            cost.requests,
            35 + 5,
            "counted per account, because each of them pages on its own"
        );

        let sentence = cost.sentence();
        assert!(sentence.contains("600 followers"), "{sentence}");
        assert!(sentence.contains("2 accounts"), "{sentence}");

        // An account the preflight could not size is walked all the same, so it
        // is said rather than folded in as nothing -- which is how "roughly 0
        // requests" used to be printed in front of a full walk.
        let partial = CheckReport {
            checked: vec![account(Some(512), Some(340)), account(None, None)],
        };
        let cost = baseline_cost(&partial);
        assert_eq!((cost.accounts, cost.uncounted), (2, 1));
        assert_eq!(cost.requests, 35);
        assert!(
            cost.sentence().contains("could not be counted"),
            "{}",
            cost.sentence()
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

    /// Consent recorded about one act is read as authority for another, so the
    /// question has to name both.
    ///
    /// `[account.consent] agreed_at` is an answer to "may this read @friend's
    /// lists?", and `Watched::may_run_unattended` then reads it as the authority
    /// for POSTing @friend's arrivals and departures -- username, full name and
    /// profile URL -- to a third-party server on every run. Both prompts framed
    /// the question as risk to the user's own account and neither mentioned the
    /// other person; the wizard already holds the address when it asks, because
    /// `ask_webhook` runs one line above `ask_accounts`.
    #[test]
    fn the_consent_question_names_where_the_names_are_sent() {
        let asked = consent_question("friend", Some("https://n8n.local/webhook/snob"));
        assert!(asked.contains("@friend"), "{asked}");
        assert!(asked.contains("https://n8n.local/webhook/snob"), "{asked}");

        // With nowhere to send it, there is nothing extra to agree to and the
        // question stays the short one.
        let plain = consent_question("friend", None);
        assert!(plain.contains("@friend"), "{plain}");
        assert!(!plain.contains("sent to"), "{plain}");

        // And the file reads back as one fact rather than two unrelated lines.
        let both = describe_config(&config(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"https://n8n.local/hook\"\n\n\
             [[account]]\ntarget = \"self\"\n\n[[account]]\ntarget = \"friend\"\n\
             [account.consent]\nagreed_at = 1\n",
        ))
        .join("\n");
        assert!(
            both.contains("go to https://n8n.local/hook"),
            "watching somebody else and sending it somewhere is one fact: {both}"
        );

        // Your own account alone has nobody else's names in it, so there is
        // nothing to join.
        let alone = describe_config(&config(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"https://n8n.local/hook\"\n",
        ))
        .join("\n");
        assert!(!alone.contains("Their usernames"), "{alone}");
    }

    /// The sentence names the accounts a run would actually walk, over every
    /// shape the file can take.
    ///
    /// `watching_line`'s own doc says it is derived from the predicate
    /// `watched_from` runs "because the two used to disagree" -- and then it
    /// re-derived that predicate by hand, so nothing enforced the agreement. The
    /// rule that makes it hard is the one they disagreed about: the viewer is
    /// added only when the account list is empty, so a file naming one stranger
    /// is walked as that stranger alone.
    ///
    /// The expected sentences are written out rather than computed from
    /// `watched_from`, which would make this agree with itself whatever either
    /// side did. Written out, a change to `watched_from` moves the sentence and
    /// fails here -- which is the whole property, and is exactly what the
    /// hand-written copy prevented.
    #[test]
    fn the_line_names_the_accounts_a_run_would_actually_walk() {
        let friend = "[account.consent]\nagreed_at = 1\n";
        for (body, expected) in [
            (
                "schema = 1\nevery = \"6h\"\n".to_string(),
                "Watches your account",
            ),
            (
                "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"self\"\n".to_string(),
                "Watches your account",
            ),
            (
                format!("schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"friend\"\n{friend}"),
                "Watches 1 account, and not your own",
            ),
            (
                format!(
                    "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"self\"\n\n\
                     [[account]]\ntarget = \"friend\"\n{friend}"
                ),
                "Watches your account and 1 other",
            ),
            (
                format!(
                    "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"@friend\"\n{friend}\n\
                     [[account]]\ntarget = \"other\"\n{friend}"
                ),
                "Watches 2 accounts, and not your own",
            ),
        ] {
            let file = config(&body);
            assert_eq!(watching_line(&file), expected, "for {body:?}");
        }
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

    /// Each webhook secret is stored under the `Kind` that says what it is for.
    ///
    /// The two were elements four and five of a tuple, both `Option<Secret>`,
    /// told apart by position across three sites -- and swapping them compiles.
    /// One of them is sent verbatim to the user's server as the `Authorization`
    /// value on every request; the other is the shared secret every signature is
    /// computed from and must never leave this machine. Swapped, the signing key
    /// is handed to the receiver as a bearer token, and every signature is
    /// computed with a value that receiver already had -- so the signature
    /// proves nothing to the only party who checks it. `plan` reads whatever is
    /// under `Kind::WatchToken` and puts it in a header; `sign` reads whatever is
    /// under `Kind::WatchSigningKey` and MACs the body with it. Neither can tell
    /// what it was given, and nothing downstream ever notices.
    ///
    /// The two secrets carry different values on purpose: a test using one
    /// string cannot fail on a swap, which is the whole failure.
    ///
    /// `store_secrets` is reachable and `setup` is not -- `ui::can_show_a_menu()`
    /// is false under `cargo test` -- so the pairing lives on the type rather
    /// than in the middle of the wizard.
    #[test]
    fn each_webhook_secret_is_stored_under_the_kind_that_says_what_it_is_for() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        // Its own service name, always. The keyring belongs to the operating
        // system and not to this process.
        let secrets = SecretStore::new(paths, true).with_service(&format!(
            "snob-ig-test-webhook-answers-{}",
            std::process::id()
        ));

        let answers = WebhookAnswers {
            url: Some("https://n8n.local/webhook/snob".to_string()),
            heartbeat: false,
            headers: vec![],
            signing_key: Some(Secret::new("signing-key-never-leaves-this-machine")),
            token: Some(Secret::new("Bearer token-the-receiver-already-has")),
        };

        if answers.store_secrets(&secrets).is_err() {
            return; // no keyring on this machine; `save_secret` says why
        }

        let stored = |kind| {
            secrets
                .load_secret(kind)
                .unwrap()
                .found()
                .map(|s| s.expose().to_string())
        };

        assert_eq!(
            stored(Kind::WatchSigningKey).as_deref(),
            Some("signing-key-never-leaves-this-machine"),
            "the signing key became the Authorization value, and is now sent to the \
             receiver on every request"
        );
        assert_eq!(
            stored(Kind::WatchToken).as_deref(),
            Some("Bearer token-the-receiver-already-has"),
            "the token became the signing key, so every signature is computed with a \
             value the receiver already holds"
        );

        secrets.forget_secret(Kind::WatchSigningKey).unwrap();
        secrets.forget_secret(Kind::WatchToken).unwrap();
    }

    /// A name typed twice must not produce a file the wizard cannot read back.
    ///
    /// `[webhook.headers]` is a TOML table and TOML refuses a duplicate key, so
    /// correcting a mistyped `X-Api-Key` by typing it again wrote the name
    /// twice. `config::parse` is deliberately the last thing before the file is
    /// written and before either secret is stored, so it refused, and the wizard
    /// threw away every answer -- including two secrets typed at a masked
    /// prompt -- and told the user it was a bug in the tool.
    #[test]
    fn a_header_named_twice_does_not_produce_a_file_the_tool_cannot_read() {
        let headers = last_of_each(vec![
            ("X-Api-Key".to_string(), "a".to_string()),
            ("X-Api-Key".to_string(), "b".to_string()),
        ]);

        let text = config::template(
            &config(&format!(
                "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"https://n8n.local/hook\"\n\n\
                 [webhook.headers]\n{}\n",
                headers
                    .iter()
                    .map(|(name, value)| format!("\"{name}\" = \"{value}\""))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
            false,
        );
        let parsed = config::parse(&text, std::path::Path::new("watch.toml"))
            .expect("the wizard must not write a file it cannot read back");

        assert_eq!(
            parsed.webhook.unwrap().headers["X-Api-Key"],
            "b",
            "the value typed last is the one that was meant"
        );
    }

    /// A name written with an at sign still names the account it watches.
    ///
    /// `status` settles which accounts the file names by looking each one up by
    /// username, and it read the file's spelling straight. With
    /// `target = "@friend"` the lookup found nobody, so every run of that
    /// account was scored as belonging to an account the configuration no longer
    /// names -- which is deliberately only a note, never a verdict. A monitor
    /// whose one watched account fails every run then exits 0 forever, which is
    /// the quiet direction and the one nobody notices.
    #[test]
    fn a_name_written_with_an_at_sign_is_still_an_account_the_file_watches() {
        let db = Store::in_memory().unwrap();
        snob_core::store::users::upsert(
            db.conn(),
            &snob_core::model::User {
                pk: 7,
                username: "friend".into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            },
        )
        .unwrap();
        snob_core::store::accounts::upsert(db.conn(), 7, false).unwrap();

        let edited = config("schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"@friend\"\n");
        assert_eq!(
            watched_pks(&db, Some(&edited)).unwrap(),
            Some(vec![7]),
            "the file names @friend and the store knows friend; they are one account"
        );
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
    fn ran(outcome: ExitCode) -> watch_store::Run {
        watch_store::Run {
            account_pk: 42,
            started_at: NOW - 60,
            finished_at: Some(NOW - 60),
            requests: 1,
            // `as_str`, which is what `commit` writes. It took a `&str` and
            // every call site spelled a token -- the same literals `health` was
            // matching on, so the fixture and the defect moved together.
            outcome: Some(outcome.as_str().to_string()),
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

        let ok = ran(ExitCode::Ok);
        assert_eq!(
            health(
                Some(&configured),
                &[of(&ok)],
                &[],
                deliveries::Owed::default(),
                NOW
            )
            .verdict,
            Verdict::Ok,
            "configured, ran just now, nothing owed"
        );

        // A cooldown lifts on its own, and an interrupt was the user. Neither
        // is a monitor that needs attention.
        for lifts in [ExitCode::RateLimited, ExitCode::Interrupted] {
            let run = ran(lifts);
            assert_eq!(
                health(
                    Some(&configured),
                    &[of(&run)],
                    &[],
                    deliveries::Owed::default(),
                    NOW
                )
                .verdict,
                Verdict::Warned,
                "{lifts:?} passes on its own"
            );
        }

        // A session that has gone will not come back without somebody logging
        // in, and every run until then does nothing at all.
        let dead = ran(ExitCode::NoSession);
        assert_eq!(
            health(
                Some(&configured),
                &[of(&dead)],
                &[],
                deliveries::Owed::default(),
                NOW
            )
            .verdict,
            Verdict::Failed
        );

        // Owed reports this configuration could post are a wait.
        assert_eq!(
            health(
                Some(&configured),
                &[of(&ok)],
                &[],
                deliveries::Owed {
                    waiting: 2,
                    elsewhere: 0,
                    given_up: 0,
                },
                NOW
            )
            .verdict,
            Verdict::Warned
        );
        // Addressed to somewhere this file does not name is a different
        // question, and this one is the answer that used to be wrong: a file
        // with no address cannot tell a run given --webhook from a [webhook]
        // somebody deleted, and guessing failed the supported shape on every
        // poll. `a_report_addressed_elsewhere_is_not_owed_to_this_run` holds
        // both directions down.
        let no_webhook = config(
            "schema = 1
every = \"6h\"
",
        );
        assert_eq!(
            health(
                Some(&no_webhook),
                &[of(&ok)],
                &[],
                deliveries::Owed {
                    waiting: 0,
                    elsewhere: 2,
                    given_up: 0,
                },
                NOW
            )
            .verdict,
            Verdict::Warned,
            "a file with no address cannot tell --webhook from a deleted section"
        );

        // And a machine with nothing configured is not broken, but a bare
        // `snob watch` there has no schedule to run on.
        let nothing = health(None, &[], &[], deliveries::Owed::default(), NOW);
        assert_eq!(nothing.verdict, Verdict::Warned);
        assert_eq!(nothing.notes.len(), 2, "{:?}", nothing.notes);
    }

    /// What stopped the last run is read in the vocabulary exit codes are
    /// written in, not in three literals spelled again inside `health`.
    ///
    /// `as_str`'s doc says the vocabulary exists so that nothing invents tokens
    /// inline, "which is how two spellings of one condition get shipped", and
    /// this function was where they were invented. The cost of respelling one
    /// was a monitor in an ordinary cooldown scored `Failed`, so `status` exits
    /// 1 about something that resumes by itself -- and the fixture above spelled
    /// the same literals, so the suite would have moved with it.
    ///
    /// Walked over `ExitCode::ALL`, so a code added later has to be placed
    /// deliberately rather than defaulted into `Failed` by nobody mentioning it.
    #[test]
    fn what_stopped_the_last_run_is_read_in_the_tokens_exit_codes_are_written_in() {
        let configured = config("schema = 1\nevery = \"6h\"\n");
        let verdict_of = |run: &watch_store::Run| {
            health(
                Some(&configured),
                &[of(run)],
                &[],
                deliveries::Owed::default(),
                NOW,
            )
            .verdict
        };

        for code in ExitCode::ALL {
            let run = ran(code);
            let expected = match code {
                ExitCode::Ok => Verdict::Ok,
                ExitCode::RateLimited | ExitCode::Interrupted => Verdict::Warned,
                _ => Verdict::Failed,
            };
            assert_eq!(
                verdict_of(&run),
                expected,
                "a run recorded as {code:?} ({})",
                code.as_str()
            );
        }

        // Named on its own as well as inside the walk, because this is the one
        // the mapping can lose without the walk noticing: drop a code from `ALL`
        // and the loop simply stops testing it.
        let cooled = ran(ExitCode::RateLimited);
        assert_eq!(
            verdict_of(&cooled),
            Verdict::Warned,
            "a cooldown lifts by itself; a probe that pages for one is a probe people switch off"
        );

        // And a row spelled by something that is not this build is a failure it
        // cannot explain, rather than a quiet `Ok`.
        let unknown = watch_store::Run {
            outcome: Some("rate-limited".to_string()),
            ..ran(ExitCode::Ok)
        };
        assert_eq!(
            verdict_of(&unknown),
            Verdict::Failed,
            "a token this build does not know is not a healthy run"
        );
    }

    /// The two probes say the same thing about a machine with no `watch.toml`.
    ///
    /// `snob watch check` decides that sentence in `engine::check` and
    /// `snob watch status` decides it here. They used to spell it out
    /// separately, character for character -- and it is the advice a newly
    /// installed tool gives, so it is the one somebody edits. Two probes run one
    /// after the other, disagreeing about the same machine, each with a test
    /// asserting it is right, is the state this stops.
    ///
    /// One run is given so `status` has nothing else to say: the only note left
    /// is the one under test, which is what makes this an equality rather than a
    /// search for a substring.
    #[test]
    fn both_probes_say_the_same_thing_about_an_unconfigured_machine() {
        let ok = ran(ExitCode::Ok);
        let status = health(None, &[of(&ok)], &[], deliveries::Owed::default(), NOW);
        assert_eq!(status.notes.len(), 1, "{:?}", status.notes);

        let check = crate::engine::check::without_a_session(None, None, NOW);
        assert_eq!(check.checked.len(), 1, "{:?}", check.checked);

        assert_eq!(
            status.notes[0],
            check.checked[0]
                .problem
                .clone()
                .expect("the check has to say why nothing is configured"),
            "two probes somebody runs one after the other, disagreeing about one machine"
        );
    }

    /// A queue for an address the file does not name is not a monitor with
    /// nowhere to send, and one for an address it *has* moved away from is.
    ///
    /// The old branch asked `config.webhook.is_none()` over a count of every
    /// pending row, and got both directions wrong. A run given the address on
    /// the command line from a unit with no `watch.toml` -- the README's own
    /// systemd example -- was `Failed` and exit 1 on every poll with one
    /// delivery failure behind it, while its rows carried a real address the
    /// next run drains. Meanwhile the shape that really does lose changes, a
    /// `[webhook] url` moved to a new host with the old queue still standing,
    /// counted as an ordinary wait and printed "the next run tries them".
    #[test]
    fn a_report_addressed_elsewhere_is_not_owed_to_this_run() {
        let ok = ran(ExitCode::Ok);
        let two_elsewhere = deliveries::Owed {
            waiting: 0,
            elsewhere: 2,
            given_up: 0,
        };

        // The address moved. Those rows can never go, and the changes in them
        // are already marked as reported, so they are the only copy.
        let moved = config(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"https://n8n.new.local/hook\"\n",
        );
        let stranded = health(Some(&moved), &[of(&ok)], &[], two_elsewhere, NOW);
        assert_eq!(stranded.verdict, Verdict::Failed, "{:?}", stranded.notes);

        // The same rows with no `[webhook]` in the file are the shape the README
        // leads with: the address is on the command line and the next run drains
        // them. A guess in the alarming direction is what costs a probe its
        // credibility.
        let from_the_flag = config("schema = 1\nevery = \"6h\"\n");
        let fine = health(Some(&from_the_flag), &[of(&ok)], &[], two_elsewhere, NOW);
        assert_eq!(fine.verdict, Verdict::Warned, "{:?}", fine.notes);
        assert_eq!(fine.verdict.exit_code(), ExitCode::Ok);
        assert!(
            fine.notes.iter().any(|n| n.contains("--webhook")),
            "and it has to say what would send them: {:?}",
            fine.notes
        );

        // And the file that says where reports go says so out loud, because
        // "Sends nothing" was printed about a monitor that delivers on every
        // run.
        assert!(
            !describe_config(&from_the_flag)
                .join("\n")
                .contains("Sends nothing"),
            "the file names no address; that is not the same as sending nothing"
        );
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
            // `every = "2w"` beside `on = ["mon"]` was in this list, and it was
            // the defect rather than the guard: days with no time of day went to
            // `Schedule::calendar`, which is a set of minutes and refuses one
            // naming none, so the shape the README leads with killed every run
            // of a service over a file the parser had accepted. It builds now,
            // and `one_monday_in_every_two_is_what_the_readme_says_it_is` is
            // where it is held down.
        ] {
            let configured = config(refused);
            let ok = ran(ExitCode::Ok);
            let health = health(
                Some(&configured),
                &[of(&ok)],
                &[],
                deliveries::Owed::default(),
                NOW,
            );
            assert_eq!(
                health.verdict,
                Verdict::Failed,
                "{refused:?} kills every run: {:?}",
                health.notes
            );
        }

        // And one that builds is still fine.
        let good = config("schema = 1\nevery = \"6h\"\n");
        let ok = ran(ExitCode::Ok);
        assert_eq!(
            health(
                Some(&good),
                &[of(&ok)],
                &[],
                deliveries::Owed::default(),
                NOW
            )
            .verdict,
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
        let ok = ran(ExitCode::Ok);

        let half = HalfRead {
            who: "@me".to_string(),
            reported: ListKind::Followers,
            missing: Some(ListKind::Following),
        };
        let blind = health(
            Some(&configured),
            &[of(&ok)],
            &[half],
            deliveries::Owed::default(),
            NOW,
        );
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
            health(
                Some(&configured),
                &[of(&ok)],
                &[whole],
                deliveries::Owed::default(),
                NOW
            )
            .verdict,
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

        let recent = ran(ExitCode::Ok);
        assert_eq!(
            health(
                Some(&configured),
                &[of(&recent)],
                &[],
                deliveries::Owed::default(),
                NOW
            )
            .verdict,
            Verdict::Ok
        );

        // One six-hour gap missed is a laptop that was shut.
        let late = watch_store::Run {
            started_at: NOW - 7 * 3_600,
            ..ran(ExitCode::Ok)
        };
        let one = health(
            Some(&configured),
            &[of(&late)],
            &[],
            deliveries::Owed::default(),
            NOW,
        );
        assert_eq!(one.verdict, Verdict::Warned, "{:?}", one.notes);

        // Three weeks is nobody coming back.
        let gone = watch_store::Run {
            started_at: NOW - 21 * 86_400,
            ..ran(ExitCode::Ok)
        };
        let stopped = health(
            Some(&configured),
            &[of(&gone)],
            &[],
            deliveries::Owed::default(),
            NOW,
        );
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
            ..ran(ExitCode::Ok)
        };

        let weekly = config("schema = 1\non = [\"mon\"]\nat = [\"09:00\"]\n");
        assert_eq!(
            health(
                Some(&weekly),
                &[of(&two_days_ago)],
                &[],
                deliveries::Owed::default(),
                NOW
            )
            .verdict,
            Verdict::Ok,
            "two days is not late for a weekly schedule"
        );

        let six_hourly = config("schema = 1\nevery = \"6h\"\n");
        assert_ne!(
            health(
                Some(&six_hourly),
                &[of(&two_days_ago)],
                &[],
                deliveries::Owed::default(),
                NOW
            )
            .verdict,
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
        let failed = ran(ExitCode::NoSession);

        let orphan = RunOf {
            run: &failed,
            who: "@stranger".to_string(),
            watched: false,
        };
        let health = health(
            Some(&configured),
            &[orphan],
            &[],
            deliveries::Owed::default(),
            NOW,
        );

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
        assert!(interval_of("5m").is_err(), "five minutes is too often");
        assert!(
            cron_of("*/5 * * * *").is_err(),
            "the same five minutes, written the other way"
        );
        assert!(
            calendar_of("", "09:00,09:05").is_err(),
            "and again, as two moments five minutes apart"
        );

        // And the fields a good answer produces, since the file is written from
        // them.
        assert_eq!(
            interval_of("6h").unwrap(),
            std::time::Duration::from_secs(21_600)
        );
        assert_eq!(cron_of(" 0 9 * * 1,4 ").unwrap(), "0 9 * * 1,4");
        assert_eq!(
            calendar_of("mon, thu", "09:00, 21:00").unwrap(),
            (
                vec!["mon".to_string(), "thu".to_string()],
                vec!["09:00".to_string(), "21:00".to_string()]
            )
        );
        assert!(
            calendar_of("", "09:00").unwrap().0.is_empty(),
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
            let wizard = calendar_of(days, times);
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

    /// The wizard offers only the jitter the schedule can absorb.
    ///
    /// `Schedule::with_jitter` clamps to what the grid can take and says
    /// nothing, so a value larger than the room becomes a smaller one at every
    /// start while `status` reads back what the file says. The room is zero for
    /// two shapes this tool advertises -- `*/15 * * * *`, whose grid is exactly
    /// the floor, and `--every 2w --on mon`, a weekly grid under a fortnightly
    /// floor -- and on those the wizard has nothing to ask and says so.
    ///
    /// The ceiling is asked of the schedule rather than worked out here:
    /// `room_for_jitter` is private, and a second copy of that arithmetic is how
    /// the wizard and a run come to disagree.
    ///
    /// What this cannot reach, said plainly: `ask_jitter`'s prompt and its
    /// refusal are behind `ui::prompt_line`, which answers nothing without a
    /// terminal, so what is pinned is the ceiling they ask against and the
    /// zero-room path that returns before asking. A schedule cannot have a
    /// default jitter larger than its room -- `fitted` clamps it at
    /// construction -- so the two are only ever told apart where there is room
    /// to spare, which is the first assertion here.
    #[test]
    fn the_wizard_offers_only_the_jitter_the_schedule_can_absorb() {
        // Six-hourly: the interval is the bound, because an interval measured
        // from the previous run slides the whole schedule.
        let interval = schedule::Schedule::every(std::time::Duration::from_secs(21_600)).unwrap();
        assert_eq!(
            room_for(&interval),
            std::time::Duration::from_secs(21_600),
            "an interval has no grid to miss, so the interval is the bound"
        );

        // The two with nothing to give. `ask_jitter` answers `None` on both
        // without asking anything, so no `jitter` key is written.
        for none_at_all in [
            schedule::Schedule::cron("*/15 * * * *").unwrap(),
            schedule::Schedule::days(
                &[snob_core::watch::schedule::Weekday::Mon],
                std::time::Duration::from_secs(14 * 24 * 3_600),
            )
            .unwrap(),
        ] {
            assert!(
                room_for(&none_at_all).is_zero(),
                "{none_at_all:?} has no room, and the question must not be asked"
            );
            assert_eq!(
                ask_jitter(&none_at_all).unwrap(),
                None,
                "nothing to ask, so nothing is written"
            );
        }

        // And a schedule with room does not accept more than it has: a run at
        // its moment pushed past the next one is the failure the ceiling exists
        // for, and it is silent when the clamp does it.
        let daily = schedule::Schedule::cron("0 9 * * *").unwrap();
        let room = room_for(&daily);
        assert!(!room.is_zero(), "a daily schedule has a day to play with");
        assert_eq!(
            daily
                .clone()
                .with_jitter(room + std::time::Duration::from_secs(60))
                .jitter(),
            room,
            "the clamp is what the wizard has to refuse in front of, not repeat"
        );
    }
}
