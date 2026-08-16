//! The sentences the commands share.
//!
//! Small, but worth one home: these phrases are the ones a user compares
//! between commands, and two copies of "run it again" that drifted apart would
//! read as two different pieces of advice about the same situation.

use snob_core::model::{ListKind, StopReason};

use crate::engine::Provenance;

use crate::exit::{ExitCode, ExitError};

/// Prints a failed run's error, as one message rather than as several.
///
/// The refusals this tool produces are paragraphs — two or three sentences with
/// deliberate newlines between them. Printed with a bare `error: {e}`, only the
/// first line carried the label and every one after it started at column zero,
/// so a single refusal read as one error followed by some unattributed text.
/// The continuations are indented under the label now, and the advice comes
/// back on its own as a `hint:` rather than as more of the complaint.
///
/// A run the user stopped is not a failure to report: it gets no label and no
/// cause chain, because "error:" over "@someone was not confirmed" reads as a
/// reprimand for doing something wrong.
///
/// `.for_stderr()` on every styled label is not optional. Without it `console`
/// decides on stdout's colour state, so the labels lose their colour when only
/// stdout is redirected, and write escape codes into the file when only stderr
/// is.
pub fn print_error(error: &anyhow::Error) {
    let code = ExitCode::from_chain(error);
    if code == Some(ExitCode::Interrupted) {
        eprintln!("{error}");
        return;
    }

    let label = console::style("error:").red().bold().for_stderr();
    eprintln!("{label} {}", indented(&error.to_string()));

    for cause in error.chain().skip(1) {
        let caused = console::style("caused by:").dim().for_stderr();
        eprintln!("  {caused} {}", indented(&cause.to_string()));
    }

    if let Some(hint) = error
        .chain()
        .find_map(|c| c.downcast_ref::<ExitError>())
        .and_then(ExitError::hint)
    {
        let label = console::style("hint:").cyan().bold().for_stderr();
        eprintln!("{label}  {}", indented(hint));
    }
}

/// Lines after the first start under the label rather than at column zero.
///
/// Deliberately not re-wrapped to the terminal width: the only hard breaks in
/// these strings are the ones somebody put between sentences, and re-wrapping
/// would eventually split a URL or `snob login --paste` across a line. The
/// terminal already soft-wraps at the width it really has.
fn indented(text: &str) -> String {
    text.replace('\n', "\n       ")
}

/// "03/08 at 14:12", from a timestamp in epoch seconds. UTC, like every other
/// timestamp the tool prints.
pub fn stored_on(taken_at: i64) -> String {
    format_epoch(taken_at, "earlier")
}

/// "04/08 at 16:30", from a cooldown end in epoch milliseconds.
pub fn cooldown_ends_at(until_ms: i64) -> String {
    format_epoch(cooldown_ends_at_secs(until_ms), "later")
}

/// A cooldown end in the unit every timestamp this tool reports uses.
///
/// `Pacer::cooldown` answers in milliseconds while `created_at` and
/// `validated_at` next to it in `whoami`'s object are in seconds, so something
/// has to convert — and both the printed date above and that JSON field are the
/// same cooldown, which is why they may not do their own arithmetic.
///
/// `div_euclid` rather than `/`, so a moment before the epoch floors instead of
/// rounding towards zero into the wrong second.
pub fn cooldown_ends_at_secs(until_ms: i64) -> i64 {
    until_ms.div_euclid(1000)
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
///
/// The outcome rather than a reason and a code picked out of it. The two have to
/// describe the same walk, and `ListOutcome::exit_code` exists precisely because
/// what Instagram said beats what the store recorded — a caller passing
/// `outcome.reason` next to a bare `ExitCode::from_stop_reason` would undo that
/// precedence while still compiling.
pub fn refuse_incomplete(
    list: ListKind,
    outcome: &crate::engine::ListOutcome,
    misreading: &str,
) -> anyhow::Error {
    ExitError::new(
        outcome.exit_code(),
        format!(
            "the {list} list could not be read in full, so the answer would be wrong: \
             the accounts missing from it would appear as if {misreading}."
        ),
    )
    .with_hint(try_again_advice(outcome.reason))
    .into()
}

/// Two stored lists too far apart to be crossed.
///
/// The wording and the code live here rather than in `engine` because that is
/// what AGENTS.md says: engine returns data and where the data came from, and
/// never decides how anything looks. It hands over the two provenances and the
/// two dates; which sentence and which code those deserve is this module's
/// question.
///
/// And they really do differ, in three ways rather than two:
///
/// - A **cooldown** is waited out, so "run it again later" is true and the
///   throttling code is right.
/// - **`--cache`** is the user's own doing, and dropping it is the fix.
/// - A **failed poll** is neither. Nobody asked for storage — the request to
///   check went out and did not come back — so advising them to drop a flag
///   they never typed sends them looking for something that is not there. This
///   arm used to fall in with `--cache` because the only question asked was
///   whether either side was a cooldown.
pub fn refuse_different_moments(
    a: Provenance,
    b: Provenance,
    a_at: i64,
    b_at: i64,
) -> anyhow::Error {
    // Each arm asks the same question of the same pair, so each one asks it the
    // same way. The cooldown arm used to go through a method of its own while its
    // sibling compared the variant inline, which left the next cause to need a
    // hint with two precedents and no reason to prefer either.
    let either = |wanted: Provenance| a == wanted || b == wanted;
    let (code, hint) = if either(Provenance::Cooldown) {
        (
            ExitCode::RateLimited,
            "Run it again once the cooldown lifts.",
        )
    } else if either(Provenance::CacheFlag) {
        (
            ExitCode::Error,
            "Run it again without --cache, so both lists are checked against the account.",
        )
    } else {
        (
            ExitCode::Error,
            "Instagram could not be reached to check either list. Run it again in a while.",
        )
    };

    ExitError::new(
        code,
        format!(
            "the two stored lists are from different moments ({} and {}), so crossing \
             them would invent results.",
            stored_on(a_at),
            stored_on(b_at)
        ),
    )
    .with_hint(hint)
    .into()
}

/// Why a cooldown could not be served around.
///
/// A plain description of the situation, handed over by `engine` so that this
/// module can choose the words.
#[derive(Debug, Clone, Copy)]
pub enum Blocked<'a> {
    /// `--refresh` was asked for, and walking is exactly what cannot happen.
    RefreshWanted,
    /// The named account has never been tracked, so there is nothing stored.
    AccountUnknown(&'a str),
    /// The account is known but this list has never been walked to the end.
    NothingStored(ListKind),
}

/// The account is in cooldown and storage cannot answer either.
pub fn refuse_in_cooldown(until_ms: i64, blocked: Blocked<'_>) -> anyhow::Error {
    let when = cooldown_ends_at(until_ms);
    let detail = match blocked {
        Blocked::RefreshWanted => {
            format!("the account is in cooldown until {when}; --refresh cannot walk until it lifts")
        }
        Blocked::AccountUnknown(name) => format!(
            "the account is in cooldown until {when}, and no list of @{name} is stored \
             to serve in the meantime"
        ),
        Blocked::NothingStored(kind) => format!(
            "the account is in cooldown until {when}, and no complete snapshot of the \
             {kind} list is stored, so there is nothing to serve"
        ),
    };
    ExitError::new(ExitCode::RateLimited, detail).into()
}

/// A cooldown that landed between the check and the walk.
pub fn refuse_cooldown_mid_walk(until_ms: i64) -> anyhow::Error {
    ExitError::new(
        ExitCode::RateLimited,
        format!(
            "the account is in cooldown until {}; nothing can be walked until it lifts",
            cooldown_ends_at(until_ms)
        ),
    )
    .into()
}

/// "@someone followers" — what a run is walking, said the same way by every
/// command that walks something.
pub fn walking(kind: ListKind, subject: &str) -> String {
    format!("{subject} {kind}")
}

/// "3 unfollowers", plus what took the others away when anything did.
///
/// Three counts, because two things can shorten a list and they are not the
/// same news. `total` is what the crossing produced, `kept` what survived the
/// filters, and `shown` what `--limit` left. Measuring only the first and the
/// last meant `--limit 3` on ten unfollowers with no filters at all reported
/// "3 unfollowers (of 10, the rest filtered out)" — nothing was filtered, and
/// `--limit`'s own help calls it a trim.
///
/// Both forms of the noun are handed in rather than an `s` being bolted on:
/// what gets counted here is a whole phrase — "accounts you follow that do not
/// follow you back" — whose singular differs by three words rather than by a
/// final letter. The count of one is not a rare case, either: it is what a
/// filtered list reaches most often.
pub fn counted(shown: usize, kept: usize, total: usize, one: &str, many: &str) -> String {
    let what = if shown == 1 { one } else { many };
    let mut line = format!("{shown} {what}");
    match (kept < total, shown < kept) {
        (false, false) => {}
        (true, false) => line.push_str(&format!(" (of {total}, the rest filtered out)")),
        (false, true) => line.push_str(&format!(" (of {kept}, trimmed by --limit)")),
        (true, true) => line.push_str(&format!(
            " (of {total}: {} filtered out, the rest trimmed by --limit)",
            total - kept
        )),
    }
    line
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
///
/// A dead session and a checkpoint are worse than a lie: running it again is
/// the one thing that cannot help, and against an account Instagram has just
/// flagged it is what turns a checkpoint into something longer.
pub fn try_again_advice(reason: StopReason) -> &'static str {
    match reason {
        StopReason::RateLimit => {
            "Run it again once the cooldown lifts; a walk stopped by throttling starts over."
        }
        StopReason::SessionInvalid => {
            "Deal with what Instagram asked for first. Running it again before that cannot get \
             any further."
        }
        // Nothing was left off. This reason is reached after the pagination has
        // already ended — `pager::verdict` reclassifies a walk that finished
        // far short of the declared count — so the snapshot closes with no
        // cursor, and `snapshots::resumable` requires one. There is nothing for
        // a second run to continue from, and it will usually be served the same
        // short list again.
        StopReason::Truncated => {
            "Instagram stopped serving this account's list; there is nothing to continue from, \
             so running it again starts over. Try later."
        }
        _ => "Run it again to continue where it left off.",
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
    use crate::engine::ListOutcome;

    /// A walk that stopped for `reason`, and what Instagram said about it when
    /// that is more specific than the reason.
    fn stopped(reason: StopReason, stopped_by: Option<ExitCode>) -> ListOutcome {
        ListOutcome {
            provenance: Provenance::Walked,
            reason,
            requests: 1,
            started_at: 0,
            taken_at: 0,
            account_pk: 1,
            stopped_by,
        }
    }

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
            &stopped(StopReason::RateLimit, None),
            "they did not follow you",
        );
        let text = error.to_string();
        assert!(text.contains("followers list"), "{text}");
        assert!(text.contains("they did not follow you"), "{text}");
        // The advice lives on the hint now, not in the sentence: see
        // `a_refusal_keeps_its_advice_apart_from_what_happened`.
        assert!(hint_of(&error).unwrap().contains("starts over"));

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
            // Nothing more specific than the stop reason, so the code comes off
            // the reason — which is `ListOutcome::exit_code`'s fallback.
            let error = refuse_incomplete(ListKind::Following, &stopped(reason, None), "whatever");
            assert_eq!(ExitCode::from_chain(&error), Some(expected), "{reason:?}");
        }
    }

    /// `SessionInvalid` is what the store records for both "log in again" and
    /// "Instagram wants the account verified", and those are different codes.
    /// The caller is allowed to know better than the stop reason does.
    #[test]
    fn a_challenge_keeps_its_own_code_under_a_session_invalid_stop() {
        let error = refuse_incomplete(
            ListKind::Followers,
            &stopped(StopReason::SessionInvalid, Some(ExitCode::Challenge)),
            "whatever",
        );
        assert_eq!(ExitCode::from_chain(&error), Some(ExitCode::Challenge));
    }

    /// Against an account Instagram has just flagged, "run it again" is the
    /// one piece of advice that cannot help and can make it worse.
    #[test]
    fn a_dead_session_is_not_told_to_try_again() {
        let advice = try_again_advice(StopReason::SessionInvalid);
        assert!(!advice.contains("continue where it left off"), "{advice}");
        assert!(advice.contains("Instagram asked for"), "{advice}");
    }

    fn hint_of(error: &anyhow::Error) -> Option<String> {
        error
            .chain()
            .find_map(|c| c.downcast_ref::<ExitError>())
            .and_then(ExitError::hint)
            .map(str::to_string)
    }

    /// The advice is separate from the failure, so the printer can label them
    /// differently. Glued together with a newline, both came out under
    /// `error:` and the advice read as more of the complaint.
    #[test]
    fn a_refusal_keeps_its_advice_apart_from_what_happened() {
        let error = refuse_incomplete(
            ListKind::Followers,
            &stopped(StopReason::RateLimit, None),
            "they did not follow you",
        );
        assert!(error.to_string().contains("would be wrong"), "{error}");
        assert!(
            !error.to_string().contains("Run it again"),
            "the advice is not part of what happened: {error}"
        );
        assert!(hint_of(&error).unwrap().contains("starts over"));
    }

    /// A cooldown is waited out and the other two causes are not, so the same
    /// refusal owes them different advice. Telling somebody to sit out a
    /// cooldown they are not in is worse than saying nothing.
    #[test]
    fn different_moments_are_explained_by_why_nobody_checked() {
        let throttled = refuse_different_moments(Provenance::Cooldown, Provenance::Cooldown, 0, 1);
        assert!(hint_of(&throttled).unwrap().contains("cooldown lifts"));
        assert_eq!(
            ExitCode::from_chain(&throttled),
            Some(ExitCode::RateLimited)
        );

        let asked_for =
            refuse_different_moments(Provenance::CacheFlag, Provenance::CacheFlag, 0, 1);
        let hint = hint_of(&asked_for).unwrap();
        assert!(hint.contains("--cache"), "{hint}");
        assert!(!hint.contains("cooldown"), "{hint}");
        assert_eq!(ExitCode::from_chain(&asked_for), Some(ExitCode::Error));

        // A failed poll is neither of the two. Nobody asked for storage — the
        // request to check went out and did not come back — so it used to be
        // told to drop a flag it never passed, which sends somebody looking for
        // something that is not in their command line.
        let nobody_could_check =
            refuse_different_moments(Provenance::PollFailed, Provenance::PollFailed, 0, 1);
        let hint = hint_of(&nobody_could_check).unwrap();
        assert!(!hint.contains("--cache"), "{hint}");
        assert!(!hint.contains("cooldown"), "{hint}");
        assert_eq!(
            ExitCode::from_chain(&nobody_could_check),
            Some(ExitCode::Error)
        );

        // All three say the same thing about what happened; only the advice
        // differs.
        for error in [&throttled, &asked_for, &nobody_could_check] {
            assert!(error.to_string().contains("different moments"), "{error}");
        }
    }

    /// Every way a cooldown can leave the user with nothing says which one it
    /// was, and all of them carry the throttling code.
    #[test]
    fn a_cooldown_refusal_names_what_is_missing() {
        let cases = [
            (Blocked::RefreshWanted, "--refresh"),
            (Blocked::AccountUnknown("someone"), "@someone"),
            (Blocked::NothingStored(ListKind::Followers), "followers"),
        ];
        for (blocked, expected) in cases {
            let error = refuse_in_cooldown(1_722_700_000_000, blocked);
            let text = error.to_string();
            assert!(text.contains(expected), "{text}");
            assert!(text.contains("in cooldown until"), "{text}");
            assert_eq!(ExitCode::from_chain(&error), Some(ExitCode::RateLimited));
        }
    }

    /// A refusal is several sentences. Under a bare `error:` only the first one
    /// carried the label and the rest started at column zero, reading as
    /// separate unattributed text.
    #[test]
    fn continuation_lines_sit_under_the_label() {
        let indented = indented(
            "first
second
third",
        );
        assert_eq!(
            indented,
            "first
       second
       third"
        );
        // A single line is left exactly as it was.
        assert_eq!(super::indented("only one"), "only one");
    }

    /// Filters that took nothing out must not leave a clause saying they did.
    #[test]
    fn the_count_only_mentions_filtering_when_something_was_filtered() {
        assert_eq!(
            counted(3, 3, 3, "unfollower", "unfollowers"),
            "3 unfollowers"
        );
        assert_eq!(
            counted(3, 3, 10, "unfollower", "unfollowers"),
            "3 unfollowers (of 10, the rest filtered out)"
        );
    }

    /// A cap is not a filter. `--limit 3` on ten unfollowers with no filters
    /// at all used to report that the other seven were filtered out.
    #[test]
    fn a_cap_is_reported_as_a_cap() {
        assert_eq!(
            counted(3, 10, 10, "unfollower", "unfollowers"),
            "3 unfollowers (of 10, trimmed by --limit)"
        );
        // And when both happened, both are named.
        assert_eq!(
            counted(2, 4, 10, "unfollower", "unfollowers"),
            "2 unfollowers (of 10: 6 filtered out, the rest trimmed by --limit)"
        );
    }

    /// A filtered list lands on one result often enough that "1 accounts you
    /// follow that do not follow you back" was the summary users saw most.
    #[test]
    fn a_count_of_one_reads_as_one() {
        assert_eq!(
            counted(1, 1, 1, "unfollower", "unfollowers"),
            "1 unfollower"
        );
        assert_eq!(
            counted(1, 1, 9, "unfollower", "unfollowers"),
            "1 unfollower (of 9, the rest filtered out)"
        );
        assert_eq!(
            counted(0, 0, 0, "unfollower", "unfollowers"),
            "0 unfollowers"
        );
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
        // These three leave a cursor behind, so there is something to continue.
        for reason in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Network,
        ] {
            assert!(try_again_advice(reason).contains("continue where it left off"));
        }
    }

    /// A truncated walk has nothing to continue from, so it must not say it
    /// has.
    ///
    /// `pager::verdict` reaches this reason **after** the pagination has ended,
    /// by reclassifying a walk that finished far short of the declared count.
    /// The snapshot therefore closes with no cursor, and
    /// `snapshots::resumable` will not return a row without one — so the offer
    /// to continue was for a walk that could never be found again.
    #[test]
    fn a_truncated_walk_does_not_promise_to_continue() {
        let advice = try_again_advice(StopReason::Truncated);
        assert!(!advice.contains("continue where it left off"), "{advice}");
        assert!(advice.contains("starts over"), "{advice}");
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
