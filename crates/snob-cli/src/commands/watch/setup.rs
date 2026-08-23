//! `snob watch setup`: the questions, and the file they are written to.
//!
//! The half of the monitor that is about not having to remember flags. It asks,
//! it offers to walk a baseline, and it writes `watch.toml` and two keyring
//! entries. What is configured is then read back by `super::status`, which is
//! the only way to tell a monitor that found nothing from one that quietly
//! stopped — and the two share `describe_config`, so `--dry-run` here and
//! `status` there cannot describe the same file differently.

use anyhow::{Context, Result, bail};
use snob_core::Epoch;
use snob_core::model::printable;
use snob_core::secret::Secret;
use snob_core::{duration, watch::schedule};
use snob_store::config::{self, WatchConfig};
use snob_store::paths::AppPaths;
use snob_store::secrets::{Kind, SecretStore};

use crate::cli::WatchSetupArgs;
use crate::engine::check::Verdict;
use crate::exit::{ExitCode, ExitError};
use crate::ui;

use super::preflight::{baseline_now, describe_check, preflight};
use super::schedule::{calendar_from, schedule_from};
use super::status::{describe_config, plural};

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
        crate::ui::say!("{text}");
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
    crate::ui::say!();
    let report = preflight(&Default::default(), &secrets, paths).await?;
    for line in describe_check(&report) {
        crate::ui::say!("{line}");
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

    crate::ui::say!();
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
    baseline_now(configured.as_ref(), secrets, paths).await
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
    let schedule = schedule_from(&Default::default(), Some(&config))?;
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
// Nothing could. The validation lives on this side of the menu, and the one
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
    calendar_from(&days, &times)?;

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
        let value = ui::prompt_secret("A shared secret (at least 32 characters, random)")?;
        // The same validator `--sign-with` runs, literally: one floor, one
        // trim, one sentence, so a key the flag accepts is a key the wizard
        // accepts and the other way round. The wizard used to keep its own
        // count, and the two had drifted over whether whitespace counts.
        let value = crate::cli::signing_secret(&value).map_err(|why| anyhow::anyhow!(why))?;
        Some(Secret::new(value))
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

fn ask_accounts(webhook: Option<&str>) -> Result<Vec<(String, Option<Epoch>)>> {
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
            accounts.push((name.to_string(), Some(snob_core::clock::now())));
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
/// person, and `engine::ask_consent_with` now says whose list it is and where
/// it lands. The wizard makes it sharpest, because `ask_webhook` runs one line
/// above `ask_accounts` and the address is already in hand while the question is
/// being asked — and `describe_config` then printed "Reports to https://…" and
/// "Watches your account and 1 other" as two unrelated lines.
///
/// Split from the prompting so it can be read. Everything around it is behind
/// `ui::menu`, which answers nothing without a terminal.
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

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::Pk;

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
                pk: Some(Pk::new(1)),
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

    /// Consent recorded about one act is read as authority for another, so the
    /// question has to name both.
    ///
    /// `[account.consent] agreed_at` is an answer to "may this read @friend's
    /// lists?", and `Watched::may_run_unattended` then reads it as the authority
    /// for POSTing @friend's arrivals and departures -- username, full name and
    /// profile URL -- to a third-party server on every run. Both prompts framed
    /// the question as risk to the user's own account and neither mentioned the
    /// other person; the wizard already holds the address when it asks, because
    /// `ask_webhook` runs one line above `ask_accounts`, so it is the one that
    /// can name the destination as well as the person.
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

    /// Setup refuses a schedule the scheduler would refuse anyway, but it does
    /// it while the person who typed it is still reading.
    ///
    /// This used to be one line asserting `Schedule::every(300).is_err()`, which
    /// is a fact about the schedule module and says nothing about whether
    /// `setup` asks it. The validation was unreachable behind the menu, so
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
            let run = calendar_from(
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
