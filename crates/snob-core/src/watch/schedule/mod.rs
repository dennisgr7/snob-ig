//! When the monitor runs.
//!
//! Two ways to say it and one thing that decides it. `--every` is a floor
//! measured from the previous run; `--on`/`--at` and `--cron` both build a
//! [`Calendar`], which is a set of minutes the run is allowed to happen in.
//! Given both, the next run is the first moment that satisfies both — so
//! `--every 2w --on mon --at 09:00` reads the way it sounds, and
//! `--every 30m --on mon,thu` means every half hour but only on those days.
//!
//! **One evaluator, two syntaxes.** A cron expression is parsed into the same
//! `Calendar` that `--on mon --at 09:00` produces, so "Monday at nine" cannot
//! come to mean two different things depending on how somebody wrote it. That
//! is also why there is a parser here rather than a dependency: the hard part
//! of cron is not reading the five fields, it is that day-of-month and
//! day-of-week combine with OR when both are restricted and with AND when
//! either is `*`. Getting that wrong fires on the wrong days silently.
//!
//! Nothing here reads a clock. Every function takes `now` as an argument, which
//! is the shape `rate_budget::decide` established for the same reason: the
//! whole of this is testable with literal timestamps and without waiting.
//!
//! **One file per question.** This one holds the [`Schedule`] itself: how one
//! is built, what makes one legal, and the words for a run being owed.
//! `parse` reads the two syntaxes, `calendar` is the set of minutes they are
//! read into and everything asked of it, and `next` is the evaluator that puts
//! a moment to the calendar and answers when the next run is.

mod calendar;
mod next;
mod parse;

use std::time::Duration;

use chrono::TimeZone;

use calendar::{Calendar, Times};
use next::{next_after, next_local_midnight};
use parse::{FieldSet, parse_cron};

pub use next::{due, next_moment, wake_at, with_jitter};
pub use parse::{Weekday, parse_time};

/// The shortest gap between two runs.
///
/// Deliberately the same length as `SAME_MOMENT_GAP_SECS` in the CLI's cooldown
/// module, and deliberately not that constant. Fifteen minutes of an account's
/// life is the drift this tool already treats as a single instant; two runs
/// closer together than that cannot see a state the caching policy would even
/// go and walk, so the second is requests spent to be told what the first was
/// told. Two questions, so two numbers: changing what may be crossed must not
/// quietly change how often the monitor knocks.
pub const MIN_GAP_SECS: i64 = 15 * 60;

/// The longest interval `--every` accepts.
///
/// A year, because past that a calendar says it better and because there was no
/// upper bound at all: the arithmetic that adds an interval to the previous run
/// overflowed on an absurd one, which is a panic in a debug build and, in
/// release, a wrap to a negative floor that turns "every two hundred billion
/// years" into "every fifteen minutes". Absurd values are not only typos — this
/// one can arrive from a hand-edited `watch.toml`.
pub const MAX_INTERVAL_SECS: i64 = 366 * 24 * 3_600;

/// How far past its due moment a run may be pushed.
///
/// Not decoration. Every unattended copy of this tool waking on the same round
/// minute is a small synchronized spike on somebody else's service, and
/// spreading the runs out costs the user nothing. Fifteen minutes, or a tenth
/// of the interval when that is smaller, with a floor of one minute.
fn default_jitter(every: Option<Duration>) -> Duration {
    let ceiling = Duration::from_secs(15 * 60);
    match every {
        Some(every) => (every / 10).clamp(Duration::from_secs(60), ceiling),
        None => ceiling,
    }
}

/// When the monitor runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    /// Minutes the run may happen in. `None` allows any minute.
    calendar: Option<Calendar>,
    /// Never sooner than this after the previous run. `None` sets no floor.
    every: Option<Duration>,
    jitter: Duration,
}

impl Schedule {
    /// Whether the moments this fires at are named rather than measured.
    ///
    /// The difference matters in exactly one place: what a fresh install should
    /// pass as `last_run`. An interval with no past to measure from is due
    /// immediately, so a caller that does not want a walk the moment the monitor
    /// is set up has to invent one. A calendar has its own moments and needs no
    /// invention — and inventing one there is harmful, because it puts
    /// `MIN_GAP_SECS` between the install and the first run and steps over any
    /// moment inside the next quarter of an hour.
    pub fn is_on_a_calendar(&self) -> bool {
        self.calendar.is_some()
    }
}

/// What is wrong with a schedule, said so somebody can fix it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("a schedule needs an interval or a calendar: try --every 6h, or --on mon --at 09:00")]
    Empty,
    #[error(
        "{interval} is too often. The monitor does not run twice inside {minutes} minutes: a \
         second run that close cannot see anything the first did not, and it spends requests to \
         find that out."
    )]
    TooOften { interval: String, minutes: i64 },
    #[error(
        "{interval} is longer than this can schedule. The longest interval is {days} days; past \
         that, use a calendar."
    )]
    TooRare { interval: String, days: i64 },
    #[error("{0}")]
    Unreadable(String),
}

impl Schedule {
    /// Every so often, counted from the previous run.
    pub fn every(interval: Duration) -> Result<Self, ScheduleError> {
        Self {
            calendar: None,
            every: Some(interval),
            jitter: default_jitter(Some(interval)),
        }
        .validated()
    }

    /// On these weekdays, at these times of day. Empty days means every day.
    pub fn calendar(days: &[Weekday], times: &[(u32, u32)]) -> Result<Self, ScheduleError> {
        if times.is_empty() {
            return Err(ScheduleError::Unreadable(
                "a calendar needs a time of day: try --at 09:00".to_string(),
            ));
        }

        for &(hour, minute) in times {
            if hour > 23 || minute > 59 {
                return Err(ScheduleError::Unreadable(format!(
                    "{hour:02}:{minute:02} is not a time of day"
                )));
            }
        }

        let days_of_week = if days.is_empty() {
            FieldSet::all(0..=6)
        } else {
            FieldSet(days.iter().fold(0u64, |bits, d| bits | (1 << d.number())))
        };

        let mut exact: Vec<(u32, u32)> = times.to_vec();
        exact.sort_unstable();
        exact.dedup();

        Self {
            calendar: Some(Calendar {
                times: Times::Exact(exact),
                days_of_month: FieldSet::all(1..=31),
                months: FieldSet::all(1..=12),
                days_of_week,
            }),
            every: None,
            jitter: default_jitter(None),
        }
        .fitted()
        .validated()
    }

    /// On these weekdays, as often as the interval allows.
    ///
    /// The shape `--every 2w --on mon` asks for, and the one thing cron cannot
    /// express — which README.md, CHANGELOG.md and AGENTS.md all print as an
    /// example and the tool refused. Days with no time of day went to
    /// [`Schedule::calendar`], which is a set of minutes and so answers "a
    /// calendar needs a time of day" to one that names none; the interval was
    /// never looked at. Through `watch.toml` that is worse than a typo at a
    /// prompt, because `config::parse` accepts `every` beside `on`: an installed
    /// service refused at startup on every run, over a file it had accepted.
    ///
    /// So every minute of an allowed day is allowed, and the interval is what
    /// keeps two runs apart. **The interval is not optional here**, which is why
    /// it is an argument rather than something [`Schedule::and_every`] adds
    /// afterwards: on its own this grid names 1440 moments a day, and
    /// [`Schedule::validated`] refuses that and is right to — "on Mondays", with
    /// nothing else said, is not a schedule.
    ///
    /// The jitter this comes out with is zero, and that is the arithmetic
    /// working rather than failing. [`Schedule::room_for_jitter`] takes the
    /// interval as the step, and a grid whose tightest gap is one minute has
    /// nothing left over. What keeps a fortnightly run off the same second is
    /// the length of the walk in front of it, which the floor already measures
    /// from.
    ///
    /// Empty days are refused rather than read as every day, which is what
    /// [`Schedule::calendar`] does with them. There they still leave a time of
    /// day behind; here they would leave a calendar allowing every minute of
    /// every day — [`Schedule::every`] wearing a calendar, whereupon
    /// `is_on_a_calendar` answers `true` and a fresh install stops inventing the
    /// last run it needs.
    pub fn days(days: &[Weekday], interval: Duration) -> Result<Self, ScheduleError> {
        if days.is_empty() {
            return Err(ScheduleError::Unreadable(
                "which days? try --on mon,thu, or --every 6h for no particular day".to_string(),
            ));
        }

        Self {
            calendar: Some(Calendar {
                times: Times::Crossed {
                    minutes: FieldSet::all(0..=59),
                    hours: FieldSet::all(0..=23),
                },
                days_of_month: FieldSet::all(1..=31),
                months: FieldSet::all(1..=12),
                days_of_week: FieldSet(days.iter().fold(0u64, |bits, d| bits | (1 << d.number()))),
            }),
            every: Some(interval),
            jitter: default_jitter(Some(interval)),
        }
        .fitted()
        .validated()
    }

    /// A five-field cron expression.
    pub fn cron(expression: &str) -> Result<Self, ScheduleError> {
        Self {
            calendar: Some(parse_cron(expression)?),
            every: None,
            jitter: default_jitter(None),
        }
        .fitted()
        .validated()
    }

    /// Brings the jitter down to what this schedule can absorb.
    ///
    /// The default is a flat fifteen minutes, which is `MIN_GAP_SECS` — so on
    /// any grid tighter than half an hour the default alone was enough to eat
    /// the next moment. Applied by the constructors, so the default is bounded
    /// the same way an explicit `--jitter` is; [`Schedule::room_for_jitter`]
    /// says why the bound is what it is.
    fn fitted(mut self) -> Self {
        self.jitter = self.jitter.min(self.room_for_jitter());
        self
    }

    /// Adds a floor to a calendar, or a calendar to a floor.
    ///
    /// The two combine as a conjunction: the next run is the first moment that
    /// satisfies both.
    pub fn and_every(mut self, interval: Duration) -> Result<Self, ScheduleError> {
        self.every = Some(interval);
        if self.calendar.is_none() {
            self.jitter = default_jitter(Some(interval));
        }
        // Adding an interval to a calendar changes what separates two runs, and
        // so changes the room there is between them. `--every 2w --on mon` has a
        // weekly grid and a fortnightly floor, and no room at all.
        self.fitted().validated()
    }

    /// Sets how far a run may be pushed past its due moment.
    ///
    /// Bounded to what the schedule can absorb. This is the one builder that
    /// did not go through [`Schedule::validated`], so `--jitter 30d` on a
    /// six-hourly schedule was accepted and displaced runs by up to 27 days —
    /// the safe direction, but unbounded, and the same value can arrive from
    /// the configuration file rather than a typo. A jitter cannot sensibly
    /// exceed the gap it is jittering within.
    ///
    /// **Both halves bound it when both are set**, and it used to be the
    /// interval alone: `--every 2w --on mon --at 09:00 --jitter 5d` kept all
    /// five days, so a run due Monday at nine woke on Friday evening — a day the
    /// calendar does not allow, on a schedule that names one.
    #[must_use]
    pub fn with_jitter(mut self, jitter: Duration) -> Self {
        self.jitter = jitter.min(self.room_for_jitter());
        self
    }

    /// The most a run may be pushed later without costing the next one.
    ///
    /// **The gap between two moments is not the room there is inside it.** The
    /// floor is measured from where a run really landed, not from the grid, so
    /// every second of jitter comes straight out of the next gap: a run at
    /// moment `m` pushed to `m + s` cannot reach `m + gap` unless
    /// `s <= gap - step`, where `step` is the floor between two runs.
    ///
    /// Nothing subtracted the step, and the default for a calendar was a flat
    /// fifteen minutes — exactly `MIN_GAP_SECS`. So `--cron "*/15 * * * *"`, the
    /// tightest expression this tool advertises as legal, ran every thirty
    /// minutes with probability 899/900 while the banner printed `*/15`, and
    /// `0,20,40` fired under twice an hour on an expression naming three.
    ///
    /// The step is the interval when there is one, because that is then what
    /// separates two runs. `--every 2w --on mon` is a Monday in every two, and
    /// its floor is a fortnight while its grid is a week: the subtraction
    /// answers zero, which is right — any jitter at all turns it into three
    /// weeks.
    fn room_for_jitter(&self) -> Duration {
        // One moment a day at most. Whatever the day fields allow, the next is
        // at least a day away — and a roll is in `[0, 1)`, so a run still lands
        // strictly before it.
        const A_DAY: i64 = 24 * 3_600;

        let step = self
            .every
            .map(|every| i64::try_from(every.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0)
            .max(MIN_GAP_SECS);

        match &self.calendar {
            Some(calendar) => {
                let gap = calendar.tightest_gap().unwrap_or(A_DAY);
                let mut room = gap.saturating_sub(step).max(0);
                // **And not past midnight, when the days are restricted.** With
                // one moment a day `tightest_gap` is `None` and the fallback is
                // a whole day, however narrow the day fields are — so
                // `--on mon --at 09:00 --jitter 20h` kept all twenty hours and a
                // run due Monday morning walked on Tuesday at three, which is
                // the failure this bound exists to stop, named in as many words
                // in `with_jitter`'s own doc. It reaches a calendar with several
                // moments too: the last one of the day has the same midnight in
                // front of it whatever the gap behind it was.
                //
                // Unrestricted days need no cap: the next day is one the
                // calendar names, so a run that lands there has landed
                // somewhere legal.
                if calendar.days_are_restricted() {
                    room = room.min(calendar.room_before_midnight());
                }
                Duration::from_secs(room as u64)
            }
            // No grid to miss, so the interval is the only bound. What that
            // costs is said rather than assumed: the loop records the
            // jittered moment as the last run and the floor is measured from
            // it, so every roll is added for good and the mean period is the
            // interval plus the mean jitter -- `--every 6h --jitter 6h` runs
            // about 160 times in sixty days where 240 were implied. The
            // default jitter is a tenth of the interval capped at fifteen
            // minutes, so it costs a few per cent; only an explicit --jitter
            // reaches further, and that is what the person asked for. The
            // calendar arm above subtracts the step because a grid moment
            // missed is a run lost; here nothing is lost, only later.
            //
            // `validated` refuses a schedule with neither half, so the fallback
            // is unreachable; a day is the answer that cannot be wrong by much.
            None => self.every.unwrap_or(Duration::from_secs(A_DAY as u64)),
        }
    }

    /// The room one particular moment really has, in real seconds.
    ///
    /// [`Schedule::room_for_jitter`] asks the same question of the grid, and the
    /// grid is seconds-of-day arithmetic: its `A_DAY` is 86400 and
    /// `room_before_midnight` is `86400 - latest`. A day a zone springs forward
    /// through is 82800 seconds long, so both are an hour too generous on it —
    /// once a year, in the one direction that costs a run.
    ///
    /// So this asks the calendar itself, in the zone, from where the run is
    /// actually due: the next moment the schedule names, less the floor between
    /// two runs, and — when the days are restricted — no further than the local
    /// midnight after `due_at`, which is the bound `room_before_midnight` draws
    /// and this is the real version of it.
    ///
    /// It only ever narrows, which is what keeps the banner honest: `jitter` has
    /// already been capped at `room_for_jitter`, so what was printed stays an
    /// upper bound on what is used.
    ///
    /// **An interval has no grid to miss**, and it is measured from where the
    /// run landed rather than from a moment — the whole schedule slides, so
    /// there is nothing to take off. Unbounded here, and bounded as before by
    /// `room_for_jitter`.
    fn room_at<Tz: TimeZone>(&self, due_at: i64, zone: &Tz) -> Duration {
        let Some(calendar) = &self.calendar else {
            return Duration::MAX;
        };

        let step = self
            .every
            .map(|every| i64::try_from(every.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0)
            .max(MIN_GAP_SECS);

        // `None` is a calendar that names nothing after this moment, which has
        // nothing left to lose.
        let mut room = match next_after(self, Some(due_at), due_at, zone) {
            Some(next) => (next - due_at).saturating_sub(step).max(0),
            None => i64::MAX,
        };

        if calendar.days_are_restricted()
            && let Some(midnight) = next_local_midnight(due_at, zone)
        {
            room = room.min((midnight - due_at).max(0));
        }
        Duration::from_secs(room as u64)
    }

    pub fn jitter(&self) -> Duration {
        self.jitter
    }

    /// Refuses a schedule that would run more often than the floor allows.
    ///
    /// **Both halves are checked, not just the interval.** This used to look at
    /// `every` alone, and `cron` did not call it at all — so `--every 5m` was
    /// refused while `--cron "*/5 * * * *"` and `--at 09:00,09:05` were
    /// accepted and ran exactly as often. The message said "the monitor does not
    /// run twice inside fifteen minutes" while two of the three ways of asking
    /// for it did.
    fn validated(self) -> Result<Self, ScheduleError> {
        if self.calendar.is_none() && self.every.is_none() {
            return Err(ScheduleError::Empty);
        }
        if let Some(every) = self.every {
            // Compared as a `u64`, not cast to `i64`. The cast wrapped, so
            // `--every 18446744073709551615` was refused with "18446744073709551615s
            // is too often" — the opposite of what was wrong with it.
            if every.as_secs() < MIN_GAP_SECS as u64 {
                return Err(ScheduleError::TooOften {
                    interval: crate::duration::format(every),
                    minutes: MIN_GAP_SECS / 60,
                });
            }
            // And an upper bound, because there was none. A `watch.toml` saying
            // `every = "9223372036854775807"` parsed, validated, and then
            // overflowed `last + every`: a panic in a debug build, and in
            // release a wrap to a negative floor that ran the monitor every
            // fifteen minutes forever.
            if every.as_secs() > MAX_INTERVAL_SECS as u64 {
                return Err(ScheduleError::TooRare {
                    interval: crate::duration::format(every),
                    days: MAX_INTERVAL_SECS / (24 * 3_600),
                });
            }
        }

        // A calendar with an interval on it is bounded by the interval, which
        // has just been checked. On its own, the calendar's own tightest gap is
        // what decides.
        if self.every.is_none()
            && let Some(calendar) = &self.calendar
            && let Some(gap) = calendar.tightest_gap()
            && gap < MIN_GAP_SECS
        {
            return Err(ScheduleError::TooOften {
                interval: crate::duration::format(Duration::from_secs(gap as u64)),
                minutes: MIN_GAP_SECS / 60,
            });
        }
        Ok(self)
    }
}

/// Whether a run is owed, and when the next one is.
///
/// **"It names no moment" is a variant, not an instant.** It used to be
/// `At(i64::MAX)`, and the loop told the two apart by matching that literal in
/// an arm written above the general one — so the whole guard was an arm order
/// and a number, both of which anything else in either file was free to move.
/// Let the sentinel through and every reader downstream treats it as a moment:
/// `with_jitter` adds to it and saturates, `wake_at` hands back `i64::MAX`, and
/// the loop naps sixty seconds at a time until the end of time. No error, no
/// exit code, and a process that looks perfectly healthy to whatever is watching
/// it. That is the worst failure this loop has available and it was one
/// reordering away. As a variant, the compiler asks for the arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Nothing yet. Sleep until this instant, an epoch in seconds.
    At(i64),
    /// Run now. `missed` counts the scheduled runs being folded into this one.
    Now { missed: u32 },
    /// The schedule names no moment at all — `0 0 31 2 *`, and nothing else this
    /// parser accepts. There is nothing to sleep until, so the caller has to say
    /// so rather than wait. The same fact [`next_moment`] answers `None` with,
    /// which is where the two readings came apart.
    Never,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Datelike, Timelike, Utc};

    /// Midnight UTC on Monday 2026-08-17. Every timestamp below is an offset
    /// from it, so the weekday arithmetic is checkable by hand.
    pub(super) const MONDAY_0000: i64 = 1_786_924_800;

    /// The constant has to be the day it claims to be. It was three tests'
    /// worth of confusing failures when it was a Friday.
    #[test]
    fn the_fixture_really_is_a_monday() {
        let day = Utc.timestamp_opt(MONDAY_0000, 0).unwrap();
        assert_eq!(day.weekday(), chrono::Weekday::Mon);
        assert_eq!((day.hour(), day.minute()), (0, 0));
    }

    pub(super) fn at(offset_secs: i64) -> i64 {
        MONDAY_0000 + offset_secs
    }

    pub(super) fn hours(n: i64) -> i64 {
        n * 3_600
    }

    /// A jitter larger than the room it is jittering within is not a jitter.
    ///
    /// The room is the gap **less the floor between two runs**, not the gap.
    /// The floor is measured from where a run really landed, so every second of
    /// jitter comes out of the next gap — which is why the two calendar
    /// expectations here are not the round numbers they used to be.
    #[test]
    fn jitter_is_bounded_by_what_the_schedule_can_absorb() {
        let every = Duration::from_secs(hours(6) as u64);
        let schedule = Schedule::every(every)
            .unwrap()
            .with_jitter(Duration::from_secs(30 * 24 * 3_600));
        assert_eq!(
            schedule.jitter(),
            every,
            "an interval has no grid to miss: what moves is the whole schedule"
        );

        let calendar = Schedule::calendar(&[], &[(9, 0), (21, 0)])
            .unwrap()
            .with_jitter(Duration::from_secs(30 * 24 * 3_600));
        assert_eq!(
            calendar.jitter(),
            Duration::from_secs((hours(12) - MIN_GAP_SECS) as u64),
            "a full twelve hours would put the nine o'clock run inside the floor \
             of the nine in the evening"
        );

        // With both halves set, what separates two runs is the interval, and
        // here it is longer than the grid: a Monday in every two, on a weekly
        // grid, has no room at all. It used to answer a whole day — and a day of
        // jitter on a fortnightly Monday puts the next run past the Monday it
        // was due on, so the fortnight quietly became three weeks.
        let both = Schedule::calendar(&[Weekday::Mon], &[(9, 0)])
            .unwrap()
            .and_every(Duration::from_secs(14 * 24 * 3_600))
            .unwrap()
            .with_jitter(Duration::from_secs(5 * 24 * 3_600));
        assert_eq!(both.jitter(), Duration::ZERO);
    }

    /// Jitter cannot move a run onto a day the calendar forbids.
    ///
    /// With one moment a day `tightest_gap` is `None` and the room fell back to
    /// a whole day, however narrow the day fields were — so
    /// `--on mon --at 09:00 --jitter 20h` kept all twenty hours and a run due
    /// Monday morning walked on Tuesday at three. That is the failure
    /// `with_jitter`'s own doc names as the reason the bound exists, arriving
    /// through the branch the bound does not cover.
    #[test]
    fn jitter_cannot_move_a_run_onto_a_day_the_calendar_forbids() {
        let schedule = Schedule::calendar(&[Weekday::Mon], &[(9, 0)])
            .unwrap()
            .with_jitter(Duration::from_secs(20 * 3_600));

        let due_at = at(hours(9));
        let woken = with_jitter(due_at, schedule.jitter(), 0.999_999);

        let landed = DateTime::from_timestamp(woken, 0).unwrap();
        assert_eq!(
            landed.weekday(),
            chrono::Weekday::Mon,
            "due Monday at nine, woke {landed} — a day this schedule does not name"
        );

        // Every day allowed is the other case, and it needs no cap: the day a
        // run spills into is one the calendar names.
        let daily = Schedule::calendar(&[], &[(9, 0)])
            .unwrap()
            .with_jitter(Duration::from_secs(20 * 3_600));
        assert_eq!(
            daily.jitter(),
            Duration::from_secs(20 * 3_600),
            "nothing to protect when tomorrow is allowed too"
        );
    }

    /// An interval nothing could serve is refused, and says which way it is
    /// wrong.
    ///
    /// There was no upper bound. `every = "9223372036854775807"` in a
    /// hand-edited `watch.toml` parsed and validated, and then adding it to the
    /// previous run overflowed: a panic in a debug build, and in release a wrap
    /// to a negative floor, so an interval of billions of years ran every
    /// fifteen minutes. `u64::MAX` was refused, but as "too often".
    #[test]
    fn an_interval_too_long_to_add_up_is_refused_as_too_long() {
        for seconds in [u64::MAX, i64::MAX as u64, MAX_INTERVAL_SECS as u64 + 1] {
            let refused = Schedule::every(Duration::from_secs(seconds));
            assert!(
                matches!(refused, Err(ScheduleError::TooRare { .. })),
                "{seconds}s: {refused:?}"
            );
        }

        // And the largest one that is allowed still answers without overflowing.
        let schedule = Schedule::every(Duration::from_secs(MAX_INTERVAL_SECS as u64)).unwrap();
        assert_eq!(
            due(&schedule, Some(at(0)), at(1), &Utc),
            Due::At(at(MAX_INTERVAL_SECS))
        );
    }

    /// The floor applies to every way of asking, not only to `--every`.
    ///
    /// `cron` did not call `validated()` at all and `validated()` only looked at
    /// the interval, so the tool refused `--every 5m` while accepting
    /// `--cron "*/5 * * * *"` and `--at 09:00,09:05` — which run just as often.
    /// The error message claimed a rule two of the three ways round it.
    #[test]
    fn nothing_gets_past_the_minimum_gap() {
        for expression in ["* * * * *", "*/5 * * * *", "0,10 * * * *"] {
            assert!(
                Schedule::cron(expression).is_err(),
                "\"{expression}\" runs more often than the floor allows"
            );
        }
        assert!(Schedule::calendar(&[], &[(9, 0), (9, 5)]).is_err());
        assert!(Schedule::every(Duration::from_secs(300)).is_err());
    }

    /// And what is spaced widely enough still goes through, including the
    /// shapes people actually write.
    #[test]
    fn an_ordinary_schedule_is_not_caught_by_the_floor() {
        for expression in ["0 9 * * *", "0 */6 * * *", "0 9 * * 1,4", "0,30 * * * *"] {
            assert!(
                Schedule::cron(expression).is_ok(),
                "\"{expression}\" should be allowed"
            );
        }
        assert!(Schedule::calendar(&[], &[(9, 0), (21, 30)]).is_ok());
        assert!(Schedule::every(Duration::from_secs(MIN_GAP_SECS as u64)).is_ok());
    }

    #[test]
    fn the_default_jitter_scales_with_the_interval_and_is_capped() {
        assert_eq!(
            default_jitter(Some(Duration::from_secs(hours(6) as u64))),
            Duration::from_secs(900),
            "a tenth of six hours is over the cap, so it is the cap"
        );
        assert_eq!(
            default_jitter(Some(Duration::from_secs(hours(1) as u64))),
            Duration::from_secs(360)
        );
        assert_eq!(
            default_jitter(Some(Duration::from_secs(MIN_GAP_SECS as u64))),
            Duration::from_secs(90)
        );
    }

    /// An interval below the floor is refused when it is set, not silently
    /// stretched later -- and the refusal says what it costs.
    #[test]
    fn an_interval_below_the_minimum_gap_is_refused_with_a_reason() {
        let error = Schedule::every(Duration::from_secs(300)).unwrap_err();
        assert!(matches!(error, ScheduleError::TooOften { .. }));
        assert!(error.to_string().contains("15 minutes"), "{error}");
    }

    #[test]
    fn a_schedule_with_neither_half_is_refused() {
        assert_eq!(
            Schedule::calendar(&[Weekday::Mon], &[]).unwrap_err(),
            ScheduleError::Unreadable("a calendar needs a time of day: try --at 09:00".to_string())
        );
    }
}
