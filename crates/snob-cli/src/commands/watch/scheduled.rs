//! The scheduled mode: `snob watch` with no subcommand.
//!
//! The loop, and what it starts its clock from. One turn of it is
//! `super::run::open_and_run`, which is the same run `once` makes; what is left
//! here is the waiting, the seed the waiting measures from, and the two
//! sentences a loop no test can drive still has to get right.

use anyhow::Result;
use snob_core::model::printable;
use snob_core::watch::schedule::{self, Due, Schedule};
use snob_store::config;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::WatchRunArgs;
use crate::exit::{ExitCode, ExitError};
use crate::report;
use crate::ui;

use super::delivery::delivery_from;
use super::run::{Printing, open_and_run};
use super::say::say_what_was_given_up;
use super::schedule::{describe_schedule, schedule_from, when_from};
use super::watched::{watched_from, watching_label};

/// Stays up and runs on a schedule until it is stopped.
///
/// The loop is deliberately thin. Everything with a rule in it — when the next
/// run is due, how many were missed, how far jitter may push one — is
/// `snob_core::watch::schedule`, which reads no clock and is tested with
/// literal timestamps. What is left here is sleeping and asking again, which
/// is the part no test can usefully drive.
pub(super) async fn scheduled(
    args: WatchRunArgs,
    secrets: SecretStore,
    paths: &AppPaths,
) -> Result<ExitCode> {
    // Before anything that can refuse to start. Reading the file, building the
    // schedule and checking the address can each end this process, and `settle`
    // is the only caller of `store::prune` — so a service that dies at startup
    // on a hand-edited file expires nothing for as long as nobody notices. This
    // is the mode the README leads with, and it had no settle at all.
    say_what_was_given_up(crate::engine::watch::settle_without_a_session(
        paths,
        snob_core::clock::now(),
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
    let wording = Printing::unattended(args.json).wording();

    let mut last_run = seed_for(paths, &schedule, snob_core::clock::now())?;

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
        last_run = Some(snob_core::clock::now());
        if let Err(e) = open_and_run(&args, &watched, delivery.as_ref(), &secrets, paths).await {
            report::print_error(&e, wording);
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
        let now = snob_core::clock::now();

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
                report::print_error(&e, wording);
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
    let store = snob_store::store::Store::open(paths)?;
    Ok(snob_store::store::watch::last_started(store.conn())?)
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

    let store = snob_store::store::Store::open(paths)?;
    if let Some(seeded) = snob_store::store::watch::interval_seeded_at(store.conn())? {
        return Ok(Some(seeded));
    }
    snob_store::store::watch::set_interval_seeded_at(store.conn(), invented)?;
    Ok(Some(invented))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
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
        let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
        let at_nine = Schedule::calendar(&[], &[schedule::parse_time("09:00").unwrap()]).unwrap();

        assert_eq!(seed_for(&paths, &at_nine, 1_700_000_000).unwrap(), None);
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
}
