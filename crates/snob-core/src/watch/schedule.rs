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

use std::time::Duration;

use chrono::{DateTime, Datelike, MappedLocalTime, TimeZone, Timelike};

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

/// A set of small numbers, as a bitmask.
///
/// Every cron field fits in 64 bits — the widest is day-of-month at 31 — so a
/// field is one integer and testing membership is one shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FieldSet(u64);

impl FieldSet {
    /// Everything in `range`, which is what `*` means.
    fn all(range: std::ops::RangeInclusive<u32>) -> Self {
        let mut bits = 0u64;
        for value in range {
            bits |= 1 << value;
        }
        Self(bits)
    }

    fn contains(self, value: u32) -> bool {
        value < 64 && self.0 & (1 << value) != 0
    }

    /// Whether this field was left unrestricted over `range`.
    ///
    /// Only day-of-month and day-of-week need to know, and only to settle how
    /// they combine. It is asked of the set rather than remembered from the
    /// text so that `*` and `1-31` behave identically, which is what a reader
    /// of the expression would expect.
    fn is_all(self, range: std::ops::RangeInclusive<u32>) -> bool {
        self == Self::all(range)
    }
}

/// Which minutes a run is allowed to happen in.
///
/// The hour and minute fields are independent, which is what cron means and is
/// **not** what `--at` means. `0 9,21 * * *` is nine and twenty-one o'clock on
/// the hour; `--at 09:00,21:30` is two specific moments, and reading it as
/// cron's product gives four — 09:00, 09:30, 21:00 and 21:30, doubling what
/// anybody asked for. So a calendar built from times of day carries them as
/// pairs and matches on those instead — which of the two readings applies is
/// [`Times`], and [`Calendar::allows`] is where they come back together.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Calendar {
    times: Times,
    days_of_month: FieldSet,
    months: FieldSet,
    /// Sunday is 0, the way cron numbers them.
    days_of_week: FieldSet,
}

/// Which times of day a calendar allows, in whichever of the two readings the
/// syntax it came from means.
///
/// An enum rather than the three fields it replaced. `--at` filled the minute
/// and hour sets *as well as* the pairs, with a comment about a caller that
/// might only want to ask whether an hour was allowed — and no such caller can
/// exist, because both readers branch on whether the pairs are empty. So the two
/// sets were written and never read on that path, and a reader had to work out
/// from the branch which of the two shapes was live.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Times {
    /// The `(hour, minute)` pairs somebody named, as they named them.
    Exact(Vec<(u32, u32)>),
    /// The two fields, to be crossed. What a cron expression means.
    Crossed { minutes: FieldSet, hours: FieldSet },
}

impl Times {
    fn allows(&self, hour: u32, minute: u32) -> bool {
        match self {
            Self::Exact(times) => times.contains(&(hour, minute)),
            Self::Crossed { minutes, hours } => hours.contains(hour) && minutes.contains(minute),
        }
    }

    /// Every allowed moment of the day, as seconds since midnight.
    fn moments(&self) -> Vec<i64> {
        match self {
            Self::Exact(times) => times
                .iter()
                .map(|&(h, m)| i64::from(h) * 3_600 + i64::from(m) * 60)
                .collect(),
            Self::Crossed { minutes, hours } => (0..24u32)
                .filter(|&hour| hours.contains(hour))
                .flat_map(|hour| {
                    (0..60u32)
                        .filter(|&minute| minutes.contains(minute))
                        .map(move |minute| i64::from(hour) * 3_600 + i64::from(minute) * 60)
                })
                .collect(),
        }
    }
}

impl Calendar {
    /// Whether this moment is one the calendar allows.
    ///
    /// The day rule is cron's, and it is the part worth reading twice. When
    /// both day fields are restricted, a moment matching **either** is allowed:
    /// `0 9 13 * 5` is "the 13th, and every Friday", not "Friday the 13th".
    /// When one of them is `*`, only the other one decides. Every
    /// implementation that gets this wrong gets it wrong quietly.
    fn allows<Tz: TimeZone>(&self, at: &DateTime<Tz>) -> bool {
        if !self.times.allows(at.hour(), at.minute()) {
            return false;
        }
        if !self.months.contains(at.month()) {
            return false;
        }

        let dom_restricted = !self.days_of_month.is_all(1..=31);
        let dow_restricted = !self.days_of_week.is_all(0..=6);
        let dom = self.days_of_month.contains(at.day());
        let dow = self
            .days_of_week
            .contains(at.weekday().num_days_from_sunday());

        match (dom_restricted, dow_restricted) {
            (true, true) => dom || dow,
            (true, false) => dom,
            (false, true) => dow,
            (false, false) => true,
        }
    }

    /// The shortest gap between two moments this calendar allows, in seconds.
    ///
    /// Only the minutes and hours are looked at, and that is enough: the day
    /// fields can only ever make the gaps *longer*, so a calendar whose
    /// within-a-day spacing is acceptable is acceptable however it is
    /// restricted by day. What this catches is `*/5 * * * *` and `09:00,09:05`.
    ///
    /// `None` when there is only one allowed minute in the whole day, where the
    /// gap is a day and there is nothing to refuse.
    fn tightest_gap(&self) -> Option<i64> {
        let mut allowed = self.times.moments();
        allowed.sort_unstable();
        allowed.dedup();

        if allowed.len() < 2 {
            return None;
        }
        // The wrap from the last of one day to the first of the next counts:
        // 23:59 and 00:00 are a minute apart, not twenty-four hours.
        //
        // But only when there really is a next day. It was counted
        // unconditionally, so `--on mon --at 00:00,23:55` was refused as "5m is
        // too often" about two runs 23h55m apart: with only Monday allowed, the
        // 23:55 run is followed by the *next* Monday's midnight.
        let wrap = self
            .days_can_be_consecutive()
            .then(|| 24 * 3_600 - allowed[allowed.len() - 1] + allowed[0]);
        allowed
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .chain(wrap)
            .min()
    }

    /// Whether some day this calendar allows can be immediately followed by
    /// another.
    ///
    /// Only [`Calendar::tightest_gap`] asks, to decide whether the wrap around
    /// midnight is a gap between two runs or a week of waiting.
    /// Whether the day fields name a subset of the days rather than all of them.
    ///
    /// The jitter ceiling asks, because pushing a run past midnight is only
    /// harmless when the next day is one the calendar allows too.
    fn days_are_restricted(&self) -> bool {
        !self.days_of_month.is_all(1..=31) || !self.days_of_week.is_all(0..=6)
    }

    /// How much of the day is left after its last moment.
    ///
    /// The bound on jitter when the days are restricted: past midnight is a day
    /// this calendar does not name, and a run that lands there is the failure
    /// [`Schedule::with_jitter`]'s doc says the bound exists to stop.
    fn room_before_midnight(&self) -> i64 {
        let latest = self.times.moments().into_iter().max().unwrap_or(0);
        24 * 3_600 - latest
    }

    fn days_can_be_consecutive(&self) -> bool {
        let dom_restricted = !self.days_of_month.is_all(1..=31);
        let dow_restricted = !self.days_of_week.is_all(0..=6);

        match (dom_restricted, dow_restricted) {
            // Nothing restricts the day, so every day is allowed and each is
            // followed by the next.
            (false, false) => true,
            // Restricted in both, where `allows` combines them with OR — so the
            // set of allowed days is the union, which is larger and not smaller.
            // Taken as possible rather than worked out across a whole month:
            // this answer only ever feeds a refusal and a jitter ceiling, and
            // both have to err towards the tighter number.
            (true, true) => true,
            (true, false) => {
                (1..31).any(|d| self.days_of_month.contains(d) && self.days_of_month.contains(d + 1))
                    // A month's last day followed by the first of the next. Which
                    // day that is depends on the month, so any of them counts.
                    || (self.days_of_month.contains(1)
                        && (28..=31).any(|d| self.days_of_month.contains(d)))
            }
            (false, true) => (0..7)
                .any(|d| self.days_of_week.contains(d) && self.days_of_week.contains((d + 1) % 7)),
        }
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
            // No grid to miss. An interval is measured from the previous run, so
            // what the jitter moves is the whole schedule rather than one run
            // out of it, and the interval itself is the only bound needed.
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

/// How far ahead a calendar is searched before it is called impossible.
///
/// Four years covers a leap day, which is the longest anything expressible here
/// can legitimately wait for. Past that the expression matches nothing —
/// `0 0 31 2 *` — and the search has to stop rather than spin.
const HORIZON_MINUTES: i64 = 4 * 366 * 24 * 60; // four years, in minutes

/// The next moment this schedule is due, for somebody who wants to see it
/// rather than sleep until it.
///
/// The same function the loop uses, deliberately: a preflight that worked the
/// moments out its own way would be checking a schedule nobody runs. `None`
/// means the calendar can never match, which is the one answer worth a red line
/// before anything is scheduled at all.
pub fn next_moment<Tz: TimeZone>(
    schedule: &Schedule,
    last_run: Option<i64>,
    now: i64,
    zone: &Tz,
) -> Option<i64> {
    next_after(schedule, last_run, now, zone)
}

/// The next moment this schedule is due after `now`.
///
/// `None` only when the calendar can never match. Both arguments and the answer
/// are epoch seconds; the zone is what turns them into wall-clock time, and it
/// is passed in so a test can pick one rather than inherit the machine's.
fn next_after<Tz: TimeZone>(
    schedule: &Schedule,
    last_run: Option<i64>,
    now: i64,
    zone: &Tz,
) -> Option<i64> {
    // The floor. Counted from the previous run rather than from an absolute
    // grid: `--every 6h` started at 09:13 means 15:13, which is what people
    // mean by it.
    //
    // **`MIN_GAP_SECS` is part of the floor, and that is where it has to be.**
    // It used to be checked after the search, in `due`, and a calendar never
    // reached the check: `Due::Now` needs the answer to equal `now` to the
    // second, and the grid the search returns is minute-aligned while the runs
    // happen off it — jitter moves a run later and the grid does not move with
    // it. So `--cron "*/15 * * * *"` breached the floor on half of its
    // consecutive pairs, worst case 36 seconds apart, and when the check *was*
    // reached it answered `last + MIN_GAP_SECS` raw: an instant the calendar
    // does not allow, so `--cron "0,20,40 * * * *" --jitter 0` ran six times an
    // hour, at :20, :35, :40, :55, on an expression that names three. Folded
    // into the search start, the answer is always both on the grid and far
    // enough from the last run.
    //
    // `saturating_add` because an absurd `--every` can arrive from a
    // hand-edited `watch.toml`: the sum overflowed, which panicked a debug
    // build and in release wrapped to a negative floor, turning an interval of
    // billions of years into one that ran every fifteen minutes.
    let interval = schedule
        .every
        .map(|every| i64::try_from(every.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);

    let Some(calendar) = &schedule.calendar else {
        // No grid. The floor counts from the run, which is exactly what an
        // interval means: `--every 6h` started at 09:13 means 15:13.
        //
        // Nothing has run: there is no past to wait from, and no run to be too
        // close to.
        let floor = last_run.map_or(now, |last| last.saturating_add(interval.max(MIN_GAP_SECS)));
        return Some(floor.max(now));
    };

    // **With a grid, the floor counts from the moment the last run served, not
    // from the row it wrote.** They are not the same instant and the difference
    // is not small: `watch_runs.started_at` is stamped by `tick` *after* both
    // lists have been walked, so it trails the moment by the whole length of the
    // walk.
    //
    // Measured from the row, the floor was pushed past the next moment on the
    // grid and that moment was dropped — so a schedule was accepted and then run
    // at half its rate, permanently and in silence. `*/15` broke for any walk at
    // all, since its gap is exactly the floor; `0,20,40` broke on a walk over
    // five minutes. Measuring from the moment served is what makes the grid the
    // thing that decides, which is what `validated()` already assumes when it
    // refuses a grid tighter than the floor.
    //
    // The price, stated plainly: two runs can now be closer together in wall
    // clock than `MIN_GAP_SECS`, by however long the walk took. They still
    // cannot overlap — the loop is sequential — and the reason the floor exists
    // is that a second run cannot see a state the first did not, which is a
    // statement about the moments, not about the instants the process happened
    // to write a row at.
    //
    // `saturating_add` because an absurd `--every` can arrive from a hand-edited
    // `watch.toml`: the sum overflowed, which panicked a debug build and in
    // release wrapped to a negative floor, turning an interval of billions of
    // years into one that ran every fifteen minutes.
    let served = last_run.map(|last| moment_served(calendar, last, zone));
    let floor = match (served, last_run) {
        // Never earlier than the row the last run wrote, whatever the snap
        // decided. A no-op in every ordinary case — the floor is a quarter of an
        // hour past a moment the row already trails — and the thing that keeps
        // this monotonic if a calendar is ever strange enough to snap somewhere
        // unhelpful.
        (Some(moment), Some(last)) => moment.saturating_add(interval.max(MIN_GAP_SECS)).max(last),
        _ => now,
    };

    // A local time that does not exist — the hour a spring-forward skips —
    // maps to no instant, so it is stepped over rather than invented.
    //
    // **`single()` here is not what refuses an ambiguous one.** It reads as if
    // it were, and the comment that used to sit here said so, but
    // `timestamp_opt` goes from an instant to a local time and that direction is
    // never ambiguous: chrono answers `Single` unconditionally and only fails
    // outside the representable range. Ambiguity is a property of the other
    // direction, and it is asked about below.
    let allowed = |minute: &i64| {
        let seconds = minute * 60;
        let Some(at) = zone.timestamp_opt(seconds, 0).single() else {
            return false;
        };
        calendar.allows(&at) && !already_run_at_this_wall_clock(zone, seconds, &at, last_run)
    };

    // **A moment that has already gone by is still owed.** The search only ever
    // looked forward, and it works in whole minutes, so the answer was always
    // the *ceiling* minute of `now` — never `now` itself unless the clock
    // happened to read exactly `:00`. `store::now()` is an arbitrary second, so
    // `due` produced `Due::Now` for a calendar with probability of about one in
    // nine hundred, and every calendar run was therefore scheduled forward
    // instead of taken.
    //
    // What that cost is the whole point of the feature: a machine powered on at
    // 09:05 with `--at 09:00` waited until the next day, a laptop that suspended
    // across the moment lost the run outright, and the calendar branch of
    // `missed_since` together with the "runs were missed" line were unreachable
    // code — while the CHANGELOG said missed runs are folded into one and
    // reported.
    //
    // So before looking forward, look back: if the floor allows it and any
    // minute between the floor and now was one this calendar names, the run is
    // owed **now**. Folded into one and never replayed, which is what
    // `missed_since` counts and what the rule against bursts requires.
    //
    // Walking backwards from `now` rather than forwards from the floor, so the
    // first hit is the most recent one and a machine that was off for a month
    // stops after a day's worth of minutes rather than a month's. The horizon
    // bounds it for the same reason it bounds the forward search.
    // **The window is bounded by the last run, not by the floor.** The floor
    // stays as the legality guard — `floor <= now` — but using it as the bottom
    // of the window dropped every moment between the two, and a run is almost
    // never recorded exactly on its grid minute: `commands::watch` stores the
    // wall clock, so one second past is the ordinary case. `*/15` recorded at
    // 09:00:01 has its floor at 09:15:01, so the 09:15 moment is below the
    // window and above nothing — neither taken nor scheduled. It ran every half
    // hour; `--every 1w --on mon --at 09:00` became fortnightly.
    //
    // Strictly after the last run's own minute, so the moment just served is not
    // served twice.
    //
    // With nothing ever run, the window is the minute `now` is in. It was
    // `now` itself, which is a whole minute only on the second — so a first
    // `snob watch --on mon --at 09:00` started at 09:00:00 ran, and the same
    // command started at 09:00:20 waited a week. That is the `:00`-to-the-second
    // discontinuity taken out of the `last_run` path, left behind where a fresh
    // install passes `None`.
    // From the moment served rather than from the row, for the reason the floor
    // is: what must not be served twice is the moment, and everything after it
    // is owed.
    let earliest = match served {
        Some(moment) => moment.div_euclid(60) + 1,
        None => now.div_euclid(60),
    };
    if floor <= now {
        let latest = now.div_euclid(60);
        if (earliest..=latest)
            .rev()
            .take(HORIZON_MINUTES as usize)
            .any(|minute| allowed(&minute))
        {
            return Some(now);
        }
    }

    // Forward, from the floor. **The answer is on the grid and past the floor,
    // both**, which is the rule AGENTS.md states and the test below sweeps: an
    // instant the expression does not name is not an answer, and neither is one
    // inside the minimum gap.
    //
    // A grid moment that falls between the last run and the floor is therefore
    // not served at its own moment. It is still served — the look back above
    // finds it the next time the loop is awake past the floor — but on a grid as
    // tight as `*/15`, where the floor is the whole gap, a run recorded a second
    // past its moment pushes the next one under the floor and the cadence
    // halves. That is a real cost, and the only way out of it is to measure the
    // floor from the moment a run served rather than from when the row was
    // written, which changes what `MIN_GAP_SECS` means. Not a thing to change
    // in passing.
    let start = floor.max(now);
    let first = start.div_euclid(60) + i64::from(start.rem_euclid(60) != 0);

    (first..)
        .take(HORIZON_MINUTES as usize)
        .find(allowed)
        .map(|minute| minute * 60)
}

/// The moment on the grid that a run recorded at `last` was serving.
///
/// The most recent minute the calendar names at or before it. A run does not
/// start on its moment and does not record itself on it either: the loop wakes
/// at the moment plus whatever jitter it rolled, and `tick` stamps the row after
/// both lists have been walked. Everything between the two is the process's own
/// latency, and none of it is time the schedule asked for.
///
/// Falls back to `last` itself when the calendar names nothing in the whole
/// horizon — an expression that matches no moment at all, where the old
/// behavior is as good an answer as any and nothing is owed regardless.
///
/// Deliberately asks `Calendar::allows` and not the fuller predicate the search
/// uses: a repeated wall-clock hour is refused for a run that has *not* happened
/// yet, and this is looking at one that has.
fn moment_served<Tz: TimeZone>(calendar: &Calendar, last: i64, zone: &Tz) -> i64 {
    let from = last.div_euclid(60);
    (0..)
        .take(HORIZON_MINUTES as usize)
        .map(|back| from - back)
        .find(|minute| {
            zone.timestamp_opt(minute * 60, 0)
                .single()
                .is_some_and(|at| calendar.allows(&at))
        })
        .map_or(last, |minute| minute * 60)
}

/// Whether this instant is the **second** showing of a wall-clock time the last
/// run already used.
///
/// The hour a fall-back repeats happens twice, so `--at 01:30` in a zone that
/// puts its clocks back names two instants an hour apart on that date. Both
/// satisfy the calendar, and the floor does not separate them: `MIN_GAP_SECS` is
/// fifteen minutes and they are sixty apart. So the monitor walked both lists
/// again and posted a second webhook for a schedule that names one run a day.
///
/// The comment where `single()` is called claimed to prevent this and the test
/// beside it credited `MIN_GAP_SECS`; neither was true, and the test could not
/// have caught it because it used a `FixedOffset`, which has no transitions.
///
/// Asked in the direction ambiguity actually exists in: from a local wall-clock
/// time to an instant. `Ambiguous` gives both, and the later one is only refused
/// when the earlier one is at or before the last run — so a fall-back date on a
/// machine that was switched off through the first showing still runs.
fn already_run_at_this_wall_clock<Tz: TimeZone>(
    zone: &Tz,
    at_secs: i64,
    local: &DateTime<Tz>,
    last_run: Option<i64>,
) -> bool {
    let Some(last) = last_run else {
        return false;
    };
    match zone.from_local_datetime(&local.naive_local()) {
        MappedLocalTime::Ambiguous(first, second) => {
            second.timestamp() == at_secs && first.timestamp() <= last
        }
        _ => false,
    }
}

/// Whether a run is owed right now.
///
/// Missed runs are **counted and folded into one**, never replayed. Twelve ticks
/// fired back to back to catch up would be exactly the burst the pacing design
/// exists to avoid — and they would all give the same answer anyway, because
/// the diff is against the last report and there is only one present state.
/// There is nothing to catch up on; there is one thing to report.
pub fn due<Tz: TimeZone>(schedule: &Schedule, last_run: Option<i64>, now: i64, zone: &Tz) -> Due {
    // A clock that went backwards. The same reading `rate_budget` takes: trust
    // the present over a stored future, rather than refusing to run for however
    // long the skew was.
    let last_run = last_run.map(|last| last.min(now));

    // Nothing has run, and a floor with no past to measure from is now.
    let Some(next) = next_after(schedule, last_run, now, zone) else {
        // An impossible calendar. Never due, and the caller says so rather than
        // sleeping on a moment that will not come.
        return Due::Never;
    };

    if next > now {
        return Due::At(next);
    }

    // No floor check here. `next_after` puts the floor into the start of its
    // search, and unlike a check bolted on afterwards the answer it gives is one
    // the calendar allows. Checking it in this position was the bug: for a
    // calendar the code below was all but unreachable, and when it was reached
    // it answered off-grid.
    //
    // What is asserted is what is true. It used to be `now - last >=
    // MIN_GAP_SECS`, and that is deliberately no longer the rule: with a grid
    // the floor counts from the moment a run served rather than from the row it
    // wrote, so two runs can be closer in wall clock than the gap by however
    // long the walk took. `next_after` says why. What still holds either way is
    // that a run is never due before the previous one recorded itself.
    debug_assert!(
        last_run.is_none_or(|last| now >= last),
        "a run is due before the one behind it"
    );

    Due::Now {
        missed: missed_since(schedule, last_run, now, zone),
    }
}

/// How many scheduled runs went by unrun, not counting the one due now.
///
/// Counted against whatever actually decides the schedule. Dividing the elapsed
/// time by the interval is only right when the interval is the whole of it: with
/// `--every 1h --on mon --at 09:00` a week of downtime is one missed run, and
/// the arithmetic said 167. The number is printed at the user, so a wrong one is
/// a sentence that is simply false.
///
/// Bounded by `MAX_MISSED_COUNTED`, because the answer is only ever used to say
/// "several" out loud and walking a calendar minute by minute over a year of
/// downtime is not worth doing to reach a larger number.
fn missed_since<Tz: TimeZone>(
    schedule: &Schedule,
    last_run: Option<i64>,
    now: i64,
    zone: &Tz,
) -> u32 {
    let Some(last) = last_run else {
        return 0; // nothing has run, so nothing was missed
    };

    // No calendar: the interval is the schedule, and division is exact.
    if schedule.calendar.is_none() {
        let Some(every) = schedule.every else {
            return 0;
        };
        let interval = every.as_secs().max(1) as i64;
        return (((now - last) / interval).max(1) - 1).min(MAX_MISSED_COUNTED as i64) as u32;
    }

    // With one, the moments it allows have to be walked. Each step starts from
    // the moment found, so `every` keeps acting as the floor it is.
    //
    // Every moment up to and including `now` is walked, and **the last one is
    // the run happening now** rather than one that was missed, so one comes off
    // the end.
    //
    // It used to stop strictly before `now` and subtract nothing, which was
    // right only while `Due::Now` meant the clock read the grid moment to the
    // second. It does not any more: a moment that went by is taken now, so the
    // moment being served is itself strictly before `now` and was counted as
    // skipped. A machine woken at 09:41 on a `--at 09:00` schedule ran once and
    // announced one missed run, which is the run it was doing.
    //
    // Subtracting at the end rather than special-casing covers both ways in: on
    // a clock that does read exactly `:00`, `now` is the last moment walked and
    // comes off just the same.
    //
    // `next > at` is what makes the walk finish. Without an `every` the floor
    // used to be `now` itself, so a moment already on the grid answered with
    // itself, the cursor never moved and the count ran to `MAX_MISSED_COUNTED`:
    // `cron "0,20,40 * * * *"` reported 1000 missed runs where one was missed.
    // The floor now includes `MIN_GAP_SECS`, so the cursor always advances —
    // this keeps the loop's termination a property of the loop rather than of a
    // constant somewhere else.
    let mut counted = 0;
    let mut at = last;
    while counted <= MAX_MISSED_COUNTED {
        match next_after(schedule, Some(at), at, zone) {
            Some(next) if next <= now && next > at => {
                counted += 1;
                at = next;
            }
            _ => break,
        }
    }
    counted.saturating_sub(1)
}

/// The largest number of skipped runs worth counting exactly.
const MAX_MISSED_COUNTED: u32 = 1_000;

/// The moment to actually wake at, once jitter is applied.
///
/// `roll` is a number in `[0, 1)` the caller supplies, so this stays pure and a
/// test can pin it. **Forward only**: moving a run earlier could put it before
/// the `--every` floor, which is the one thing the floor is there to prevent.
///
/// The bound is not applied here. [`wake_at`] is what the loop calls, because
/// deciding how much room a moment has needs the zone, and this deliberately has
/// no arithmetic in it beyond the multiplication.
pub fn with_jitter(due_at: i64, jitter: Duration, roll: f64) -> i64 {
    let spread = jitter.as_secs() as f64 * roll.clamp(0.0, 1.0);
    due_at.saturating_add(spread as i64)
}

/// The first instant of the local day after `at`.
///
/// `None` when the zone skips its own midnight, which some have done: there is
/// no such instant, and the calendar's own search is then the only bound left.
fn next_local_midnight<Tz: TimeZone>(at: i64, zone: &Tz) -> Option<i64> {
    let local = zone.timestamp_opt(at, 0).single()?;
    let tomorrow = local.date_naive().succ_opt()?.and_hms_opt(0, 0, 0)?;
    zone.from_local_datetime(&tomorrow)
        .earliest()
        .map(|midnight| midnight.timestamp())
}

/// The moment to wake at for a run due at `due_at`. What the loop calls.
///
/// [`with_jitter`] with the roll capped to what this moment really has room
/// for — see [`Schedule::room_at`]. The two are apart because the cap needs a
/// zone, and keeping the roll on this side is what lets the bound be tested
/// against a zone with a transition in it rather than against a `FixedOffset`,
/// which has none.
///
/// What it fixes happens once a year, and both halves were driven.
/// `room_for_jitter` measures the day in seconds-of-day, so on a day a zone
/// springs forward through it allows an hour more than the day holds:
/// `--at 09:00 --jitter 24h` allowed 85500 against a real gap of 82800, and a
/// roll near the top woke the run **past** the next day's own moment — which
/// `moment_served` then snapped back onto, so two days ran once between them and
/// `missed` counted none. `--on sun --at 01:00 --jitter 23h` allowed 82800
/// against 79200 of real room and landed on a Monday, a day that calendar
/// forbids. It takes an explicitly configured jitter within an hour of the
/// ceiling; the default is 900 seconds and a breach needs
/// `s > nominal_gap - 4500`.
///
/// **It does not close the whole class, and the neighbor has no jitter in it at
/// all.** `MIN_GAP_SECS` is real seconds while the grid is local, so on the
/// short day `--at 01:50,03:00` has 600 real seconds between its two moments
/// where `tightest_gap` declared 4200 and `validated` accepted it: the run at
/// 01:50 puts the floor at a local 03:05, past 03:00, and the second moment is
/// dropped rather than run. The next answer is the following day's 01:50. That
/// is the floor doing exactly what the floor is for, and nothing here can help
/// with it.
pub fn wake_at<Tz: TimeZone>(schedule: &Schedule, due_at: i64, roll: f64, zone: &Tz) -> i64 {
    with_jitter(
        due_at,
        schedule.jitter().min(schedule.room_at(due_at, zone)),
        roll,
    )
}

/// A day of the week, as `--on mon,thu` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weekday {
    Sun,
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
}

impl Weekday {
    /// Sunday is 0, matching cron and `chrono`'s `num_days_from_sunday`.
    fn number(self) -> u32 {
        match self {
            Self::Sun => 0,
            Self::Mon => 1,
            Self::Tue => 2,
            Self::Wed => 3,
            Self::Thu => 4,
            Self::Fri => 5,
            Self::Sat => 6,
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "sun" | "sunday" | "0" | "7" => Self::Sun,
            "mon" | "monday" | "1" => Self::Mon,
            "tue" | "tues" | "tuesday" | "2" => Self::Tue,
            "wed" | "weds" | "wednesday" | "3" => Self::Wed,
            "thu" | "thur" | "thurs" | "thursday" | "4" => Self::Thu,
            "fri" | "friday" | "5" => Self::Fri,
            "sat" | "saturday" | "6" => Self::Sat,
            _ => return None,
        })
    }
}

/// Parses `HH:MM`.
pub fn parse_time(text: &str) -> Result<(u32, u32), ScheduleError> {
    let text = text.trim();
    let (hour, minute) = text.split_once(':').ok_or_else(|| {
        ScheduleError::Unreadable(format!("\"{text}\" is not a time (try 09:00)"))
    })?;

    let bad = || ScheduleError::Unreadable(format!("\"{text}\" is not a time (try 09:00)"));
    let hour: u32 = hour.trim().parse().map_err(|_| bad())?;
    let minute: u32 = minute.trim().parse().map_err(|_| bad())?;
    if hour > 23 || minute > 59 {
        return Err(bad());
    }
    Ok((hour, minute))
}

/// Parses a five-field cron expression into the same calendar `--on`/`--at`
/// produces.
fn parse_cron(expression: &str) -> Result<Calendar, ScheduleError> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(ScheduleError::Unreadable(format!(
            "a cron expression has five fields (minute hour day month weekday), \
             and \"{expression}\" has {}",
            fields.len()
        )));
    }

    Ok(Calendar {
        // Crossed, not paired: for cron that is the correct reading and the one
        // anybody writing an expression expects.
        times: Times::Crossed {
            minutes: cron_field(fields[0], 0..=59, "minute")?,
            hours: cron_field(fields[1], 0..=23, "hour")?,
        },
        days_of_month: cron_field(fields[2], 1..=31, "day of month")?,
        months: cron_field(fields[3], 1..=12, "month")?,
        // Seven and zero are both Sunday, which is what every cron accepts.
        days_of_week: cron_field(fields[4], 0..=7, "day of week").map(fold_sunday)?,
    })
}

/// Seven means Sunday, and Sunday is zero everywhere else here.
fn fold_sunday(set: FieldSet) -> FieldSet {
    if set.contains(7) {
        FieldSet((set.0 & !(1 << 7)) | 1)
    } else {
        set
    }
}

/// One cron field: `*`, `a`, `a-b`, any of those with `/step`, or a
/// comma-separated list of them.
fn cron_field(
    text: &str,
    range: std::ops::RangeInclusive<u32>,
    name: &str,
) -> Result<FieldSet, ScheduleError> {
    let mut bits = 0u64;
    for part in text.split(',') {
        let (spec, step) = match part.split_once('/') {
            Some((spec, step)) => (
                spec,
                step.parse::<u32>()
                    .ok()
                    .filter(|&s| s > 0)
                    .ok_or_else(|| unreadable_field(part, name))?,
            ),
            None => (part, 1),
        };

        let (from, to) = if spec == "*" {
            (*range.start(), *range.end())
        } else if let Some((from, to)) = spec.split_once('-') {
            (number(from, &range, name)?, number(to, &range, name)?)
        } else {
            let value = number(spec, &range, name)?;
            // `5/15` means "from 5 onwards, every 15", which is what cron does
            // with a bare number and a step.
            if step > 1 {
                (value, *range.end())
            } else {
                (value, value)
            }
        };

        if from > to {
            return Err(unreadable_field(part, name));
        }
        for value in (from..=to).step_by(step as usize) {
            bits |= 1 << value;
        }
    }

    if bits == 0 {
        return Err(unreadable_field(text, name));
    }
    Ok(FieldSet(bits))
}

fn number(
    text: &str,
    range: &std::ops::RangeInclusive<u32>,
    name: &str,
) -> Result<u32, ScheduleError> {
    let value: u32 = text
        .trim()
        .parse()
        .map_err(|_| unreadable_field(text, name))?;
    if !range.contains(&value) {
        return Err(ScheduleError::Unreadable(format!(
            "{value} is not a {name} (it has to be between {} and {})",
            range.start(),
            range.end()
        )));
    }
    Ok(value)
}

fn unreadable_field(text: &str, name: &str) -> ScheduleError {
    ScheduleError::Unreadable(format!(
        "\"{text}\" is not a {name} a cron expression can have"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate, NaiveDateTime, Utc};

    /// A zone whose clocks go back an hour, so one hour of the wall clock
    /// happens twice.
    ///
    /// Written here rather than depended on. `chrono-tz` carries the whole IANA
    /// database to provide a transition, and this project takes a dependency for
    /// what it uses; the two rules under test are "an hour that repeats is not
    /// two runs" and "one that never happened is not a run at all", and both
    /// need a transition rather than a real city.
    ///
    /// `FixedOffset` cannot stand in for it — it has no transitions at all,
    /// which is exactly why the test that credited `MIN_GAP_SECS` with stopping
    /// the double run could never have caught it doing nothing.
    ///
    /// Four hours behind UTC until [`FALL_BACK_AT`], five hours behind from then
    /// on: local 02:00 becomes local 01:00, and 01:00 through 01:59 come round
    /// twice.
    #[derive(Clone, Copy, Debug)]
    struct FallsBack;

    /// Monday 06:00 UTC, which is 02:00 before the change and 01:00 after it.
    const FALL_BACK_AT: i64 = MONDAY_0000 + 6 * 3_600;

    const BEFORE: i32 = -4 * 3_600;
    const AFTER: i32 = -5 * 3_600;

    fn east(seconds: i32) -> FixedOffset {
        FixedOffset::east_opt(seconds).expect("a whole number of hours is a valid offset")
    }

    impl TimeZone for FallsBack {
        type Offset = FixedOffset;

        fn from_offset(_: &FixedOffset) -> Self {
            FallsBack
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<FixedOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<FixedOffset> {
            // What instant each of the two offsets would put this wall-clock
            // reading at, and whether that instant is on the side of the change
            // where the offset actually applies.
            let wall = local.and_utc().timestamp();
            let as_before = wall - i64::from(BEFORE) < FALL_BACK_AT;
            let as_after = wall - i64::from(AFTER) >= FALL_BACK_AT;

            match (as_before, as_after) {
                // The repeated hour: earliest first, which is chrono's order.
                (true, true) => MappedLocalTime::Ambiguous(east(BEFORE), east(AFTER)),
                (true, false) => MappedLocalTime::Single(east(BEFORE)),
                (false, true) => MappedLocalTime::Single(east(AFTER)),
                // An hour the wall clock skipped. This zone has no
                // spring-forward, so it is unreachable here.
                (false, false) => MappedLocalTime::None,
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            if utc.and_utc().timestamp() < FALL_BACK_AT {
                east(BEFORE)
            } else {
                east(AFTER)
            }
        }
    }

    /// A zone whose clocks go forward an hour, so one hour of the wall clock
    /// never happens.
    ///
    /// The sibling of [`FallsBack`], and it exists for the other direction: a
    /// day this zone springs forward through is 82800 seconds long, and every
    /// bound on the jitter is written in seconds-of-day. `FixedOffset` cannot
    /// stand in for it, for the same reason it could not stand in there.
    ///
    /// Five hours behind UTC until [`SPRING_FORWARD_AT`], four hours behind from
    /// then on: local 02:00 becomes local 03:00, and 02:00 through 02:59 never
    /// come round at all.
    #[derive(Clone, Copy, Debug)]
    struct SpringsForward;

    /// Monday 07:00 UTC, which is 02:00 before the change and 03:00 after it.
    const SPRING_FORWARD_AT: i64 = MONDAY_0000 + 7 * 3_600;

    const WINTER: i32 = -5 * 3_600;
    const SUMMER: i32 = -4 * 3_600;

    impl TimeZone for SpringsForward {
        type Offset = FixedOffset;

        fn from_offset(_: &FixedOffset) -> Self {
            SpringsForward
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<FixedOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<FixedOffset> {
            let wall = local.and_utc().timestamp();
            let as_winter = wall - i64::from(WINTER) < SPRING_FORWARD_AT;
            let as_summer = wall - i64::from(SUMMER) >= SPRING_FORWARD_AT;

            match (as_winter, as_summer) {
                // The hour the wall clock skipped: no instant reads it.
                (false, false) => MappedLocalTime::None,
                (true, false) => MappedLocalTime::Single(east(WINTER)),
                (false, true) => MappedLocalTime::Single(east(SUMMER)),
                // This zone has no fall-back, so nothing here is ambiguous.
                (true, true) => MappedLocalTime::Ambiguous(east(WINTER), east(SUMMER)),
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            if utc.and_utc().timestamp() < SPRING_FORWARD_AT {
                east(WINTER)
            } else {
                east(SUMMER)
            }
        }
    }

    /// Midnight UTC on Monday 2026-08-17. Every timestamp below is an offset
    /// from it, so the weekday arithmetic is checkable by hand.
    const MONDAY_0000: i64 = 1_786_924_800;

    /// The constant has to be the day it claims to be. It was three tests'
    /// worth of confusing failures when it was a Friday.
    #[test]
    fn the_fixture_really_is_a_monday() {
        let day = Utc.timestamp_opt(MONDAY_0000, 0).unwrap();
        assert_eq!(day.weekday(), chrono::Weekday::Mon);
        assert_eq!((day.hour(), day.minute()), (0, 0));
    }

    fn at(offset_secs: i64) -> i64 {
        MONDAY_0000 + offset_secs
    }

    fn hours(n: i64) -> i64 {
        n * 3_600
    }

    #[test]
    fn an_interval_is_measured_from_the_previous_run() {
        let schedule = Schedule::every(Duration::from_secs(hours(6) as u64)).unwrap();
        // Started at 09:13, so the next one is 15:13 rather than 12:00.
        let last = at(hours(9) + 13 * 60);
        assert_eq!(
            next_after(&schedule, Some(last), last + 60, &Utc),
            Some(last + hours(6))
        );
    }

    /// There is no past to wait from, so the first run is now.
    #[test]
    fn an_interval_with_nothing_behind_it_is_due_at_once() {
        let schedule = Schedule::every(Duration::from_secs(hours(6) as u64)).unwrap();
        assert_eq!(due(&schedule, None, at(0), &Utc), Due::Now { missed: 0 });
    }

    /// The floor holds against the **moments**, not against the rows, and what
    /// it names is one the calendar allows.
    ///
    /// `validated()` refuses a schedule whose grid is tighter than the floor,
    /// but jitter moves each run off that grid and the next moment used to be
    /// computed from where the run landed: `*/15` — the tightest schedule the
    /// tool advertises as legal — put half of its consecutive pairs under the
    /// floor, worst case 36 seconds apart, and the check that should have caught
    /// that answered `last + MIN_GAP_SECS`, an instant no `*/15` expression
    /// names. Folding the floor into the start of the search fixed both.
    ///
    /// It then produced the opposite failure, which is what the sweep below is
    /// really for. `watch_runs.started_at` is stamped after both lists are
    /// walked, so it trails the moment by the whole length of the walk; measured
    /// from there the floor lands past the next grid moment and that moment is
    /// dropped. `*/15` ran every thirty minutes for any walk at all, on an
    /// expression the tool had accepted.
    ///
    /// So the floor counts from the moment served. **That is a deliberate
    /// softening**: two runs can be closer together in wall clock than
    /// `MIN_GAP_SECS` by however long the walk took, which the second half of
    /// this test pins at its extreme. They still cannot overlap, and what the
    /// floor is about — that a second run cannot see a state the first did not —
    /// is a statement about the moments.
    #[test]
    fn a_run_is_never_due_inside_the_floor_nor_off_the_calendar() {
        let schedule = Schedule::cron("*/15 * * * *").unwrap();
        let start = at(0);
        let grid = 15 * 60;

        // A run that served `start` and wrote its row seven seconds later.
        // Whatever the current moment is before the next one comes round, `due`
        // names that next moment: on the quarter hour, and a full grid step from
        // the one that was served.
        let last = start + 7;
        for ahead in 0..(grid - 7) {
            match due(&schedule, Some(last), last + ahead, &Utc) {
                Due::At(next) => {
                    assert_eq!(
                        next - start,
                        grid,
                        "the moment served was {start}; the next one is a grid step away"
                    );
                    assert_eq!(
                        (next - start) % grid,
                        0,
                        "{next} is not a moment \"*/15\" names"
                    );
                }
                other => panic!("declared due {ahead}s after the last run: {other:?}"),
            }
        }

        // And the moment itself is taken when it arrives, rather than pushed
        // past by the seven seconds the row trails.
        assert!(matches!(
            due(&schedule, Some(last), start + grid, &Utc),
            Due::Now { .. }
        ));

        // The extreme of the softening, stated rather than left to be found: a
        // walk that took almost the whole gap leaves the next run one second
        // after the previous row. That is what a schedule asking for a run every
        // fifteen minutes, on an account that takes fifteen minutes to walk,
        // amounts to — and what bounds the requests there is the pacer's budget,
        // not this.
        let slow = start + grid - 1;
        assert!(matches!(
            due(&schedule, Some(slow), start + grid, &Utc),
            Due::Now { .. }
        ));
    }

    /// A tight grid keeps its cadence however long the walk takes.
    ///
    /// `watch_runs.started_at` is stamped by `tick` *after* both lists have been
    /// walked, so the row trails the moment by the whole length of the walk.
    /// With the floor measured from the row, that pushed it past the next grid
    /// moment and the moment was dropped: `*/15` ran every thirty minutes for
    /// any walk at all, `0,20,40` for a walk over five minutes — on expressions
    /// the tool had accepted and printed back.
    ///
    /// Driven as the loop drives it: serve a moment, walk for `W`, record, ask
    /// again. What is asserted is the moments, which is what the schedule names.
    #[test]
    fn a_tight_grid_keeps_its_cadence_however_long_the_walk_takes() {
        for (expression, grid) in [("*/15 * * * *", 15 * 60), ("0,20,40 * * * *", 20 * 60)] {
            let schedule = Schedule::cron(expression).unwrap();

            // A walk that takes most of the gap, which is the worst case that
            // still leaves the schedule meaningful.
            for walk in [1, 60, grid / 2] {
                let start = at(hours(9));
                let mut served = start;

                for step in 1..=4 {
                    // The row is written when the walk finishes.
                    let row = served + walk;
                    let expected = start + step * grid;

                    match due(&schedule, Some(row), expected, &Utc) {
                        Due::Now { .. } => {}
                        other => {
                            panic!("{expression} with a {walk}s walk skipped {expected}: {other:?}")
                        }
                    }
                    served = expected;
                }
            }
        }
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

    /// Jitter cannot swallow the next moment on a day that is an hour short.
    ///
    /// `room_for_jitter` measures the day in seconds-of-day: `A_DAY` is 86400,
    /// so `--at 01:00` gets its whole gap whatever the calendar does that night.
    /// On a spring-forward day the real gap between one moment and the next is
    /// 82800, and a roll near the top woke the run *past* the following day's
    /// own moment — which `moment_served` then snapped back onto, so the two
    /// days ran once between them and nothing counted a missed run.
    #[test]
    fn jitter_cannot_reach_past_the_next_moment_on_a_short_day() {
        // The fixture has to be the thing it claims: an hour that never happens.
        let skipped = NaiveDate::from_ymd_opt(2026, 8, 17)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        assert!(
            matches!(
                SpringsForward.from_local_datetime(&skipped),
                MappedLocalTime::None
            ),
            "the zone has to actually skip that hour"
        );

        let schedule = Schedule::calendar(&[], &[(1, 0)])
            .unwrap()
            .with_jitter(Duration::from_secs(24 * 3_600));

        // Monday 01:00 local, and Tuesday 01:00 local, which is 23 hours later.
        let due_at = at(hours(6));
        let next = at(hours(29));
        assert_eq!(
            next - due_at,
            hours(23),
            "the fixture depends on the day being short"
        );

        let woken = wake_at(&schedule, due_at, 0.999, &SpringsForward);
        assert!(
            woken + MIN_GAP_SECS <= next,
            "woke at {woken}, which leaves no room before the next moment at {next}"
        );
    }

    /// And it cannot spill onto a forbidden day when the day is an hour short.
    ///
    /// `room_before_midnight` is `86400 - latest`, so `--on mon --at 01:00`
    /// keeps 82800 seconds of room. On the short day there are only 79200
    /// between 01:00 and midnight, and a roll of 0.96 landed the run on the
    /// Tuesday — the failure `with_jitter`'s own doc says the bound exists to
    /// stop, arriving through the hour the arithmetic does not know about.
    #[test]
    fn jitter_cannot_spill_onto_the_next_day_when_the_day_is_short() {
        let schedule = Schedule::calendar(&[Weekday::Mon], &[(1, 0)])
            .unwrap()
            .with_jitter(Duration::from_secs(23 * 3_600));

        let due_at = at(hours(6));
        let midnight = at(hours(28));
        assert_eq!(
            midnight - due_at,
            hours(22),
            "01:00 to midnight is 22 real hours on this day, not 23"
        );

        let woken = wake_at(&schedule, due_at, 0.96, &SpringsForward);
        let landed = SpringsForward.timestamp_opt(woken, 0).single().unwrap();
        assert_eq!(
            landed.weekday(),
            chrono::Weekday::Mon,
            "due Monday at one, woke {landed} — a day this schedule does not name"
        );
    }

    /// Every moment a tight grid names is actually run, worst-case roll and all.
    ///
    /// The default for a calendar was a flat fifteen minutes, which is exactly
    /// `MIN_GAP_SECS`, and nothing took the floor off the gap. So on
    /// `*/15` — the tightest expression this tool advertises as legal — a run
    /// pushed `s` seconds later put the next grid moment inside its own floor
    /// for every `s` above zero: it ran every half hour, 899 times out of 900,
    /// while the banner printed what the user typed.
    #[test]
    fn a_tight_grid_hits_every_moment_it_names_under_the_worst_roll() {
        for (expression, grid) in [("*/15 * * * *", 15 * 60), ("0,20,40 * * * *", 20 * 60)] {
            let schedule = Schedule::cron(expression).unwrap();
            let start = at(hours(9));

            // The worst case: pushed by the whole jitter, every time. Through
            // `wake_at`, which is what the loop calls — `with_jitter` alone is
            // half the bound, and this test is here to walk the real path.
            let mut last = wake_at(&schedule, start, 0.999_999, &Utc);
            for step in 1..=4 {
                let expected = start + step * grid;
                match due(&schedule, Some(last), expected, &Utc) {
                    Due::Now { .. } => {}
                    other => panic!(
                        "{expression} should be due at its own moment {expected} \
                         after a run at {last}: {other:?}"
                    ),
                }
                last = wake_at(&schedule, expected, 0.999_999, &Utc);
            }
        }
    }

    /// The hour a fall-back repeats does not turn one daily run into two.
    ///
    /// `--at 01:30` in a zone that puts its clocks back names two instants an
    /// hour apart on that date, and both satisfy the calendar. `MIN_GAP_SECS` is
    /// fifteen minutes and they are sixty apart, so nothing separated them: the
    /// monitor walked both lists again and posted a second webhook for a
    /// schedule that names one run a day.
    ///
    /// The comment at the search credited `single()` with refusing this and the
    /// rationale on the neighboring test credited `MIN_GAP_SECS`. Neither was
    /// doing anything, and that test used a `FixedOffset`, which has no
    /// transitions to be ambiguous about.
    #[test]
    fn the_repeated_hour_of_a_fall_back_does_not_run_twice() {
        let schedule = Schedule::cron("30 1 * * *").unwrap();

        // The same wall-clock 01:30, an hour apart in real time.
        let first = at(hours(5) + 1_800);
        let second = at(hours(6) + 1_800);
        assert_eq!(second - first, 3_600, "an hour apart, and the floor is 15m");

        // The fixture has to be the thing it claims: two instants for one
        // reading, or the test proves nothing about the guard.
        let reading = DateTime::from_timestamp(first, 0)
            .unwrap()
            .with_timezone(&FallsBack)
            .naive_local();
        assert!(
            matches!(
                FallsBack.from_local_datetime(&reading),
                MappedLocalTime::Ambiguous(..)
            ),
            "the zone must actually repeat that hour"
        );

        // Having run at the first showing, the second is not another run: the
        // next one is tomorrow.
        assert_eq!(
            due(&schedule, Some(first), second, &FallsBack),
            Due::At(at(hours(30) + 1_800)),
            "one run a day means one run a day, including the day with 25 hours"
        );

        // And the other half: a machine that was off through the first showing
        // still runs at the second. The rule is "not twice", not "not at all".
        assert!(
            matches!(
                due(&schedule, Some(first - 24 * 3_600), second, &FallsBack),
                Due::Now { .. }
            ),
            "a run that has not happened is still owed"
        );
    }

    /// The midnight wrap is only a gap when there is a next day.
    ///
    /// It was counted unconditionally, so `--on mon --at 00:00,23:55` was
    /// refused as "5m is too often" about two runs 23h55m apart: with only
    /// Monday allowed, 23:55 is followed by the next Monday's midnight.
    #[test]
    fn the_wrap_is_not_a_gap_when_the_days_are_not_consecutive() {
        assert!(
            Schedule::calendar(&[Weekday::Mon], &[(0, 0), (23, 55)]).is_ok(),
            "a whole week apart is not too often"
        );
        assert!(
            Schedule::cron("0,55 0,23 * * 1").is_ok(),
            "the same schedule written as cron"
        );

        // Consecutive days, and it is a gap again: Monday 23:55 to Tuesday
        // 00:00 is five minutes.
        assert!(matches!(
            Schedule::calendar(&[Weekday::Mon, Weekday::Tue], &[(0, 0), (23, 55)]),
            Err(ScheduleError::TooOften { .. })
        ));
        assert!(matches!(
            Schedule::calendar(&[], &[(0, 0), (23, 55)]),
            Err(ScheduleError::TooOften { .. })
        ));
    }

    /// The count is against what actually decides the schedule, not against the
    /// interval alone. A week down on `--every 1h --on mon --at 09:00` is one
    /// missed Monday; dividing the elapsed time by the hour said 167, and that
    /// number is printed at the user.
    #[test]
    fn a_calendar_counts_the_runs_it_allows_and_not_the_hours() {
        let schedule = Schedule::calendar(&[Weekday::Mon], &[(9, 0)])
            .unwrap()
            .and_every(Duration::from_secs(hours(1) as u64))
            .unwrap();

        // Last ran one Monday at nine; it is now the Monday a fortnight later.
        let last = at(hours(9));
        match due(&schedule, Some(last), last + 14 * hours(24), &Utc) {
            Due::Now { missed } => assert_eq!(missed, 1, "one Monday went by unrun"),
            other => panic!("should be due: {other:?}"),
        }
    }

    /// A calendar moment that went by is due now, whatever second it is.
    ///
    /// The search only ever looked forward and works in whole minutes, so the
    /// answer was always the *ceiling* minute of `now` — never `now` itself
    /// unless the clock read exactly `:00`. `store::now()` gives an arbitrary
    /// second, so `Due::Now` for a calendar came up about once in nine hundred,
    /// and a machine powered on at 09:05 with `--at 09:00` waited until the next
    /// day. Every calendar test that existed happened to use an aligned
    /// timestamp, which is the whole reason the suite was green over it.
    #[test]
    fn a_calendar_moment_that_went_by_is_due_at_any_second_of_the_clock() {
        let schedule = Schedule::cron("0 9 * * *").unwrap();
        // Ran yesterday at nine.
        let last = at(hours(9) - hours(24));

        // Powered on at 09:05:37: nine o'clock has gone by unrun.
        let now = at(hours(9) + 5 * 60 + 37);
        assert!(
            matches!(due(&schedule, Some(last), now, &Utc), Due::Now { .. }),
            "a run owed since 09:00 is still owed at 09:05:37"
        );

        // And 08:55:37 is before it, so it is not owed yet — the backward look
        // must not reach past the floor and find yesterday's moment again.
        let before = at(hours(8) + 55 * 60 + 37);
        assert_eq!(
            due(&schedule, Some(last), before, &Utc),
            Due::At(at(hours(9))),
            "the moment has not come yet"
        );
    }

    /// A first run does not depend on which second of the minute it started in.
    ///
    /// With nothing ever run the floor is `now`, and the window used to start
    /// there — so it was a whole minute only when the clock read `:00` exactly.
    /// The same `snob watch --on mon --at 09:00` started at 09:00:00 ran, and
    /// started at 09:00:20 waited a week. That is the `:00`-to-the-second
    /// discontinuity taken out of the `last_run` path, left behind where a fresh
    /// install passes `None`.
    #[test]
    fn a_first_run_does_not_depend_on_the_second_it_started_in() {
        let schedule = Schedule::calendar(&[Weekday::Mon], &[(9, 0)]).unwrap();

        for second in [0, 20, 59] {
            assert!(
                matches!(
                    due(&schedule, None, at(hours(9) + second), &Utc),
                    Due::Now { .. }
                ),
                "started {second}s into the minute it names, and it is that minute"
            );
        }

        // And the minute after it is not that minute.
        assert_eq!(
            due(&schedule, None, at(hours(9) + 60), &Utc),
            Due::At(at(hours(9) + 7 * 24 * 3_600)),
            "09:01 on a Monday is not 09:00 on a Monday"
        );
    }

    /// A moment that went by between the last run and the floor is owed, not
    /// dropped.
    ///
    /// The look back was bounded below by the floor, so a moment in that gap was
    /// beneath the window and above nothing — the forward search starts at the
    /// floor too. It is bounded by the last run now, and still guarded by
    /// `floor <= now`, so what it produces is a run at `now`, past the floor:
    /// served late rather than lost.
    #[test]
    fn a_moment_between_the_last_run_and_the_floor_is_served_late() {
        // One moment a day, so nothing later can stand in for it.
        let schedule = Schedule::cron("15 9 * * *").unwrap();

        // Ran at 09:00:07 — seven seconds past a moment of its own — which puts
        // the floor at 09:15:07, seven seconds past the next one.
        let last = at(hours(9) + 7);
        let now = at(hours(9) + 20 * 60);

        match due(&schedule, Some(last), now, &Utc) {
            Due::Now { .. } => {}
            other => panic!("09:15 went by unrun and is owed: {other:?}"),
        }
    }

    /// The same, for the shape a laptop is actually in: suspended across the
    /// moment, woken minutes later, with `--on` and `--at` rather than cron.
    #[test]
    fn a_moment_slept_through_is_taken_on_waking() {
        let schedule = Schedule::calendar(&[Weekday::Mon], &[(9, 0)]).unwrap();
        let last = at(hours(9) - 7 * hours(24));

        match due(&schedule, Some(last), at(hours(9) + 41 * 60 + 3), &Utc) {
            Due::Now { missed } => assert_eq!(
                missed, 0,
                "the one due now is not itself a missed run: {missed}"
            ),
            other => panic!("a Monday nine o'clock slept through is owed: {other:?}"),
        }
    }

    /// A calendar with no interval on it counts, rather than answering "1000".
    ///
    /// Without an `every` the floor used to be `now` itself, so asking for the
    /// next moment after a moment already on the grid answered with that same
    /// moment: the cursor never advanced and the count ran to
    /// `MAX_MISSED_COUNTED`. The number is printed at the user, so a wrong one
    /// is a sentence that is simply false.
    #[test]
    fn a_calendar_with_no_interval_counts_the_runs_it_missed() {
        let schedule = Schedule::cron("0 * * * *").unwrap();
        let last = at(hours(10));

        match due(&schedule, Some(last), at(hours(13)), &Utc) {
            Due::Now { missed } => assert_eq!(missed, 2, "eleven and twelve o'clock went by"),
            other => panic!("should be due: {other:?}"),
        }
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

    /// The rule that keeps an outage from becoming a burst.
    #[test]
    fn a_process_that_was_down_for_days_runs_once_and_says_how_many_it_missed() {
        let schedule = Schedule::every(Duration::from_secs(hours(6) as u64)).unwrap();
        let last = at(0);
        // Three days later: twelve intervals have gone by.
        match due(&schedule, Some(last), last + hours(72), &Utc) {
            Due::Now { missed } => assert_eq!(missed, 11, "eleven skipped, and this one now"),
            other => panic!("should be due: {other:?}"),
        }
    }

    #[test]
    fn a_calendar_fires_on_the_named_day_at_the_named_time() {
        let schedule = Schedule::calendar(&[Weekday::Mon, Weekday::Thu], &[(9, 0)]).unwrap();
        // Sunday evening: the next one is Monday at nine.
        let sunday = at(-hours(4));
        assert_eq!(
            next_after(&schedule, None, sunday, &Utc),
            Some(at(hours(9)))
        );
    }

    /// Thursday is three days after Monday, and the search has to walk there
    /// rather than stopping at the first day it does not match.
    #[test]
    fn a_calendar_skips_the_days_it_was_not_given() {
        let schedule = Schedule::calendar(&[Weekday::Thu], &[(9, 0)]).unwrap();
        assert_eq!(
            next_after(&schedule, None, at(hours(10)), &Utc),
            Some(at(3 * hours(24) + hours(9)))
        );
    }

    /// The conjunction. "Every two weeks on a Monday" is the interval cron
    /// cannot express, and it is the one people ask for.
    #[test]
    fn an_interval_and_a_calendar_both_have_to_be_satisfied() {
        let schedule = Schedule::calendar(&[Weekday::Mon], &[(9, 0)])
            .unwrap()
            .and_every(Duration::from_secs(14 * 24 * 3_600))
            .unwrap();

        let last = at(hours(9));
        let next = next_after(&schedule, Some(last), last + 60, &Utc).unwrap();

        assert_eq!(
            next,
            last + 14 * hours(24),
            "the Monday a fortnight later, not the one a week later"
        );
    }

    /// The test that holds "one evaluator, two syntaxes" together. If these
    /// ever disagree, the same schedule means two things.
    ///
    /// Compared by the moments they fire at rather than by their fields. The
    /// two representations are no longer identical and should not be: a
    /// calendar built from `--at` carries the exact times, because `09:00,21:30`
    /// means two moments while cron's `0,30 9,21` means four. What has to match
    /// is the answer, and that is what this asks for.
    #[test]
    fn the_dsl_and_cron_agree_about_monday_at_nine() {
        let dsl = Schedule::calendar(&[Weekday::Mon], &[(9, 0)]).unwrap();
        let cron = Schedule::cron("0 9 * * 1").unwrap();

        let mut at = at(-hours(30));
        for _ in 0..5 {
            let from_dsl = next_after(&dsl, None, at, &Utc);
            assert_eq!(from_dsl, next_after(&cron, None, at, &Utc));
            at = from_dsl.expect("Monday comes round") + 60;
        }
    }

    /// `--at 09:00,21:30` is two moments a day, not the four that crossing the
    /// hour and minute fields gives. It used to be four, which quietly doubled
    /// what anybody who named two times was spending.
    #[test]
    fn two_times_of_day_are_two_moments_and_not_their_product() {
        let schedule = Schedule::calendar(&[], &[(9, 0), (21, 30)]).unwrap();

        let mut fired = Vec::new();
        let mut at = at(0);
        for _ in 0..4 {
            let next = next_after(&schedule, None, at, &Utc).unwrap();
            let local = Utc.timestamp_opt(next, 0).unwrap();
            fired.push(format!("{:02}:{:02}", local.hour(), local.minute()));
            at = next + 60;
        }

        assert_eq!(fired, vec!["09:00", "21:30", "09:00", "21:30"]);
    }

    /// And cron keeps cron's reading, which is the product. `0,30 9,21 * * *`
    /// really is four times a day, and somebody writing that expression means
    /// exactly that.
    #[test]
    fn a_cron_expression_still_crosses_its_hour_and_minute_fields() {
        let schedule = Schedule::cron("0,30 9,21 * * *").unwrap();

        let mut fired = Vec::new();
        let mut at = at(0);
        for _ in 0..4 {
            let next = next_after(&schedule, None, at, &Utc).unwrap();
            let local = Utc.timestamp_opt(next, 0).unwrap();
            fired.push(format!("{:02}:{:02}", local.hour(), local.minute()));
            at = next + 60;
        }

        assert_eq!(fired, vec!["09:00", "09:30", "21:00", "21:30"]);
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

    /// The gap across midnight is a gap. `23:55` and `00:00` are five minutes
    /// apart, and measuring only the forward differences within a day would
    /// call them nearly twenty-four hours and let them through.
    #[test]
    fn the_wrap_around_midnight_counts_as_a_gap() {
        assert!(Schedule::calendar(&[], &[(23, 55), (0, 0)]).is_err());
    }

    #[test]
    fn cron_accepts_the_shapes_a_cron_expression_has() {
        // Every six hours on the hour.
        let every_six = Schedule::cron("0 */6 * * *").unwrap();
        assert_eq!(
            next_after(&every_six, None, at(hours(1)), &Utc),
            Some(at(hours(6)))
        );

        // A list of days.
        let mon_thu = Schedule::cron("0 9 * * 1,4").unwrap();
        assert_eq!(
            next_after(&mon_thu, None, at(hours(10)), &Utc),
            Some(at(3 * hours(24) + hours(9)))
        );
    }

    /// Seven and zero are both Sunday. Folded, so `* * * * 7` and `* * * * 0`
    /// cannot mean different days.
    #[test]
    fn cron_reads_seven_as_sunday() {
        assert_eq!(
            Schedule::cron("0 9 * * 0").unwrap().calendar,
            Schedule::cron("0 9 * * 7").unwrap().calendar
        );
    }

    /// The rule every reimplementation gets wrong. `0 9 13 * 5` is "the 13th,
    /// and every Friday" -- not "Friday the 13th".
    #[test]
    fn two_restricted_day_fields_combine_with_or() {
        let calendar = parse_cron("0 9 13 * 5").unwrap();
        let day = |ts: i64| calendar.allows(&Utc.timestamp_opt(ts, 0).unwrap());

        // Friday 2026-08-21 at 09:00 -- a Friday that is not the 13th.
        assert!(day(at(4 * hours(24) + hours(9))));
        // Monday 2026-08-17 at 09:00 -- neither.
        assert!(!day(at(hours(9))));
    }

    /// And when one of them is `*`, only the other decides. Read as OR, `*`
    /// would match every day and the weekday field would do nothing.
    #[test]
    fn one_unrestricted_day_field_leaves_the_other_in_charge() {
        let calendar = parse_cron("0 9 * * 1").unwrap();
        let day = |ts: i64| calendar.allows(&Utc.timestamp_opt(ts, 0).unwrap());

        assert!(day(at(hours(9))), "Monday");
        assert!(!day(at(hours(24) + hours(9))), "Tuesday");
    }

    #[test]
    fn cron_refuses_what_it_cannot_read() {
        for bad in [
            "0 9 * *",
            "0 9 * * * *",
            "60 9 * * *",
            "0 24 * * *",
            "0 9 * * 8",
            "abc 9 * * *",
            "0 9-5 * * *",
            "0 */0 * * *",
        ] {
            assert!(Schedule::cron(bad).is_err(), "\"{bad}\" should be refused");
        }
    }

    /// "It never fires" is an answer, and it must not be an instant.
    ///
    /// The sentinel was `Due::At(i64::MAX)` and the loop matched that literal in
    /// an arm above the general one. Everything downstream of `At` treats the
    /// payload as a moment -- `with_jitter` adds to it and saturates, `wake_at`
    /// hands back `i64::MAX` -- so a schedule that names nothing put the monitor
    /// to sleep a minute at a time for ever, with nothing failing and nothing to
    /// read. Two things are pinned: that the impossible calendar answers its own
    /// variant, and that nothing this module accepts ever hands the loop an
    /// instant anywhere near the end of time, which is what made the sentinel
    /// look safe.
    #[test]
    fn a_schedule_that_names_nothing_is_never_due_rather_than_due_at_the_end_of_time() {
        // A century out, which is past anything expressible here:
        // `MAX_INTERVAL_SECS` is a year and the search horizon is four years.
        let a_century = at(0) + 100 * 366 * 24 * 3_600;
        for schedule in [
            Schedule::every(Duration::from_secs(hours(6) as u64)).unwrap(),
            Schedule::every(Duration::from_secs(MAX_INTERVAL_SECS as u64)).unwrap(),
            Schedule::cron("*/15 * * * *").unwrap(),
            // A leap day: rare enough to look impossible, and it is not.
            Schedule::cron("0 9 29 2 *").unwrap(),
            Schedule::days(&[Weekday::Mon], Duration::from_secs(14 * 24 * 3_600)).unwrap(),
        ] {
            match due(&schedule, Some(at(0)), at(hours(1)), &Utc) {
                Due::At(next) => assert!(
                    next < a_century,
                    "{schedule:?} handed the loop {next} to sleep until"
                ),
                Due::Now { .. } => {}
                Due::Never => panic!("{schedule:?} names moments and one of them is next"),
            }
        }
    }

    /// An expression that matches nothing must answer rather than search
    /// forever. There is no 31st of February.
    #[test]
    fn an_impossible_calendar_answers_none_instead_of_spinning() {
        let schedule = Schedule::cron("0 0 31 2 *").unwrap();
        assert_eq!(next_after(&schedule, None, at(0), &Utc), None);
        assert_eq!(
            due(&schedule, None, at(0), &Utc),
            Due::Never,
            "there is no 31st of February, and that is not a moment to sleep until"
        );
    }

    /// The search matches against local wall-clock time, which is what decides
    /// what happens at a daylight-saving transition.
    ///
    /// A fixed offset has no transitions, so this pins the matching alone: a
    /// moment is chosen by what the wall clock reads, not by UTC.
    ///
    /// What happens at a transition is a separate question and is asked
    /// separately, by `the_repeated_hour_of_a_fall_back_does_not_run_twice`
    /// against a zone written for it. The rationale that used to be here said
    /// `single()` refused an ambiguous hour and `MIN_GAP_SECS` stopped the
    /// second run. Neither was true: `timestamp_opt` goes from an instant to a
    /// local time, a direction that is never ambiguous, and the two showings of
    /// a repeated hour are sixty minutes apart while the floor is fifteen. A
    /// fixed offset could not have shown that, which is why it went unnoticed.
    #[test]
    fn the_calendar_matches_against_local_wall_clock_time() {
        let schedule = Schedule::calendar(&[], &[(2, 30)]).unwrap();
        let zone = FixedOffset::east_opt(3_600).unwrap();
        let next = next_after(&schedule, None, at(0), &zone).unwrap();

        let local = zone.timestamp_opt(next, 0).unwrap();
        assert_eq!((local.hour(), local.minute()), (2, 30));
    }

    /// The calendar is read in the zone it is given, which is what makes "nine
    /// in the morning" mean the user's morning rather than UTC's.
    #[test]
    fn the_time_of_day_is_local() {
        let schedule = Schedule::calendar(&[], &[(9, 0)]).unwrap();
        let zone = FixedOffset::east_opt(2 * 3_600).unwrap();
        let next = next_after(&schedule, None, at(0), &zone).unwrap();

        assert_eq!(
            next,
            at(hours(7)),
            "09:00 two hours east of UTC is 07:00 UTC"
        );
    }

    #[test]
    fn jitter_only_ever_moves_a_run_later() {
        let due_at = at(hours(9));
        for roll in [0.0, 0.5, 0.99] {
            let woken = with_jitter(due_at, Duration::from_secs(900), roll);
            assert!(woken >= due_at, "jitter must not pull a run earlier");
            assert!(woken <= due_at + 900);
        }
    }

    /// A roll outside its range is clamped rather than trusted: the caller
    /// supplies it, and an out-of-range one would push a run arbitrarily far.
    #[test]
    fn a_roll_outside_its_range_cannot_push_a_run_anywhere() {
        let due_at = at(0);
        assert_eq!(
            with_jitter(due_at, Duration::from_secs(900), 5.0),
            due_at + 900
        );
        assert_eq!(with_jitter(due_at, Duration::from_secs(900), -1.0), due_at);
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

    /// A stored future is a clock that went backwards, and the monitor has to
    /// keep working rather than wait the skew out on top of the interval.
    ///
    /// The reading `rate_budget` takes for the same situation: the present wins
    /// over a stored future. With a day of skew the wait is one interval, not a
    /// day and an interval.
    #[test]
    fn a_clock_that_went_backwards_costs_at_most_one_interval() {
        let every = Duration::from_secs(hours(6) as u64);
        let schedule = Schedule::every(every).unwrap();
        let now = at(0);

        // The last run is recorded a day in the future.
        let Due::At(next) = due(&schedule, Some(now + hours(24)), now, &Utc) else {
            panic!("a run six hours out is not due yet");
        };
        assert_eq!(
            next,
            now + hours(6),
            "the skew is discarded rather than waited out"
        );
    }

    #[test]
    fn weekdays_are_read_the_ways_people_write_them() {
        assert_eq!(Weekday::parse("mon"), Some(Weekday::Mon));
        assert_eq!(Weekday::parse("Monday"), Some(Weekday::Mon));
        assert_eq!(Weekday::parse("THU"), Some(Weekday::Thu));
        assert_eq!(Weekday::parse("0"), Some(Weekday::Sun));
        assert_eq!(Weekday::parse("7"), Some(Weekday::Sun));
        assert_eq!(Weekday::parse("funday"), None);
    }

    #[test]
    fn times_are_read_and_refused_as_written() {
        assert_eq!(parse_time("09:00").unwrap(), (9, 0));
        assert_eq!(parse_time(" 21:30 ").unwrap(), (21, 30));
        for bad in ["9", "25:00", "09:60", "nine", "09:"] {
            assert!(parse_time(bad).is_err(), "\"{bad}\" should be refused");
        }
    }
}
