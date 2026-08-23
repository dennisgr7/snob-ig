//! The evaluator: which moment comes next, and whether one is owed now.
//!
//! The floor, the search across the calendar in both directions, the count of
//! runs that went by unrun, and the jitter that keeps every copy of this tool
//! off the same round minute. Nothing here reads a clock either — `now` is an
//! argument to all of it.

use std::time::Duration;

use chrono::{DateTime, MappedLocalTime, TimeZone};

use crate::Epoch;

use super::calendar::Calendar;
use super::{Due, MIN_GAP_SECS, Schedule};

/// How far ahead a calendar is searched before it is called impossible.
///
/// Four years covers a leap day, which is the longest anything expressible here
/// can legitimately wait for. Past that the expression matches nothing —
/// `0 0 31 2 *` — and the search has to stop rather than spin.
///
/// A span of time, not a number of evaluations: the search steps over whole
/// days the calendar does not name, so reaching the horizon costs about as many
/// steps as there are days in it, not minutes.
const HORIZON_MINUTES: i64 = 4 * 366 * 24 * 60; // four years, in minutes

/// Which way a search walks the calendar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

/// The minutes from `from`, in `direction`, that fall on a day the calendar
/// names — as the minute index and the local time it reads — out to the
/// horizon.
///
/// A day the calendar does not name is stepped over whole. It holds no minute
/// `allows` could accept, and asking about each one costs a timezone conversion
/// per minute: walking every minute of the horizon is what `0 0 31 2 *` used to
/// cost — 2.1 million conversions, 1.6 s in release, inside `snob check`, for
/// the answer "never" — and `watch` paid it again on every wake, through
/// `room_at`. A day the calendar does name is still walked minute by minute, so
/// nothing about how a minute is judged changes here.
///
/// **The neighboring day is asked of the zone, not reached by adding
/// twenty-four hours.** The day a zone springs forward through is twenty-three
/// hours long, so twenty-four hours from its midnight is 01:00 of the day
/// after — an hour into a day the calendar may name, past a moment the walk
/// would then never see. Going back, the same arithmetic on a twenty-five-hour
/// day lands an hour short, which is harmless, and on the short day an hour too
/// far, which is not. So the jump lands on the first minute the zone says the
/// next day has (the last one the previous day has, going back), and steps a
/// single minute instead when the zone has no such instant — a midnight it
/// skips — or when the landing would not move the cursor the way it is walking,
/// which a fall-back that straddles midnight can produce.
///
/// One deviation from the minute walk is accepted, and it is confined to zones
/// whose fall-back crosses midnight: there, the instants that read "yesterday
/// 23:xx" for the second time sit inside today's span, and a forward jump from
/// today to tomorrow steps over them. The look back refuses those second
/// showings whenever their first was served anyway; what is left is a first run
/// that lands in that hour, once a year, in a handful of zones.
fn minutes_on_named_days<'a, Tz: TimeZone>(
    calendar: &'a Calendar,
    zone: &'a Tz,
    from: i64,
    direction: Direction,
) -> impl Iterator<Item = (i64, DateTime<Tz>)> + 'a {
    let step = match direction {
        Direction::Forward => 1,
        Direction::Backward => -1,
    };
    let mut cursor = from;
    std::iter::from_fn(move || {
        loop {
            if (cursor - from).abs() >= HORIZON_MINUTES {
                return None;
            }
            let Some(at) = zone.timestamp_opt(cursor * 60, 0).single() else {
                cursor += step;
                continue;
            };
            if calendar.allows_day(&at) {
                let found = (cursor, at);
                cursor += step;
                return Some(found);
            }
            let landing = match direction {
                Direction::Forward => first_instant_of_next_local_day(&at, zone)
                    .map(|instant| minute_at_or_after(instant.timestamp()))
                    .filter(|minute| *minute > cursor),
                Direction::Backward => last_instant_of_previous_local_day(&at, zone)
                    .map(|instant| instant.timestamp().div_euclid(60))
                    .filter(|minute| *minute < cursor),
            };
            cursor = landing.unwrap_or(cursor + step);
        }
    })
}

/// The first whole minute at or after an instant.
fn minute_at_or_after(seconds: i64) -> i64 {
    seconds.div_euclid(60) + i64::from(seconds.rem_euclid(60) != 0)
}

/// The first instant of the local day after the one `at` falls in.
///
/// `None` when the zone skips its own midnight, which some have done: there is
/// no such instant. The `earliest` of an ambiguous midnight, because the first
/// showing is the first instant that reads tomorrow's date.
fn first_instant_of_next_local_day<Tz: TimeZone>(
    at: &DateTime<Tz>,
    zone: &Tz,
) -> Option<DateTime<Tz>> {
    let tomorrow = at.date_naive().succ_opt()?.and_hms_opt(0, 0, 0)?;
    zone.from_local_datetime(&tomorrow).earliest()
}

/// The last minute of the local day before the one `at` falls in.
///
/// The `latest` of an ambiguous 23:59, because the second showing is the last
/// instant that reads yesterday's date. `None` when the zone has no such
/// instant.
fn last_instant_of_previous_local_day<Tz: TimeZone>(
    at: &DateTime<Tz>,
    zone: &Tz,
) -> Option<DateTime<Tz>> {
    let yesterday = at.date_naive().pred_opt()?.and_hms_opt(23, 59, 0)?;
    zone.from_local_datetime(&yesterday).latest()
}

/// The next moment this schedule is due, for somebody who wants to see it
/// rather than sleep until it.
///
/// The same function the loop uses, deliberately: a preflight that worked the
/// moments out its own way would be checking a schedule nobody runs. `None`
/// means the calendar can never match, which is the one answer worth a red line
/// before anything is scheduled at all.
pub fn next_moment<Tz: TimeZone>(
    schedule: &Schedule,
    last_run: Option<Epoch>,
    now: Epoch,
    zone: &Tz,
) -> Option<Epoch> {
    next_after(schedule, last_run, now, zone)
}

/// The next moment this schedule is due after `now`.
///
/// `None` only when the calendar can never match. Both arguments and the answer
/// are moments; the zone is what turns them into wall-clock time, and it is
/// passed in so a test can pick one rather than inherit the machine's.
///
/// The minute arithmetic below works in bare seconds, and deliberately: a
/// minute index is not a moment, and neither is the offset of one from another.
/// The moments come in and go out as [`Epoch`], which is where a transposition
/// would cost something.
pub(super) fn next_after<Tz: TimeZone>(
    schedule: &Schedule,
    last_run: Option<Epoch>,
    now: Epoch,
    zone: &Tz,
) -> Option<Epoch> {
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
    // The addition saturates, and [`Epoch`]'s own `Add` is where that now
    // lives: an absurd `--every` can arrive from a hand-edited `watch.toml`,
    // the sum overflowed, and that panicked a debug build and in release
    // wrapped to a negative floor — turning an interval of billions of years
    // into one that ran every fifteen minutes.
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
        let floor = last_run.map_or(now, |last| {
            last + Duration::from_secs(interval.max(MIN_GAP_SECS) as u64)
        });
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
    // The addition saturates, in [`Epoch`]'s own `Add`, because an absurd
    // `--every` can arrive from a hand-edited `watch.toml`: the sum overflowed,
    // which panicked a debug build and in release wrapped to a negative floor,
    // turning an interval of billions of years into one that ran every fifteen
    // minutes.
    let served = last_run.map(|last| moment_served(calendar, last, zone));
    let floor = match (served, last_run) {
        // Never earlier than the row the last run wrote, whatever the snap
        // decided. A no-op in every ordinary case — the floor is a quarter of an
        // hour past a moment the row already trails — and the thing that keeps
        // this monotonic if a calendar is ever strange enough to snap somewhere
        // unhelpful.
        (Some(moment), Some(last)) => {
            (moment + Duration::from_secs(interval.max(MIN_GAP_SECS) as u64)).max(last)
        }
        _ => now,
    };

    // The candidates come from `minutes_on_named_days`, which converts each
    // instant to local time once. A local time that does not exist — the hour
    // a spring-forward skips — is never produced by that direction: going from
    // an instant to a local time is never ambiguous either, chrono answers
    // `Single` unconditionally and only fails outside the representable range.
    // Ambiguity is a property of the other direction, and it is asked about
    // below, in `already_run_at_this_wall_clock` — which is the reason for the
    // order here: the calendar first, the reverse conversion only for a minute
    // the calendar names.
    let allowed = |(minute, at): &(i64, DateTime<Tz>)| {
        calendar.allows(at)
            && !already_run_at_this_wall_clock(zone, Epoch::new(minute * 60), at, last_run)
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
    // stops at the most recent named day rather than walking the whole month.
    // The horizon bounds it for the same reason it bounds the forward search.
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
        Some(moment) => moment.get().div_euclid(60) + 1,
        None => now.get().div_euclid(60),
    };
    if floor <= now {
        let latest = now.get().div_euclid(60);
        if minutes_on_named_days(calendar, zone, latest, Direction::Backward)
            .take_while(|(minute, _)| *minute >= earliest)
            .any(|candidate| allowed(&candidate))
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
    let first = minute_at_or_after(floor.max(now).get());

    minutes_on_named_days(calendar, zone, first, Direction::Forward)
        .find(|candidate| allowed(candidate))
        .map(|(minute, _)| Epoch::new(minute * 60))
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
fn moment_served<Tz: TimeZone>(calendar: &Calendar, last: Epoch, zone: &Tz) -> Epoch {
    let from = last.get().div_euclid(60);
    minutes_on_named_days(calendar, zone, from, Direction::Backward)
        .find(|(_, at)| calendar.allows(at))
        .map_or(last, |(minute, _)| Epoch::new(minute * 60))
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
    at: Epoch,
    local: &DateTime<Tz>,
    last_run: Option<Epoch>,
) -> bool {
    let Some(last) = last_run else {
        return false;
    };
    match zone.from_local_datetime(&local.naive_local()) {
        MappedLocalTime::Ambiguous(first, second) => {
            second.timestamp() == at.get() && first.timestamp() <= last.get()
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
pub fn due<Tz: TimeZone>(
    schedule: &Schedule,
    last_run: Option<Epoch>,
    now: Epoch,
    zone: &Tz,
) -> Due {
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
/// "several" out loud, and each count is a search of its own: a thousand is
/// already more than the sentence needs.
fn missed_since<Tz: TimeZone>(
    schedule: &Schedule,
    last_run: Option<Epoch>,
    now: Epoch,
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
pub fn with_jitter(due_at: Epoch, jitter: Duration, roll: f64) -> Epoch {
    let spread = jitter.as_secs() as f64 * roll.clamp(0.0, 1.0);
    due_at + Duration::from_secs(spread as u64)
}

/// The first instant of the local day after `at`.
///
/// `None` when the zone skips its own midnight, which some have done: there is
/// no such instant, and the calendar's own search is then the only bound left.
pub(super) fn next_local_midnight<Tz: TimeZone>(at: Epoch, zone: &Tz) -> Option<Epoch> {
    let local = zone.timestamp_opt(at.get(), 0).single()?;
    first_instant_of_next_local_day(&local, zone).map(|midnight| Epoch::new(midnight.timestamp()))
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
pub fn wake_at<Tz: TimeZone>(schedule: &Schedule, due_at: Epoch, roll: f64, zone: &Tz) -> Epoch {
    with_jitter(
        due_at,
        schedule.jitter().min(schedule.room_at(due_at, zone)),
        roll,
    )
}

#[cfg(test)]
mod tests {
    use chrono::{Datelike, FixedOffset, NaiveDate, NaiveDateTime, Timelike, Utc};

    use super::super::tests::{MONDAY_0000, at, hours, secs};
    use super::super::{MAX_INTERVAL_SECS, Weekday};
    use super::*;

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

    #[test]
    fn an_interval_is_measured_from_the_previous_run() {
        let schedule = Schedule::every(Duration::from_secs(hours(6) as u64)).unwrap();
        // Started at 09:13, so the next one is 15:13 rather than 12:00.
        let last = at(hours(9) + 13 * 60);
        assert_eq!(
            next_after(&schedule, Some(last), last + secs(60), &Utc),
            Some(last + secs(hours(6)))
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
        let last = start + secs(7);
        for ahead in 0..(grid - 7) {
            match due(&schedule, Some(last), last + secs(ahead), &Utc) {
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
            due(&schedule, Some(last), start + secs(grid), &Utc),
            Due::Now { .. }
        ));

        // The extreme of the softening, stated rather than left to be found: a
        // walk that took almost the whole gap leaves the next run one second
        // after the previous row. That is what a schedule asking for a run every
        // fifteen minutes, on an account that takes fifteen minutes to walk,
        // amounts to — and what bounds the requests there is the pacer's budget,
        // not this.
        let slow = start + secs(grid - 1);
        assert!(matches!(
            due(&schedule, Some(slow), start + secs(grid), &Utc),
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
                    let row = served + secs(walk);
                    let expected = start + secs(step * grid);

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
            woken + secs(MIN_GAP_SECS) <= next,
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
        let landed = SpringsForward
            .timestamp_opt(woken.get(), 0)
            .single()
            .unwrap();
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
                let expected = start + secs(step * grid);
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
        let reading = DateTime::from_timestamp(first.get(), 0)
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
                due(
                    &schedule,
                    Some(first - secs(24 * 3_600)),
                    second,
                    &FallsBack
                ),
                Due::Now { .. }
            ),
            "a run that has not happened is still owed"
        );
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
        match due(&schedule, Some(last), last + secs(14 * hours(24)), &Utc) {
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

    /// The rule that keeps an outage from becoming a burst.
    #[test]
    fn a_process_that_was_down_for_days_runs_once_and_says_how_many_it_missed() {
        let schedule = Schedule::every(Duration::from_secs(hours(6) as u64)).unwrap();
        let last = at(0);
        // Three days later: twelve intervals have gone by.
        match due(&schedule, Some(last), last + secs(hours(72)), &Utc) {
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
        let next = next_after(&schedule, Some(last), last + secs(60), &Utc).unwrap();

        assert_eq!(
            next,
            last + secs(14 * hours(24)),
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
            at = from_dsl.expect("Monday comes round") + secs(60);
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
            let local = Utc.timestamp_opt(next.get(), 0).unwrap();
            fired.push(format!("{:02}:{:02}", local.hour(), local.minute()));
            at = next + secs(60);
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
            let local = Utc.timestamp_opt(next.get(), 0).unwrap();
            fired.push(format!("{:02}:{:02}", local.hour(), local.minute()));
            at = next + secs(60);
        }

        assert_eq!(fired, vec!["09:00", "09:30", "21:00", "21:30"]);
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
        let a_century = at(0) + secs(100 * 366 * 24 * 3_600);
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

        let local = zone.timestamp_opt(next.get(), 0).unwrap();
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
            assert!(woken <= due_at + secs(900));
        }
    }

    /// A roll outside its range is clamped rather than trusted: the caller
    /// supplies it, and an out-of-range one would push a run arbitrarily far.
    #[test]
    fn a_roll_outside_its_range_cannot_push_a_run_anywhere() {
        let due_at = at(0);
        assert_eq!(
            with_jitter(due_at, Duration::from_secs(900), 5.0),
            due_at + secs(900)
        );
        assert_eq!(with_jitter(due_at, Duration::from_secs(900), -1.0), due_at);
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
        let Due::At(next) = due(&schedule, Some(now + secs(hours(24))), now, &Utc) else {
            panic!("a run six hours out is not due yet");
        };
        assert_eq!(
            next,
            now + secs(hours(6)),
            "the skew is discarded rather than waited out"
        );
    }

    // ----- The day jump, against the two zones with a transition in them -----

    /// What the search used to do, kept so the jumping one can be swept
    /// against it: a minute at a time, a conversion per minute, out to
    /// `horizon` minutes. Verbatim from the code it replaced, with the horizon
    /// as a parameter so that "never" stays cheap to ask about.
    fn reference_next_after<Tz: TimeZone>(
        schedule: &Schedule,
        last_run: Option<Epoch>,
        now: Epoch,
        zone: &Tz,
        horizon: i64,
    ) -> Option<Epoch> {
        let interval = schedule
            .every
            .map(|every| i64::try_from(every.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let calendar = calendar_of(schedule);
        let served = last_run.map(|last| reference_moment_served(calendar, last, zone, horizon));
        let floor = match (served, last_run) {
            (Some(moment), Some(last)) => {
                (moment + Duration::from_secs(interval.max(MIN_GAP_SECS) as u64)).max(last)
            }
            _ => now,
        };
        let allowed = |minute: &i64| {
            let seconds = minute * 60;
            let Some(at) = zone.timestamp_opt(seconds, 0).single() else {
                return false;
            };
            calendar.allows(&at)
                && !already_run_at_this_wall_clock(zone, Epoch::new(seconds), &at, last_run)
        };
        let earliest = match served {
            Some(moment) => moment.get().div_euclid(60) + 1,
            None => now.get().div_euclid(60),
        };
        if floor <= now {
            let latest = now.get().div_euclid(60);
            if (earliest..=latest)
                .rev()
                .take(horizon as usize)
                .any(|minute| allowed(&minute))
            {
                return Some(now);
            }
        }
        let start = floor.max(now).get();
        let first = start.div_euclid(60) + i64::from(start.rem_euclid(60) != 0);
        (first..)
            .take(horizon as usize)
            .find(allowed)
            .map(|minute| Epoch::new(minute * 60))
    }

    fn reference_moment_served<Tz: TimeZone>(
        calendar: &Calendar,
        last: Epoch,
        zone: &Tz,
        horizon: i64,
    ) -> Epoch {
        let from = last.get().div_euclid(60);
        (0..)
            .take(horizon as usize)
            .map(|back| from - back)
            .find(|minute| {
                zone.timestamp_opt(minute * 60, 0)
                    .single()
                    .is_some_and(|at| calendar.allows(&at))
            })
            .map_or(last, |minute| Epoch::new(minute * 60))
    }

    fn calendar_of(schedule: &Schedule) -> &Calendar {
        schedule.calendar.as_ref().expect("a calendar")
    }

    /// Twenty-four hours from the midnight of a twenty-three-hour day is 01:00
    /// of the day after, and a jump that landed there stepped over Tuesday's
    /// 00:30 and answered the Tuesday after. The jump asks the zone instead.
    ///
    /// The fixture: Monday is `[at(5h), at(28h))` in `SpringsForward`, and
    /// Tuesday 00:30 local, at the post-transition offset, is `at(28h30m)`.
    #[test]
    fn a_day_jump_does_not_overshoot_the_hour_a_spring_forward_takes() {
        let schedule = Schedule::calendar(&[Weekday::Tue], &[(0, 30)]).unwrap();
        assert_eq!(
            next_after(&schedule, None, at(hours(5)), &SpringsForward),
            Some(at(hours(28) + 30 * 60)),
            "Tuesday 00:30, not the Tuesday after"
        );
    }

    /// Going back, twenty-four hours from the last minute of the short day is
    /// 22:59 of the day before, and the hour that 23:30 sits in was never
    /// looked at. Sunday 23:30 local, before the transition, is `at(4h30m)`.
    #[test]
    fn the_moment_served_is_found_across_a_day_an_hour_short() {
        let schedule = Schedule::calendar(&[Weekday::Sun], &[(23, 30)]).unwrap();
        // Monday 23:59 local, after the transition.
        let late_monday = at(hours(27) + 59 * 60);
        assert_eq!(
            moment_served(calendar_of(&schedule), late_monday, &SpringsForward),
            at(hours(4) + 30 * 60),
            "the Sunday moment, not the one a week before it"
        );
    }

    /// The look back jumps Tuesday to Monday to Sunday and still finds the
    /// moment that went by; and the one being served is not counted as missed.
    #[test]
    fn a_moment_that_went_by_before_a_short_day_is_still_owed() {
        let schedule = Schedule::calendar(&[Weekday::Sun], &[(23, 30)]).unwrap();
        let sunday_moment = at(hours(4) + 30 * 60);
        let a_week_before = sunday_moment - secs(7 * 24 * 3_600);
        let tuesday_0010 = at(hours(28) + 10 * 60);
        assert_eq!(
            due(
                &schedule,
                Some(a_week_before),
                tuesday_0010,
                &SpringsForward
            ),
            Due::Now { missed: 0 }
        );
    }

    /// On the long day the arithmetic lands an hour short rather than an hour
    /// far, which the re-check after landing absorbs; pinned so that the
    /// twenty-five-hour path is driven too. Monday is `[at(4h), at(29h))` in
    /// `FallsBack`.
    #[test]
    fn a_day_jump_lands_on_the_day_after_one_with_twenty_five_hours() {
        let tuesday = Schedule::calendar(&[Weekday::Tue], &[(0, 30)]).unwrap();
        assert_eq!(
            next_after(&tuesday, None, at(hours(4)), &FallsBack),
            Some(at(hours(29) + 30 * 60)),
            "Tuesday 00:30 at the post-transition offset"
        );

        let sunday = Schedule::calendar(&[Weekday::Sun], &[(23, 30)]).unwrap();
        // Monday 23:59 local, after the transition.
        let late_monday = at(hours(28) + 59 * 60);
        assert_eq!(
            moment_served(calendar_of(&sunday), late_monday, &FallsBack),
            at(hours(3) + 30 * 60),
            "Sunday 23:30 at the pre-transition offset"
        );
    }

    /// A jump back from Tuesday lands at the end of a Monday with a repeated
    /// hour in it, and the walk through that hour still refuses the second
    /// showing of a moment the first showing served.
    #[test]
    fn the_look_back_jumps_over_a_day_and_still_refuses_the_second_showing() {
        let schedule = Schedule::calendar(&[Weekday::Mon], &[(1, 30)]).unwrap();
        let first_showing = at(hours(5) + 1_800);
        let tuesday_0010 = at(hours(29) + 10 * 60);
        assert_eq!(
            due(&schedule, Some(first_showing), tuesday_0010, &FallsBack),
            Due::At(at(7 * hours(24) + hours(6) + 1_800)),
            "next Monday 01:30, at the post-transition offset"
        );
    }

    /// The leap day is four years out and the jump has to get there, day by
    /// day, rather than stop at a horizon measured in steps.
    #[test]
    fn a_leap_day_is_found_four_years_out() {
        let schedule = Schedule::cron("0 9 29 2 *").unwrap();
        assert_eq!(
            next_after(&schedule, None, at(0), &Utc),
            Some(Epoch::new(1_835_427_600)),
            "2028-02-29 09:00 UTC"
        );
    }

    /// The iterator, on its own: over a three-day window in each zone with a
    /// transition, it yields exactly the minutes whose local day the calendar
    /// names, in order, in both directions.
    #[test]
    fn the_day_iterator_names_exactly_the_minutes_the_calendar_does() {
        fn check<Tz: TimeZone + std::fmt::Debug>(
            schedule: &Schedule,
            zone: &Tz,
            from: i64,
            to: i64,
        ) {
            let calendar = calendar_of(schedule);
            let named = |minute: &i64| {
                zone.timestamp_opt(minute * 60, 0)
                    .single()
                    .is_some_and(|at| calendar.allows_day(&at))
            };
            let expected_forward: Vec<i64> = (from..=to).filter(named).collect();
            let forward: Vec<i64> = minutes_on_named_days(calendar, zone, from, Direction::Forward)
                .map(|(minute, _)| minute)
                .take_while(|minute| *minute <= to)
                .collect();
            assert_eq!(
                forward, expected_forward,
                "{schedule:?} forward in {zone:?}"
            );

            let expected_backward: Vec<i64> = (from..=to).rev().filter(named).collect();
            let backward: Vec<i64> = minutes_on_named_days(calendar, zone, to, Direction::Backward)
                .map(|(minute, _)| minute)
                .take_while(|minute| *minute >= from)
                .collect();
            assert_eq!(
                backward, expected_backward,
                "{schedule:?} backward in {zone:?}"
            );
        }

        let schedules = [
            Schedule::calendar(&[Weekday::Tue], &[(0, 30)]).unwrap(),
            Schedule::calendar(&[Weekday::Sun], &[(23, 30)]).unwrap(),
            Schedule::cron("0 9 13 * 5").unwrap(),
        ];
        let from = at(-hours(24)).get().div_euclid(60);
        let to = at(hours(52)).get().div_euclid(60);
        for schedule in &schedules {
            check(schedule, &FallsBack, from, to);
            check(schedule, &SpringsForward, from, to);
            check(schedule, &Utc, from, to);
        }
    }

    /// The sweep: the jumping search and the minute-by-minute one it replaced
    /// answer the same, across both transition fixtures, with and without a
    /// last run, on calendars that name some days and calendars that name them
    /// all. Every six hours over two days, and every half hour through the
    /// hours around the transition, where the two could differ.
    #[test]
    fn the_jumping_search_agrees_with_the_minute_by_minute_one() {
        fn sweep<Tz: TimeZone + std::fmt::Debug>(schedule: &Schedule, zone: &Tz, horizon: i64) {
            let coarse = (0..=8).map(|step| at(-hours(2) + step * hours(6)));
            let fine = (0..=9).map(|step| at(hours(4) + step * 30 * 60));
            for now in coarse.chain(fine) {
                for last_run in [
                    None,
                    Some(now - secs(3_600)),
                    Some(now - secs(25 * 3_600)),
                    Some(now - secs(2 * 24 * 3_600)),
                ] {
                    assert_eq!(
                        next_after(schedule, last_run, now, zone),
                        reference_next_after(schedule, last_run, now, zone, horizon),
                        "{schedule:?} in {zone:?} at {now} after {last_run:?}"
                    );
                }
            }
        }

        // Two or three days a week each, so the reference walk -- a minute at
        // a time, the thing being replaced -- stays short enough to run on
        // every build; the jumps over the unnamed days between are the point.
        let schedules = [
            Schedule::calendar(&[Weekday::Tue, Weekday::Fri], &[(0, 30)]).unwrap(),
            Schedule::calendar(&[Weekday::Sun, Weekday::Wed], &[(23, 30)]).unwrap(),
            Schedule::calendar(&[Weekday::Mon, Weekday::Thu], &[(1, 30)]).unwrap(),
            Schedule::cron("30 1 * * *").unwrap(),
            Schedule::cron("0 9 13,17,19,21 * 5").unwrap(),
        ];
        let ten_days = 10 * 24 * 60;
        for schedule in &schedules {
            sweep(schedule, &FallsBack, ten_days);
            sweep(schedule, &SpringsForward, ten_days);
        }
    }
}
