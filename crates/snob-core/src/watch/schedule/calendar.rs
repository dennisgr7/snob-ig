//! The set of minutes a run is allowed to happen in.
//!
//! What both syntaxes are read into, and the three questions the rest of the
//! schedule puts to it: whether a moment is allowed, how close together two
//! allowed moments can be, and how much of a day is left after the last of
//! them. The first decides a run, the other two bound the floor and the
//! jitter.

use chrono::{DateTime, Datelike, TimeZone, Timelike};

use super::parse::FieldSet;

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
pub(super) struct Calendar {
    pub(super) times: Times,
    pub(super) days_of_month: FieldSet,
    pub(super) months: FieldSet,
    /// Sunday is 0, the way cron numbers them.
    pub(super) days_of_week: FieldSet,
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
pub(super) enum Times {
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
    pub(super) fn allows<Tz: TimeZone>(&self, at: &DateTime<Tz>) -> bool {
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
    pub(super) fn tightest_gap(&self) -> Option<i64> {
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

    /// Whether the day fields name a subset of the days rather than all of them.
    ///
    /// The jitter ceiling asks, because pushing a run past midnight is only
    /// harmless when the next day is one the calendar allows too.
    pub(super) fn days_are_restricted(&self) -> bool {
        !self.days_of_month.is_all(1..=31) || !self.days_of_week.is_all(0..=6)
    }

    /// How much of the day is left after its last moment.
    ///
    /// The bound on jitter when the days are restricted: past midnight is a day
    /// this calendar does not name, and a run that lands there is the failure
    /// [`Schedule::with_jitter`]'s doc says the bound exists to stop.
    pub(super) fn room_before_midnight(&self) -> i64 {
        let latest = self.times.moments().into_iter().max().unwrap_or(0);
        24 * 3_600 - latest
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

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::super::parse::parse_cron;
    use super::super::tests::{at, hours};
    use super::super::{Schedule, ScheduleError, Weekday};
    use crate::Epoch;

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

    /// The gap across midnight is a gap. `23:55` and `00:00` are five minutes
    /// apart, and measuring only the forward differences within a day would
    /// call them nearly twenty-four hours and let them through.
    #[test]
    fn the_wrap_around_midnight_counts_as_a_gap() {
        assert!(Schedule::calendar(&[], &[(23, 55), (0, 0)]).is_err());
    }

    /// The rule every reimplementation gets wrong. `0 9 13 * 5` is "the 13th,
    /// and every Friday" -- not "Friday the 13th".
    #[test]
    fn two_restricted_day_fields_combine_with_or() {
        let calendar = parse_cron("0 9 13 * 5").unwrap();
        let day = |at: Epoch| calendar.allows(&Utc.timestamp_opt(at.get(), 0).unwrap());

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
        let day = |at: Epoch| calendar.allows(&Utc.timestamp_opt(at.get(), 0).unwrap());

        assert!(day(at(hours(9))), "Monday");
        assert!(!day(at(hours(24) + hours(9))), "Tuesday");
    }
}
