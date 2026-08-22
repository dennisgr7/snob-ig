//! The schedule, from the flags or from the file.
//!
//! The translation layer and nothing more: what either source wrote becomes a
//! `snob_core::watch::schedule::Schedule` here, and every rule about when that
//! schedule comes round is the domain's.

use anyhow::Result;
use snob_core::watch::schedule::{self, Schedule, Weekday};
use snob_store::config::WatchConfig;

use crate::cli::WatchRunArgs;
use crate::report;

/// The schedule as written, before it is built: whichever of the two sources
/// won, in the words it was given in.
///
/// The flags win as a set rather than field by field, because a schedule half
/// from the file and half from the command line is one nobody can read.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct When {
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
pub(super) fn when_from(args: &WatchRunArgs, configured: Option<&WatchConfig>) -> When {
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
pub(super) fn calendar_from<S: AsRef<str>>(days: &[S], times: &[S]) -> Result<Schedule> {
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
pub(super) fn describe_schedule(when: &When, schedule: &Schedule, now: bool) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::fixtures::watch_toml;

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
    /// dropped fails and a field that is copied into its neighbor fails too --
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
}
