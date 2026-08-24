//! Reading the two syntaxes.
//!
//! `--on`/`--at` and a five-field cron expression, and the bitmask both of
//! them end up in. Nothing here decides when a run happens: what it produces
//! is a `Calendar`, and asking that calendar anything is `next`'s job.

use super::ScheduleError;
use super::calendar::{Calendar, Times};

/// A set of small numbers, as a bitmask.
///
/// Every cron field fits in 64 bits — the widest is day-of-month at 31 — so a
/// field is one integer and testing membership is one shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FieldSet(pub(super) u64);

impl FieldSet {
    /// Everything in `range`, which is what `*` means.
    ///
    /// A mask rather than a loop. `Calendar::allows` asks `is_all` twice per
    /// minute it looks at, and the search looks at a lot of minutes; the loop
    /// rebuilt the set from scratch on every one of them.
    pub(super) fn all(range: std::ops::RangeInclusive<u32>) -> Self {
        if range.is_empty() {
            return Self(0);
        }
        let (start, end) = (*range.start(), *range.end());
        debug_assert!(end < 64, "a field value is a bit in a u64");
        let through_end = u64::MAX >> (63 - end);
        let below_start = (1u64 << start) - 1;
        Self(through_end & !below_start)
    }

    pub(super) fn contains(self, value: u32) -> bool {
        value < 64 && self.0 & (1 << value) != 0
    }

    /// Whether this field was left unrestricted over `range`.
    ///
    /// Only day-of-month and day-of-week need to know, and only to settle how
    /// they combine. It is asked of the set rather than remembered from the
    /// text so that `*` and `1-31` behave identically, which is what a reader
    /// of the expression would expect.
    pub(super) fn is_all(self, range: std::ops::RangeInclusive<u32>) -> bool {
        self == Self::all(range)
    }
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
    pub(super) fn number(self) -> u32 {
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
pub(super) fn parse_cron(expression: &str) -> Result<Calendar, ScheduleError> {
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
            "{value} is not {} {name} (it has to be between {} and {})",
            article(name),
            range.start(),
            range.end()
        )));
    }
    Ok(value)
}

/// "an hour", "a minute": the field names are few and one of them starts
/// with a vowel sound, and "24 is not a hour" is what a person was shown.
fn article(name: &str) -> &'static str {
    if name.starts_with(['a', 'e', 'i', 'o', 'u', 'h']) {
        "an"
    } else {
        "a"
    }
}

fn unreadable_field(text: &str, name: &str) -> ScheduleError {
    ScheduleError::Unreadable(format!(
        "\"{text}\" is not {} {name} a cron expression can have",
        article(name)
    ))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::super::Schedule;
    use super::super::next::next_after;
    use super::super::tests::{at, hours};
    use super::*;

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

    /// The mask is the same set the loop it replaced built, for every range a
    /// field has, and for the empty one.
    #[test]
    fn a_whole_field_is_every_value_in_its_range() {
        for range in [1..=31, 0..=6, 0..=59, 0..=23, 1..=12, 0..=63] {
            let mut bits = 0u64;
            for value in range.clone() {
                bits |= 1 << value;
            }
            assert_eq!(FieldSet::all(range.clone()), FieldSet(bits), "{range:?}");
            assert!(FieldSet::all(range.clone()).is_all(range));
        }
        let empty = std::ops::RangeInclusive::new(5, 4);
        assert_eq!(FieldSet::all(empty), FieldSet(0), "an empty range is empty");
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
