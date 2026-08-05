//! The sentences the commands share.
//!
//! Small, but worth one home: these phrases are the ones a user compares
//! between commands, and two copies of "run it again" that drifted apart would
//! read as two different pieces of advice about the same situation.

use snob_core::model::{ListKind, StopReason};

use crate::exit::{ExitCode, ExitError};

/// "03/08 at 14:12", from a timestamp in epoch seconds. UTC, like every other
/// timestamp the tool prints.
pub fn stored_on(taken_at: i64) -> String {
    format_epoch(taken_at, "earlier")
}

/// "04/08 at 16:30", from a cooldown end in epoch milliseconds.
pub fn cooldown_ends_at(until_ms: i64) -> String {
    format_epoch(until_ms.div_euclid(1000), "later")
}

fn format_epoch(seconds: i64, unknown: &str) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|t| t.format("%d/%m at %H:%M").to_string())
        .unwrap_or_else(|| unknown.to_string())
}

/// The refusal shared by the crossings and the summary.
///
/// Both stop for the same reason and owe the user the same three things: which
/// list failed, what the answer would have claimed if it had been given anyway,
/// and whether running it again continues or starts over. Two copies of that
/// would be two ways of describing one situation.
///
/// `misreading` completes "the accounts missing from it would appear as if …",
/// which is the only part that differs between them.
pub fn refuse_incomplete(list: ListKind, reason: StopReason, misreading: &str) -> anyhow::Error {
    ExitError::new(
        ExitCode::from_stop_reason(reason),
        format!(
            "the {list} list could not be read in full, so the answer would be wrong: \
             the accounts missing from it would appear as if {misreading}.\n{}",
            try_again_advice(reason)
        ),
    )
    .into()
}

/// "3 unfollowers", plus what the filters took out when they took anything.
///
/// Both forms of the noun are handed in rather than an `s` being bolted on:
/// what gets counted here is a whole phrase — "accounts you follow that do not
/// follow you back" — whose singular differs by three words rather than by a
/// final letter. The count of one is not a rare case, either: it is what a
/// filtered list reaches most often.
pub fn counted(shown: usize, before_filtering: usize, one: &str, many: &str) -> String {
    let what = if shown == 1 { one } else { many };
    if shown == before_filtering {
        return format!("{shown} {what}");
    }
    format!("{shown} {what} (of {before_filtering}, the rest filtered out)")
}

/// "1 request" / "7 requests".
pub fn requests(n: u32) -> String {
    if n == 1 {
        "1 request".to_string()
    } else {
        format!("{n} requests")
    }
}

/// The closing advice for a message about an incomplete walk.
///
/// A walk stopped by throttling can never be resumed: the resume window is
/// fifteen minutes from `started_at` and the shortest cooldown is two hours, so
/// by the time walking is allowed again the partial has expired. Promising a
/// continuation there would be a lie.
pub fn try_again_advice(reason: StopReason) -> &'static str {
    if reason == StopReason::RateLimit {
        "Run it again once the cooldown lifts; a walk stopped by throttling starts over."
    } else {
        "Run it again to continue where it left off."
    }
}

/// Why a walk did not finish, in the words the summary uses.
pub fn why_incomplete(reason: StopReason) -> Option<&'static str> {
    match reason {
        StopReason::Completed => None,
        StopReason::PageLimit => Some("cut short by the cap you asked for"),
        StopReason::Canceled => Some("interrupted"),
        StopReason::Truncated => Some("Instagram stopped serving pages"),
        StopReason::RateLimit => Some("Instagram is throttling requests"),
        StopReason::Network => Some("network failure"),
        StopReason::SessionInvalid => Some("the session stopped working"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A timestamp that makes no sense still has to read as something, because
    /// the alternative is a message with a hole in the middle of it.
    #[test]
    fn a_date_out_of_range_still_reads_as_something() {
        assert_eq!(stored_on(i64::MAX), "earlier");
        assert_eq!(cooldown_ends_at(i64::MAX), "later");
        assert_eq!(stored_on(1_722_700_000), "03/08 at 15:46");
        // Milliseconds, and a negative one must not round towards zero into a
        // different second than it belongs to.
        assert_eq!(cooldown_ends_at(1_722_700_000_000), "03/08 at 15:46");
    }

    /// The refusal has to name the mistake the caller would otherwise have
    /// made, and carry the code that says what to do about it.
    #[test]
    fn refusing_names_the_list_the_misreading_and_the_code() {
        let error = refuse_incomplete(
            ListKind::Followers,
            StopReason::RateLimit,
            "they did not follow you",
        );
        let text = error.to_string();
        assert!(text.contains("followers list"), "{text}");
        assert!(text.contains("they did not follow you"), "{text}");
        assert!(text.contains("starts over"), "{text}");

        assert_eq!(ExitCode::from_chain(&error), Some(ExitCode::RateLimited));
    }

    #[test]
    fn a_refusal_carries_the_code_of_whatever_stopped_the_walk() {
        for (reason, expected) in [
            (StopReason::Canceled, ExitCode::Interrupted),
            (StopReason::RateLimit, ExitCode::RateLimited),
            (StopReason::SessionInvalid, ExitCode::NoSession),
            (StopReason::Truncated, ExitCode::Error),
        ] {
            let error = refuse_incomplete(ListKind::Following, reason, "whatever");
            assert_eq!(ExitCode::from_chain(&error), Some(expected), "{reason:?}");
        }
    }

    /// Filters that took nothing out must not leave a clause saying they did.
    #[test]
    fn the_count_only_mentions_filtering_when_something_was_filtered() {
        assert_eq!(counted(3, 3, "unfollower", "unfollowers"), "3 unfollowers");
        assert_eq!(
            counted(3, 10, "unfollower", "unfollowers"),
            "3 unfollowers (of 10, the rest filtered out)"
        );
    }

    /// A filtered list lands on one result often enough that "1 accounts you
    /// follow that do not follow you back" was the summary users saw most.
    #[test]
    fn a_count_of_one_reads_as_one() {
        assert_eq!(counted(1, 1, "unfollower", "unfollowers"), "1 unfollower");
        assert_eq!(
            counted(1, 9, "unfollower", "unfollowers"),
            "1 unfollower (of 9, the rest filtered out)"
        );
        assert_eq!(counted(0, 0, "unfollower", "unfollowers"), "0 unfollowers");
    }

    #[test]
    fn the_request_count_is_pluralized() {
        assert_eq!(requests(0), "0 requests");
        assert_eq!(requests(1), "1 request");
        assert_eq!(requests(2), "2 requests");
    }

    #[test]
    fn the_advice_depends_on_the_stop_reason() {
        assert!(try_again_advice(StopReason::RateLimit).contains("starts over"));
        for reason in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Network,
        ] {
            assert!(try_again_advice(reason).contains("continue where it left off"));
        }
    }

    /// Only a full walk has nothing to explain. Every other ending owes the
    /// user a reason, and a missing one would print an empty clause.
    #[test]
    fn every_incomplete_ending_has_something_to_say() {
        assert_eq!(why_incomplete(StopReason::Completed), None);
        for reason in StopReason::ALL {
            if reason != StopReason::Completed {
                assert!(why_incomplete(reason).is_some(), "{reason:?}");
            }
        }
    }
}
