//! The sentences the commands share.
//!
//! Small, but worth one home: these phrases are the ones a user compares
//! between commands, and two copies of "run it again" that drifted apart would
//! read as two different pieces of advice about the same situation.

use snob_core::model::{ListKind, StopReason, User, printable};

use crate::engine::Provenance;

use crate::exit::{ExitCode, ExitError};

/// What a machine with no `watch.toml` is told, by both things that look.
///
/// `engine::check::without_a_session` decides it for `snob watch check` and
/// `watch_setup::health` decides it for `snob watch status`, and the two spelled
/// it out separately, character for character. It is the advice a newly
/// installed tool gives, so it is the sentence somebody edits — and an edit to
/// one copy leaves two probes a person runs one after the other saying different
/// things about the same machine, each with a test asserting it is right.
pub const NOTHING_CONFIGURED: &str =
    "nothing is configured, so a bare \"snob watch\" has no schedule to run on";

/// What a missing consent means for a run with nobody at the keyboard.
///
/// Two lines in `engine::check` describe this one condition — the account that
/// was polled, and the account nothing was asked about because a cooldown was
/// standing, whose sentence is this one with a parenthetical after it. It is the
/// reason `snob watch` refuses to start at all, so it is worth exactly one
/// wording.
///
/// What it deliberately does not do is say what to do about it.
/// `commands::watch::refuse_unattended` is the sentence that names
/// `snob watch setup`, and that is the refusal itself rather than a report about
/// one.
///
/// A `const` and not a typed reason on `Checked`. The wording being decided
/// inside `engine` is the architecture rule, and moving it out is the right
/// shape — but `Checked::problem` is a pass-through for whatever the schedule
/// parser, Instagram or the user's own server said, so a typed reason needs a
/// free-string variant anyway and every consumer still handles one. That trade
/// is worth revisiting the day `problem` stops carrying foreign text; it is not
/// worth nine sentences and a byte-identity promise across two renderers today.
pub const NO_RECORDED_CONSENT: &str =
    "no recorded consent, so an unattended run will refuse to read it";

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
    eprint!("{}", rendered(error));
}

/// The block `print_error` writes, built rather than printed.
///
/// Separate so a test can read it. Printing was four `eprintln!` calls and one
/// early `return`, and the `return` was the branch that skipped the filter:
/// there is no way to catch that from outside the process, so the way to catch
/// it is to have something to assert on.
fn rendered(error: &anyhow::Error) -> String {
    let code = ExitCode::from_chain(error);
    if code == Some(ExitCode::Interrupted) {
        // No label, so nothing to indent under — but the filter is not the
        // label's business. It applies here for the same reason it applies
        // below: the sentence names an account, and the name came from
        // `watch.toml` or from a terminal, not from this program.
        return format!("{}\n", filtered(&error.to_string(), "\n"));
    }

    let label = console::style("error:").red().bold().for_stderr();
    let mut out = format!("{label} {}\n", indented(&error.to_string()));

    for cause in error.chain().skip(1) {
        let caused = console::style("caused by:").dim().for_stderr();
        out.push_str(&format!("  {caused} {}\n", indented(&cause.to_string())));
    }

    if let Some(hint) = error
        .chain()
        .find_map(|c| c.downcast_ref::<ExitError>())
        .and_then(ExitError::hint)
    {
        let label = console::style("hint:").cyan().bold().for_stderr();
        out.push_str(&format!("{label}  {}\n", indented(hint)));
    }

    out
}

/// Lines after the first start under the label rather than at column zero.
///
/// Deliberately not re-wrapped to the terminal width: the only hard breaks in
/// these strings are the ones somebody put between sentences, and re-wrapping
/// would eventually split a URL or `snob login --paste` across a line. The
/// terminal already soft-wraps at the width it really has.
///
fn indented(text: &str) -> String {
    filtered(text, "\n       ")
}

/// The filter every string this module prints goes through, which makes it the
/// boundary the rule asks for: a name is filtered before anything draws it,
/// whoever it came from. Each of these messages is built by interpolating
/// something into a sentence, and each interpolation was one more place to
/// remember — `checked_url` and `Blocked::AccountUnknown` were both forgotten.
///
/// **Line by line, not over the whole string.** `printable` turns any
/// whitespace into a space, newlines included, so filtering the text whole
/// would collapse the deliberate paragraph breaks [`indented`] exists to lay
/// out. Split first and the real breaks survive while an escape sequence
/// injected into a name does not.
///
/// `join` is how the caller puts the lines back together: under the label for
/// a reported failure, and with a bare newline where there is no label to
/// indent under.
fn filtered(text: &str, join: &str) -> String {
    text.split('\n')
        .map(printable)
        .collect::<Vec<_>>()
        .join(join)
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
    .with_hint(try_again_advice(outcome.reason, outcome.resumable))
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
        // Filtered here rather than at the call site, so every caller of the
        // variant gets it. `engine::cooldown` passes `target::clean`'s answer,
        // which only strips a leading `@` — and that name reaches this without
        // anybody typing it, because `Watched::list_args` puts the one from
        // `watch.toml` straight into `ListArgs.target` and nothing validates a
        // username there.
        Blocked::AccountUnknown(name) => format!(
            "the account is in cooldown until {when}, and no list of @{} is stored \
             to serve in the meantime",
            printable(name)
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
///
/// Everything else is told by `resumable`, which the store answered rather than
/// this module guessing from the reason. `Truncated` used to be treated as
/// proof that nothing was left to continue from, and that is true of exactly
/// one of the five ways it arrives: the reclassification `verify_completion`
/// makes
/// once pagination has already ended, where there is no cursor to save. The
/// other four — the hard page cap, a cursor that came back unchanged, two empty
/// pages, and several pages with nothing new — stop in the **middle** of the
/// pagination with a cursor stored, and a run within the resume window
/// continues from it. So the advice asks instead of assuming.
pub fn try_again_advice(reason: StopReason, resumable: bool) -> &'static str {
    match reason {
        StopReason::RateLimit => {
            "Run it again once the cooldown lifts; a walk stopped by throttling starts over."
        }
        StopReason::SessionInvalid => {
            "Deal with what Instagram asked for first. Running it again before that cannot get \
             any further."
        }
        StopReason::Truncated if !resumable => {
            "Instagram stopped serving this account's list; there is nothing to continue from, \
             so running it again starts over. Try later."
        }
        StopReason::Truncated => {
            "Instagram stopped serving pages. Run it again in the next few minutes and it \
             continues from where it stopped; after that it starts over."
        }
        _ if !resumable => {
            "Run it again; there is nothing stored to continue from, so it starts over."
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

/// Nothing stored to answer with, and `--cache` said not to look.
///
/// Two situations reach this: the account has never been seen at all, and the
/// account is known but this list of it has never been walked. They get the
/// same answer because there is one thing to do about either — and they were
/// written out separately, character for character, in two functions of
/// `engine`. Reword one and they become two explanations of one situation with
/// nothing comparing them.
///
/// It carries the `hint:` its four siblings here carry. It gains no exit code:
/// the `anyhow` fallback is already `Error`, and this is the shape that keeps
/// the advice apart from what happened.
pub fn refuse_nothing_stored(kind: ListKind) -> anyhow::Error {
    ExitError::new(
        ExitCode::Error,
        format!("no {kind} list is stored, and --cache says not to look for one"),
    )
    .with_hint(format!("run \"snob {kind}\" once, or drop --cache"))
    .into()
}

/// "pepito, carlos and 4 others", or `None` when there is nobody to name.
///
/// The cap is not about width. Past a handful the line stops being "people you
/// know" and becomes a list, and a list is what the `friends` command is for.
///
/// It lives here rather than in `engine` because it is a finished English
/// sentence: it prefixes each name with `@`, joins with commas, swaps the last
/// separator for "and" and picks between "1 other" and "N others". Wording is
/// what `report` holds and what `engine` does not, and the drift had already
/// started — with the count as a separate clause the line read "@ana, @luis and
/// @eva and 2 others", two lists stapled together, because only one of the two
/// joins knew about the other. The count is the last item now.
pub fn name_a_few(people: &[User], cap: usize) -> Option<String> {
    // A cap of zero would name nobody and count everybody, which is not a
    // sentence anyone wants to read. At least one name, always.
    let shown = cap.max(1).min(people.len());
    if shown == 0 {
        return None;
    }

    // Filtered here rather than at each consumer: this line is the first thing
    // `snob scan` prints, with no flag needed, and it also goes into the
    // markdown summary, whose escaping is about table cells rather than about
    // what a terminal obeys.
    let mut parts: Vec<String> = people[..shown]
        .iter()
        .map(|u| format!("@{}", u.safe_username()))
        .collect();
    parts.extend(match people.len() - shown {
        0 => None,
        1 => Some("1 other".to_string()),
        n => Some(format!("{n} others")),
    });

    Some(match parts.as_slice() {
        [one] => one.clone(),
        // The last one joins with "and" rather than a comma, because this is a
        // sentence rather than a column.
        [start @ .., last] => format!("{} and {last}", start.join(", ")),
        [] => unreachable!("the empty case returned above"),
    })
}

/// A schedule as clauses, ready to be joined into a sentence.
///
/// Two places read a schedule back to a person — the banner a run opens with,
/// and the summary `status` and `setup` print — and both built these four
/// clauses out of the same four fields, down to the quoting around the cron
/// expression and the `printable` on each one. They disagreed only on how to
/// name the cron clause, which is exactly the kind of drift that makes a reader
/// wonder whether the two are describing the same thing.
///
/// The caller supplies the verb, because "Runs …" and "Running …" are the
/// difference between a description and an announcement.
pub fn schedule_clauses(
    every: Option<std::time::Duration>,
    on: &[String],
    at: &[String],
    cron: Option<&str>,
) -> Vec<String> {
    let mut clauses = Vec::new();
    if let Some(every) = every {
        clauses.push(format!("every {}", snob_core::duration::format(every)));
    }
    if !on.is_empty() {
        clauses.push(format!("on {}", printable(&on.join(", "))));
    }
    if !at.is_empty() {
        clauses.push(format!("at {}", printable(&at.join(", "))));
    }
    if let Some(cron) = cron {
        clauses.push(format!("on the schedule \"{}\"", printable(cron)));
    }
    clauses
}

/// What the jitter does, or nothing at all when there is none.
///
/// **The wording is shared and the number is not.** The banner reports
/// `Schedule::jitter()`, which is clamped to what the calendar can absorb; the
/// summary reports what the file says. Those are two different facts about the
/// same setting, and a reader comparing them is entitled to see both — so each
/// caller passes its own.
pub fn jitter_sentence(jitter: std::time::Duration) -> Option<String> {
    (!jitter.is_zero()).then(|| {
        format!(
            "Each run is pushed up to {} later, so it does not land on the same second every \
             time.",
            snob_core::duration::format(jitter)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ListOutcome;

    /// Every string this module draws goes through the name filter, and the
    /// paragraph breaks it lays out survive it.
    ///
    /// `printable` turns any whitespace into a space, newlines included, so
    /// filtering a refusal whole would collapse the very structure `indented`
    /// exists to produce. Line by line, both hold.
    #[test]
    fn the_layout_survives_the_filter_and_an_escape_sequence_does_not() {
        let hostile = "first line\u{1b}[2K\u{1b}[A\nsecond line";
        let out = indented(hostile);

        assert!(
            !out.chars().any(|c| c.is_control() && c != '\n'),
            "{out:?} reaches a terminal"
        );
        assert_eq!(
            out.lines().count(),
            2,
            "the deliberate break between sentences is not the filter's business: {out:?}"
        );
        assert!(out.starts_with("first line"));
        assert!(out.trim_end().ends_with("second line"));
    }

    fn people(names: &[&str]) -> Vec<User> {
        names
            .iter()
            .enumerate()
            .map(|(i, name)| User {
                pk: i as u64 + 1,
                username: (*name).into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            })
            .collect()
    }

    #[test]
    fn nobody_is_not_a_sentence() {
        assert_eq!(name_a_few(&[], 3), None);
    }

    /// This line opens `snob scan` with no flag asked for, so a username is
    /// the shortest route from somebody else's profile to the terminal.
    #[test]
    fn a_hostile_name_cannot_drive_the_terminal() {
        let hostile = people(&["ana\u{1b}[2K", "lu\u{202e}is"]);
        let line = name_a_few(&hostile, 3).unwrap();
        assert!(!line.contains('\u{1b}'), "{line:?}");
        assert!(!line.contains('\u{202e}'), "{line:?}");
        assert_eq!(line, "@ana[2K and @luis");
    }

    #[test]
    fn one_name_stands_alone() {
        assert_eq!(name_a_few(&people(&["ana"]), 3).unwrap(), "@ana");
    }

    #[test]
    fn the_last_one_joins_with_and() {
        assert_eq!(
            name_a_few(&people(&["ana", "luis"]), 3).unwrap(),
            "@ana and @luis"
        );
        assert_eq!(
            name_a_few(&people(&["ana", "luis", "eva"]), 3).unwrap(),
            "@ana, @luis and @eva"
        );
    }

    /// The count is the last item of the one list, not a second list after it.
    #[test]
    fn past_the_cap_the_rest_are_counted() {
        let five = people(&["ana", "luis", "eva", "juan", "sara"]);
        assert_eq!(
            name_a_few(&five, 3).unwrap(),
            "@ana, @luis, @eva and 2 others"
        );
        assert_eq!(
            name_a_few(&five, 4).unwrap(),
            "@ana, @luis, @eva, @juan and 1 other"
        );
        // Exactly at the cap nothing is left over to count.
        assert_eq!(
            name_a_few(&five, 5).unwrap(),
            "@ana, @luis, @eva, @juan and @sara"
        );
    }

    /// A run somebody stopped gets no label, and for a while that meant it got
    /// no filter either: the branch printed the error and returned above
    /// everything else. The refusal below is the one a scheduled run raises,
    /// and the name in it comes from `watch.toml`, where nothing validates a
    /// username.
    #[test]
    fn a_stopped_run_is_filtered_even_though_it_carries_no_label() {
        let hostile = "gh\u{1b}[2K\u{1b}[A";
        let error: anyhow::Error = ExitError::new(
            ExitCode::Interrupted,
            format!(
                "reading @{hostile}'s lists needs confirmation, and a scheduled run has \
                 nobody to ask.\nRun \"snob watch setup\" to answer it once."
            ),
        )
        .into();

        let out = rendered(&error);

        assert!(
            !out.chars().any(|c| c.is_control() && c != '\n'),
            "{out:?} reaches a terminal"
        );
        assert!(out.contains("@gh"), "the name is still shown: {out}");
        assert!(
            !out.contains("error:"),
            "stopping a run is not a failure to report: {out}"
        );
        // No label above it, so the second sentence stays at column zero.
        let mut lines = out.lines();
        assert!(lines.next().is_some_and(|l| l.starts_with("reading @gh")));
        assert_eq!(
            lines.next(),
            Some("Run \"snob watch setup\" to answer it once.")
        );
        assert_eq!(lines.next(), None);
    }

    /// An account name reaches the cooldown refusal without anybody typing it:
    /// `Watched::list_args` puts the one from `watch.toml` straight into
    /// `ListArgs.target`, and nothing validates a username there.
    #[test]
    fn a_cooldown_refusal_cannot_be_made_to_erase_the_line_above_it() {
        let name = "gh\u{1b}[2K\u{1b}[A";
        let error = refuse_in_cooldown(1_000, Blocked::AccountUnknown(name));
        let message = error.to_string();

        assert!(
            !message.chars().any(|c| c.is_control()),
            "{message:?} is printed to a terminal"
        );
        assert!(
            message.contains("@gh"),
            "the name is still shown: {message}"
        );
    }

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
            snapshot_id: 1,
            stopped_by,
            resumable: false,
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
        // Both ways: a checkpoint can land mid-pagination with a cursor stored,
        // and continuing is still the wrong thing to offer.
        for resumable in [false, true] {
            let advice = try_again_advice(StopReason::SessionInvalid, resumable);
            assert!(!advice.contains("continue where it left off"), "{advice}");
            assert!(advice.contains("Instagram asked for"), "{advice}");
        }
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
        assert!(try_again_advice(StopReason::RateLimit, false).contains("starts over"));
        // These three stop mid-pagination, so the store has a cursor.
        for reason in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Network,
        ] {
            assert!(try_again_advice(reason, true).contains("continue where it left off"));
        }
    }

    /// The offer to continue follows what the store kept, not what the reason
    /// suggests.
    ///
    /// Every reason here can arrive either way. A `Canceled` walk normally has
    /// a cursor, but one interrupted more than fifteen minutes after it began
    /// has already aged out of the resume window — and the advice must not
    /// offer a continuation the next run cannot make.
    /// The two ways this module offers one. Both are matched, because saying
    /// "nothing to continue from" also contains the word.
    fn offers_a_continuation(advice: &str) -> bool {
        advice.contains("continue where it left off") || advice.contains("continues from")
    }

    #[test]
    fn nothing_is_offered_to_continue_when_there_is_nothing_stored() {
        for reason in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Network,
            StopReason::Truncated,
        ] {
            let advice = try_again_advice(reason, false);
            assert!(
                !offers_a_continuation(advice),
                "{reason:?} offered a continuation: {advice}"
            );
            assert!(advice.contains("starts over"), "{reason:?}: {advice}");
        }
    }

    /// The reclassified truncation is the one that really has nothing left.
    ///
    /// `verify_completion` reaches it **after** the pagination has ended, so the
    /// snapshot closes with no cursor and `snapshots::resumable` will not
    /// return a row without one. The four guards that stop in the middle of the
    /// pagination do leave one, which is why the reason alone cannot answer
    /// this and `resumable` is asked separately.
    #[test]
    fn a_truncated_walk_says_which_of_the_two_it_was() {
        let ended = try_again_advice(StopReason::Truncated, false);
        assert!(!offers_a_continuation(ended), "{ended}");
        assert!(ended.contains("starts over"), "{ended}");

        let stopped_short = try_again_advice(StopReason::Truncated, true);
        assert!(stopped_short.contains("continues from where it stopped"));
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
