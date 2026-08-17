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

use chrono::{DateTime, Datelike, TimeZone, Timelike};

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
/// Not decoration. A walk that starts at exactly 09:00:00 every day is a
/// pattern, and lowering the chance of a checkpoint is what the whole pacing
/// design is for. Fifteen minutes, or a tenth of the interval when that is
/// smaller, with a floor of one minute.
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
        .validated()
    }

    /// A five-field cron expression.
    pub fn cron(expression: &str) -> Result<Self, ScheduleError> {
        Self {
            calendar: Some(parse_cron(expression)?),
            every: None,
            jitter: default_jitter(None),
        }
        .validated()
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
        self.validated()
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
        let day = Duration::from_secs(24 * 3_600);
        let from_calendar = self.calendar.as_ref().map(|c| {
            c.tightest_gap()
                .map(|gap| Duration::from_secs(gap as u64))
                // One moment a day. Whatever the day fields allow, the next
                // moment is at least a day away, so a day is the bound — and a
                // roll is in `[0, 1)`, so the run still lands strictly before it.
                .unwrap_or(day)
        });
        let ceiling = match (self.every, from_calendar) {
            (Some(every), Some(gap)) => every.min(gap),
            (Some(every), None) => every,
            (None, Some(gap)) => gap,
            // Neither half. `validated` refuses this, so it is unreachable; a
            // day is the answer that cannot be wrong by much.
            (None, None) => day,
        };
        self.jitter = jitter.min(ceiling);
        self
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Nothing yet. Sleep until this instant, an epoch in seconds.
    At(i64),
    /// Run now. `missed` counts the scheduled runs being folded into this one.
    Now { missed: u32 },
}

/// How far ahead a calendar is searched before it is called impossible.
///
/// Four years covers a leap day, which is the longest anything expressible here
/// can legitimately wait for. Past that the expression matches nothing —
/// `0 0 31 2 *` — and the search has to stop rather than spin.
const HORIZON_MINUTES: i64 = 4 * 366 * 24 * 60;

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
    let floor = match last_run {
        Some(last) => {
            let interval = schedule
                .every
                .map(|every| i64::try_from(every.as_secs()).unwrap_or(i64::MAX))
                .unwrap_or(0);
            last.saturating_add(interval.max(MIN_GAP_SECS))
        }
        // Nothing has run. There is no past to wait from, and no run to be too
        // close to.
        None => now,
    };

    let Some(calendar) = &schedule.calendar else {
        return Some(floor.max(now));
    };

    // Minute resolution, so the search starts at the next whole minute at or
    // after the floor. Seconds are not expressible in either syntax.
    let start = floor.max(now);
    let first = start.div_euclid(60) + i64::from(start.rem_euclid(60) != 0);

    (first..)
        .take(HORIZON_MINUTES as usize)
        .find(|minute| {
            // A local time that does not exist — the hour a spring-forward
            // skips — maps to no instant, and one that happens twice maps to
            // two. `single()` accepts neither, so a time the wall clock never
            // showed is stepped over rather than invented.
            zone.timestamp_opt(minute * 60, 0)
                .single()
                .is_some_and(|at| calendar.allows(&at))
        })
        .map(|minute| minute * 60)
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
        return Due::At(i64::MAX);
    };

    if next > now {
        return Due::At(next);
    }

    // No floor check here. `next_after` puts `MIN_GAP_SECS` into the start of
    // its search, so `next <= now` already implies `now - last >= MIN_GAP_SECS`
    // — and unlike a check bolted on afterwards, the answer it gives is one the
    // calendar allows. Checking it in this position was the bug: for a calendar
    // the code below was all but unreachable, and when it was reached it
    // answered off-grid.
    debug_assert!(
        last_run.is_none_or(|last| now - last >= MIN_GAP_SECS),
        "the floor belongs to next_after and it did not hold"
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
    // Only moments strictly before `now` are counted, so the one due this
    // instant is already excluded — nothing is subtracted afterwards.
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
    while counted < MAX_MISSED_COUNTED {
        match next_after(schedule, Some(at), at, zone) {
            Some(next) if next < now && next > at => {
                counted += 1;
                at = next;
            }
            _ => break,
        }
    }
    counted
}

/// The largest number of skipped runs worth counting exactly.
const MAX_MISSED_COUNTED: u32 = 1_000;

/// The moment to actually wake at, once jitter is applied.
///
/// `roll` is a number in `[0, 1)` the caller supplies, so this stays pure and a
/// test can pin it. **Forward only**: moving a run earlier could put it before
/// the `--every` floor, which is the one thing the floor is there to prevent.
pub fn with_jitter(due_at: i64, jitter: Duration, roll: f64) -> i64 {
    let spread = jitter.as_secs() as f64 * roll.clamp(0.0, 1.0);
    due_at.saturating_add(spread as i64)
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
    use chrono::{FixedOffset, Utc};

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

    /// The floor holds against the runs, not only against the grid — and the
    /// moment it names is one the calendar allows.
    ///
    /// Two defects, one property. `validated()` refuses a schedule whose grid is
    /// tighter than the floor, but jitter moves each run off that grid and the
    /// next due moment is computed from where the run actually landed: `*/15` —
    /// the tightest schedule the tool advertises as legal — put half of its
    /// consecutive pairs under the floor, worst case 36 seconds apart. And the
    /// check that was supposed to stop that answered `last + MIN_GAP_SECS`, an
    /// instant no `*/15` expression names, so `--jitter 0` ran on minutes the
    /// expression forbids. Both are gone by folding the floor into the start of
    /// the search instead of testing it afterwards.
    #[test]
    fn a_run_is_never_due_inside_the_floor_nor_off_the_calendar() {
        let schedule = Schedule::cron("*/15 * * * *").unwrap();
        let start = at(0);
        let grid = 15 * 60;

        // Swept over the whole window: whatever the last run and the current
        // moment are, `due` never says "now" while the floor has not passed, and
        // whatever it does name is on the quarter hour.
        let last = start + 7;
        for ahead in 0..MIN_GAP_SECS {
            match due(&schedule, Some(last), last + ahead, &Utc) {
                Due::At(next) => {
                    assert!(
                        next - last >= MIN_GAP_SECS,
                        "next run {}s after the last one",
                        next - last
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

        // And the pair the old floor produced: a run at :14:59 must wait for
        // :30:00, not for :29:59.
        let last = start + MIN_GAP_SECS - 1;
        assert_eq!(
            due(&schedule, Some(last), start + MIN_GAP_SECS, &Utc),
            Due::At(start + 2 * grid)
        );
    }

    /// A jitter larger than the gap it is jittering within is not a jitter.
    #[test]
    fn jitter_is_bounded_by_what_the_schedule_can_absorb() {
        let every = Duration::from_secs(hours(6) as u64);
        let schedule = Schedule::every(every)
            .unwrap()
            .with_jitter(Duration::from_secs(30 * 24 * 3_600));
        assert_eq!(schedule.jitter(), every);

        let calendar = Schedule::calendar(&[], &[(9, 0), (21, 0)])
            .unwrap()
            .with_jitter(Duration::from_secs(30 * 24 * 3_600));
        assert_eq!(calendar.jitter(), Duration::from_secs(hours(12) as u64));

        // With both halves set, the tighter of the two bounds it. The interval
        // alone used to, so `--every 2w --on mon --at 09:00 --jitter 5d` kept
        // all five days and woke on a Friday, which the calendar forbids.
        let both = Schedule::calendar(&[Weekday::Mon], &[(9, 0)])
            .unwrap()
            .and_every(Duration::from_secs(14 * 24 * 3_600))
            .unwrap()
            .with_jitter(Duration::from_secs(5 * 24 * 3_600));
        assert_eq!(both.jitter(), Duration::from_secs(24 * 3_600));
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

    /// An expression that matches nothing must answer rather than search
    /// forever. There is no 31st of February.
    #[test]
    fn an_impossible_calendar_answers_none_instead_of_spinning() {
        let schedule = Schedule::cron("0 0 31 2 *").unwrap();
        assert_eq!(next_after(&schedule, None, at(0), &Utc), None);
        assert_eq!(due(&schedule, None, at(0), &Utc), Due::At(i64::MAX));
    }

    /// The search matches against local wall-clock time, which is what decides
    /// what happens at a daylight-saving transition.
    ///
    /// A fixed offset has no transitions, so this cannot provoke one; what it
    /// pins is the mechanism that governs both cases. `next_after` asks
    /// `timestamp_opt(..).single()`, so an hour the local clock skips over maps
    /// to no instant and is stepped past rather than invented, and an hour that
    /// happens twice maps to two and is not fired twice — `MIN_GAP_SECS` is
    /// what stops the second, since a run within fifteen minutes of the last is
    /// not due. Proving that end to end would mean carrying a zone database
    /// into the test build for one assertion.
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
