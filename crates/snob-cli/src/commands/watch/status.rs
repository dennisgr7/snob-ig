//! `snob watch status`: what is configured, and whether it is working.
//!
//! The other half of not having to remember flags. `super::setup` writes the
//! file; this reads it back alongside what the runs actually did, and answers
//! the one question a scheduled thing cannot answer for itself — a monitor that
//! found nothing and a monitor that quietly stopped look identical from outside.
//!
//! `health` is where that answer is decided, in `status`'s output **and** in its
//! exit code, so a machine and a person are told the same thing.

use anyhow::Result;
use snob_core::model::{ListKind, printable};
use snob_core::watch::schedule;
use snob_store::config::{self, WatchConfig};
use snob_store::paths::AppPaths;
use snob_store::store::{Store, deliveries, watch as watch_store};

use crate::cli::WatchStatusArgs;
use crate::engine::check::Verdict;
use crate::exit::ExitCode;
use crate::report;

use super::schedule::schedule_from;
use super::watched::watched_from;

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
        .map(|url| super::delivery::destination_of(&url));
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
        let name = snob_store::store::users::name(db.conn(), run.account_pk)?;
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
        let name = snob_store::store::users::name(db.conn(), pk)?;
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
        snob_core::clock::now(),
    );

    if args.json {
        let document = super::wire::status_json(
            config.is_some(),
            &config::path(paths),
            &owed,
            &health,
            &last_runs,
            &marks,
        );
        crate::ui::say!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(health.verdict.exit_code());
    }

    match &config {
        Some(config) => {
            crate::ui::say!("Configured in {}", config::path(paths).display());
            for line in describe_config(config) {
                crate::ui::say!("  {line}");
            }
        }
        None => crate::ui::say!(
            "Nothing is configured. Run \"snob watch setup\", or pass the schedule on the \
             command line."
        ),
    }

    crate::ui::say!();
    // Said before the marks, because it answers the question somebody opening
    // `status` actually has. A run that could not look moves no mark, so a
    // monitor that has been in a cooldown since Monday looks, from the marks
    // alone, exactly like one that was killed on Monday.
    if last_runs.is_empty() {
        crate::ui::say!("It has not run yet.");
    } else {
        for run in &last_runs {
            let pk = run.account_pk;
            let name = snob_store::store::users::name(db.conn(), pk)?;
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
            crate::ui::say!("{line}.");
        }
    }

    crate::ui::say!();
    if marks.is_empty() {
        crate::ui::say!("The monitor has not reported on anything yet.");
    } else {
        for mark in &marks {
            let name = snob_store::store::users::name(db.conn(), mark.account_pk)?;
            let who = crate::app::label(mark.account_pk, name.as_deref());
            crate::ui::say!(
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
        crate::ui::say!();
    }
    if owed.given_up > 0 {
        let (subject, what) = if owed.given_up == 1 {
            ("report was", "What it said is")
        } else {
            ("reports were", "What they said is")
        };
        crate::ui::say!(
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
        crate::ui::say!(
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
        crate::ui::say!(
            "{} {subject} addressed to a webhook this configuration does not send to, so \
             nothing here will try {it}. {they} {expire} on {their} own.",
            owed.elsewhere
        );
    }

    if !health.notes.is_empty() {
        crate::ui::say!();
        crate::ui::say!("Health: {}", health.verdict.as_str());
        for note in &health.notes {
            crate::ui::say!("  {note}");
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
pub(super) struct Health {
    pub(super) verdict: Verdict,
    pub(super) notes: Vec<String>,
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

    let watched = watched_from(None, Some(config));
    let mut pks = Vec::new();
    if watched.iter().any(|w| w.name().is_none()) {
        match snob_store::store::accounts::own(conn)? {
            Some(pk) => pks.push(pk),
            // The file watches this machine's own account and the machine has
            // never recorded which one that is. Nothing here can be settled.
            None => return Ok(None),
        }
    }
    for named in watched.iter().filter_map(|w| w.name()) {
        if let Some(pk) = snob_store::store::accounts::find_pk_by_username(conn, named)? {
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

/// How long this configuration means the monitor may be silent for.
///
/// Asked of the schedule rather than guessed at: six hours for `--every 6h`,
/// and a week for `--on mon --at 09:00`, which is the point — a weekly monitor
/// that has not run since Tuesday is not late.
///
/// **The widest gap in a cycle, not the distance between the next two
/// moments.** Those are the same number only on a uniform grid, and the wizard
/// prompts for something that is not one, back to back: "Which days?
/// (mon,thu)" and "At what times? (09:00,21:00)". Answer both with the example
/// and the real gaps are 12h, 60h, 12h, 84h. On a Saturday the next two
/// moments are Monday 09:00 and Monday 21:00, so the gap read as 12h, a last
/// run on Thursday evening was 36h ago, that is three missed runs, and `status`
/// went red — for 48 hours every week, on a monitor doing exactly what it was
/// told. `--at 09:00,10:00` was worse: red from one in the afternoon until nine
/// the next morning, daily. The nearby note about a probe people learn to
/// ignore is about precisely this.
///
/// Bounded two ways so an `--every 5m` file does not walk a week of moments:
/// a horizon of eight days, which covers any weekly pattern, and a hard cap on
/// iterations. A uniform schedule reaches its widest gap on the first step, so
/// the cap costs it nothing.
///
/// `None` when the file has no schedule this can build, or names one that never
/// fires. Both of those are their own line elsewhere and neither is a reason to
/// call the monitor late as well.
fn expected_gap(config: &WatchConfig, now: i64) -> Option<i64> {
    /// Far enough to see a whole week's pattern, and one day over so a weekly
    /// schedule is measured rather than truncated.
    const HORIZON_SECS: i64 = 8 * 24 * 3600;
    /// A backstop for a schedule that fires often enough to make the horizon
    /// expensive. Two hundred steps of `--every 5m` is under a day, and a
    /// uniform grid has already given its answer by step one.
    const MOST_STEPS: usize = 200;

    let schedule = schedule_from(&Default::default(), Some(config)).ok()?;
    let first = schedule::next_moment(&schedule, Some(now), now, &chrono::Local)?;

    let mut at = first;
    let mut widest = 0;
    for _ in 0..MOST_STEPS {
        let Some(next) = schedule::next_moment(&schedule, Some(at), at, &chrono::Local) else {
            break;
        };
        widest = widest.max(next - at);
        at = next;
        if at - first >= HORIZON_SECS {
            break;
        }
    }
    (widest > 0).then_some(widest)
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
        && let Err(e) = schedule_from(&Default::default(), Some(config))
    {
        at_least(Verdict::Failed);
        notes.push(format!("the configured schedule cannot be built: {e}"));
    }

    // And an address the delivery refuses is a monitor that cannot finish.
    //
    // The sibling of the schedule check above, and it was missing for the same
    // reason it was added there: `delivery_from` runs before anything is opened
    // or spent, so a bad address kills every run before `record_failed_run` can
    // file one. The table stays empty and the branch above calls that "it has
    // not run yet" -- a warning, exit 0, for ever. See
    // `webhook::problem_with_config` for what the same file looked like to
    // `snob watch check`, which reported it correctly all along.
    if let Some(webhook) = config.and_then(|c| c.webhook.as_ref())
        && let Some(problem) =
            crate::watch::webhook::problem_with_config(&webhook.url, &webhook.headers)
    {
        at_least(Verdict::Failed);
        notes.push(problem);
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
pub(super) fn describe_config(config: &WatchConfig) -> Vec<String> {
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

    // Redacted, not just filtered. `webhook::check` refuses an address
    // carrying a password and its comment says why: it "would be echoed by
    // `status`". This is `status`, and it echoed it -- twice, because the
    // sentence about other people's names below printed the raw string
    // after this one had been cleaned. One address, cleaned once, and both
    // sentences read it.
    let address = config.webhook.as_ref().map(|webhook| {
        let shown = url::Url::parse(&webhook.url).map_or_else(
            |_| webhook.url.clone(),
            |u| crate::watch::webhook::shown(&u),
        );
        printable(&shown)
    });

    match (&config.webhook, &address) {
        (Some(webhook), Some(address)) => {
            lines.push(format!("Reports to {address}"));
            if webhook.heartbeat {
                lines.push("Sends a report even when nothing changed".to_string());
            }
        }
        // What the **file** says, which is not the whole of where a report can
        // go. `--webhook` on the command line is a supported way to run this and
        // leaves nothing here to read back, so a flat "sends nothing" described
        // a monitor that delivers on every run as one that does not — the same
        // wrong reading the health verdict made of the same field.
        _ => lines.push(
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
    if let Some(address) = &address
        && config.accounts.iter().any(|a| !a.is_own())
    {
        lines.push(format!(
            "Their usernames and names go to {address} with every report"
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
    let watched = watched_from(None, Some(config));
    let own = watched.iter().any(|w| w.name().is_none());
    let others = watched.iter().filter(|w| w.name().is_some()).count();

    match (own, others) {
        (true, 0) => "Watches your account".to_string(),
        (true, n) => format!("Watches your account and {n} other{}", plural(n)),
        (false, n) => format!("Watches {n} account{}, and not your own", plural(n)),
    }
}

pub(super) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> WatchConfig {
        config::parse(text, std::path::Path::new("watch.toml")).unwrap()
    }

    /// The one address in the file is shown twice when somebody else is
    /// watched, and both showings are the cleaned one. The second was the
    /// raw string, so a hand-written `user:pass@` -- which `check` refuses
    /// and `status` does not run -- reached the terminal after the first
    /// line had taken care to hide it.
    #[test]
    fn a_password_in_the_address_is_hidden_from_every_line() {
        let lines = describe_config(&config(
            "schema = 1
every = \"6h\"

[webhook]
url = \"https://me:hunter2@n8n.local/hook\"

[[account]]
target = \"someone\"
",
        ));
        let text = lines.join(
            "
",
        );
        assert!(text.contains("go to"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("n8n.local/hook"), "{text}");
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
        snob_store::store::users::upsert(
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
        snob_store::store::accounts::upsert(db.conn(), 7, false).unwrap();

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

    /// **An address that kills every run before it starts is a failure, not a
    /// quiet month.**
    ///
    /// `delivery_from` runs before anything is opened or spent, so a bad
    /// address means `record_failed_run` never files a row and `watch_runs`
    /// stays empty. The empty table reads as "it has not run yet", which is a
    /// warning and exit 0, and it stays that way for ever. `snob watch check`
    /// reported the same file correctly all along; this is the probe that did
    /// not.
    #[test]
    fn an_address_that_can_never_work_is_a_failure_and_not_a_quiet_monitor() {
        // No scheme. Exactly what a hand-edited file looks like, and the file
        // invites hand-editing on its first line.
        let no_scheme = config(
            "schema = 1
every = \"6h\"

[webhook]
url = \"n8n.local/hook\"
",
        );
        let found = health(Some(&no_scheme), &[], &[], deliveries::Owed::default(), NOW);
        assert_eq!(
            found.verdict,
            Verdict::Failed,
            "an unusable address with an empty run log: {:?}",
            found.notes
        );
        assert_eq!(found.verdict.exit_code(), ExitCode::Error);

        // A credential in the address is the other refusal `webhook::check`
        // makes, and it reaches this probe by the same route.
        let in_the_url = config(
            "schema = 1
every = \"6h\"

[webhook]
url = \"https://user:pw@example.com/hook\"
",
        );
        assert_eq!(
            health(
                Some(&in_the_url),
                &[],
                &[],
                deliveries::Owed::default(),
                NOW
            )
            .verdict,
            Verdict::Failed
        );

        // And an address that is merely unreachable is not this probe's
        // question -- it cannot be answered without posting.
        let fine = config(
            "schema = 1
every = \"6h\"

[webhook]
url = \"https://example.com/hook\"
",
        );
        let ok = health(Some(&fine), &[], &[], deliveries::Owed::default(), NOW);
        assert_eq!(ok.verdict, Verdict::Warned, "{:?}", ok.notes);
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

    /// **A schedule that is not a uniform grid, which is what the wizard
    /// suggests.**
    ///
    /// The test above uses one weekly moment and `every = "6h"` — both
    /// uniform, so both have one gap and could not see this. Answer the two
    /// prompts with the examples they print and the gaps are 12h, 60h, 12h and
    /// 84h; reading the first one made `status` call a working monitor three
    /// runs late for two days out of every seven.
    #[test]
    fn an_uneven_schedule_is_measured_by_its_widest_gap_and_not_its_narrowest() {
        let uneven = config(
            "schema = 1
on = [\"mon\", \"thu\"]
at = [\"09:00\", \"21:00\"]
",
        );
        let gap = expected_gap(&uneven, NOW).expect("the schedule builds and fires");
        assert!(
            gap >= 80 * 3600,
            "the widest gap in this week is 84h; got {}h",
            gap / 3600
        );

        // Twice a day is still twice a day: the widest gap is the overnight
        // one, not the twelve hours between the two the wizard prints.
        let daily_pair = config(
            "schema = 1
at = [\"09:00\", \"10:00\"]
",
        );
        let gap = expected_gap(&daily_pair, NOW).expect("fires");
        assert!(
            gap >= 22 * 3600,
            "09:00 and 10:00 leaves 23 hours overnight; got {}h",
            gap / 3600
        );

        // And a uniform grid is unchanged, which is the half that already
        // worked and must keep working.
        let uniform = config(
            "schema = 1
every = \"6h\"
",
        );
        assert_eq!(expected_gap(&uniform, NOW), Some(6 * 3600));
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
}
