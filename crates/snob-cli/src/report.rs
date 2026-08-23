//! The sentences the commands share.
//!
//! Small, but worth one home: these phrases are the ones a user compares
//! between commands, and two copies of "run it again" that drifted apart would
//! read as two different pieces of advice about the same situation.

use snob_core::model::{ListKind, StopReason, User, printable};
use snob_core::{Epoch, EpochMs};
use snob_ig::pager::Warning;

use crate::app::ConsentInAdvance;
use crate::engine::Provenance;

use crate::exit::{ExitCode, ExitError};

/// What a machine with no `watch.toml` is told, by both things that look.
///
/// `snob watch check` reaches it through `commands::watch::say::problem_line`
/// and `watch::status::health` reads it directly, and the two spelled it out
/// separately, character for character. It is the advice a newly installed tool
/// gives, so it is the sentence somebody edits — and an edit to one copy leaves
/// two probes a person runs one after the other saying different things about
/// the same machine, each with a test asserting it is right.
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
/// `commands::watch::scheduled::refuse_unattended` is the sentence that names
/// `snob watch setup`, and that is the refusal itself rather than a report about
/// one.
///
/// A `const` here rather than a sentence inside `engine::check`, which is where
/// both of these lines used to be written. `Checked::problem` is
/// [`crate::engine::check::Problem`] now — a variant per decided reason,
/// rendered by `commands::watch::say::problem_line` — so this is what that
/// renderer and `watch::status::health` share.
///
/// What held the typed reason up was `Problem::Foreign`: some of those lines
/// carry text this program did not write, so a typed reason needs a free-string
/// variant whatever else it has, and the trade looked like nine sentences and a
/// byte-identity promise across two renderers for one variant less. It is one
/// variant among twelve now rather than a reason for the other eleven to be
/// strings, and the two renderers are the point rather than the price: the
/// terminal report and `check --json` agreed by copying a string and agree by
/// construction instead.
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
/// decides on stdout's color state, so the labels lose their color when only
/// stdout is redirected, and write escape codes into the file when only stderr
/// is.
pub fn print_error(error: &anyhow::Error, wording: Wording) {
    match wording {
        Wording::Prose => eprint!("{}", rendered(error)),
        Wording::Json => eprintln!("{}", error_json(error)),
    }
}

/// How a failure is told: to a person, or to a program.
///
/// A run whose result was going to be JSON had its failure written as
/// English prose, so a script reading `snob followers --format json` got a
/// stable exit code and nothing else it could parse -- not the hint, not the
/// challenge address a code 4 carries. `whoami --json` already answered in
/// JSON on failure; this makes every command do the same, and makes the
/// decision `main`'s, from the format the command was going to answer in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wording {
    Prose,
    Json,
}

/// The failure as one JSON object, for a caller that asked for JSON.
///
/// One shape for every command: `code` is the same token the exit status
/// names, so a reader that only has the stream and a reader that only has
/// `$?` are told the same thing; `message` and `causes` are the chain the
/// prose prints, in the same order; `hint` is the advice the prose sets
/// apart; `url` is the address a challenge has to be cleared at. Filtered
/// line by line like the prose, and for the same reason -- a name is
/// filtered before anything draws it, and a JSON consumer may well print it.
pub fn error_json(error: &anyhow::Error) -> serde_json::Value {
    let code = crate::exit::exit_code_for(error);
    let mut chain = error
        .chain()
        .map(|cause| filtered(&cause.to_string(), "\n"));
    let message = chain.next().unwrap_or_default();
    let causes: Vec<String> = chain.collect();
    let instagram = error
        .chain()
        .find_map(|c| c.downcast_ref::<snob_ig::error::IgError>());
    // The same fallback `rendered` applies, so the two shapes cannot disagree
    // about whether there was any advice — which is the whole reason this
    // function exists rather than a second rendering.
    let hint = error
        .chain()
        .find_map(|c| c.downcast_ref::<ExitError>())
        .and_then(ExitError::hint)
        .or_else(|| instagram.and_then(advice_for))
        .map(|hint| filtered(hint, "\n"));
    let url = instagram.and_then(|e| e.challenge_url());
    let lifts = instagram.and_then(|e| match e {
        snob_ig::error::IgError::InCooldown { until_ms } => Some(until_ms.to_epoch()),
        _ => None,
    });

    serde_json::json!({
        "error": {
            "code": code.as_str(),
            "exit": code as u8,
            "message": message,
            "causes": causes,
            "hint": hint,
            "url": url,
            "cooldown_until": lifts,
        }
    })
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

    // The command's own advice first, and Instagram's client's only when the
    // command had none. `IgError`'s two pieces of advice used to be part of its
    // message, so they arrived whatever else was in the chain; putting them on
    // the hint means deciding which one line the reader gets, and a command
    // that has written advice for this exact situation knows more than a
    // variant does. The two cannot in fact meet — `ExitError` carries no
    // source, so nothing of Instagram's is ever underneath one — but the order
    // is written down rather than left to that, and `error_json` reads it the
    // same way so the two shapes cannot come to disagree.
    if let Some(hint) = error
        .chain()
        .find_map(|c| c.downcast_ref::<ExitError>())
        .and_then(ExitError::hint)
        .or_else(|| {
            error
                .chain()
                .find_map(|c| c.downcast_ref::<snob_ig::error::IgError>())
                .and_then(advice_for)
        })
    {
        let label = console::style("hint:").cyan().bold().for_stderr();
        out.push_str(&format!("{label}  {}\n", indented(hint)));
    }

    // The backstop in `Pacer::clear` answers with the epoch and no wording,
    // because when a cooldown lifts is a date and `snob-ig` has no business
    // formatting one. Said here so the last resort tells a person the same thing
    // the eight gates in front of it tell them; every path anybody really
    // reaches names the date itself, so this fires only when one of them was
    // forgotten — which is exactly when the person reading needs it most.
    if let Some(snob_ig::error::IgError::InCooldown { until_ms }) = error
        .chain()
        .find_map(|c| c.downcast_ref::<snob_ig::error::IgError>())
    {
        let label = console::style("hint:").cyan().bold().for_stderr();
        out.push_str(&format!(
            "{label}  {}\n",
            indented(&format!("it lifts {}", cooldown_ends_at(*until_ms)))
        ));
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

/// The requests half of a summary line: what was spent, or the promise that
/// nothing was.
///
/// Two commands wrote the `if` themselves — the single lists and the
/// crossings — and "without touching the network" is exactly the kind of
/// sentence this module exists to keep from drifting into two ways of
/// describing one situation.
pub fn spent(n: u32) -> String {
    if n > 0 {
        format!(" - {}", requests(n))
    } else {
        " - without touching the network".to_string()
    }
}

/// The date a crossing's summary names: the older of the two captures,
/// because a crossing is only as recent as its staler half.
///
/// `check_same_moment` is what stops the two being far apart at all, so this
/// is completeness rather than a correction. The rule was written out twice,
/// comment and all, in `sets` and `scan`.
pub fn stored_on_the_older_of(a: snob_core::Epoch, b: snob_core::Epoch) -> String {
    stored_on(a.min(b))
}

/// "Aug 3 at 14:12", in the local zone like every other moment the tool prints.
pub fn stored_on(taken_at: Epoch) -> String {
    format_epoch(taken_at, "earlier")
}

/// "Aug 3, 2024", for a moment that may be years old.
///
/// [`stored_on`] carries no year because everything it dates — a capture, a
/// story, a cooldown — is at most days away and the hour is the informative
/// part. A highlight is the opposite: kept for years on purpose, so "Aug 3"
/// alone would read as this year and be wrong most of the time, and the
/// hour of a moment years back says nothing worth a column.
pub fn dated(at: Epoch) -> String {
    chrono::DateTime::from_timestamp(at.get(), 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%b %-d, %Y")
                .to_string()
        })
        .unwrap_or_else(|| "sometime".to_string())
}

/// "04/08 at 16:30", from a cooldown end.
///
/// The conversion to seconds is [`EpochMs::to_epoch`] and is not written here.
/// It used to be — `cooldown_ends_at_secs`, in this file — and the printed date
/// and `whoami`'s JSON field are the same cooldown, so the arithmetic belongs
/// with the type rather than one crate out from it.
pub fn cooldown_ends_at(until_ms: EpochMs) -> String {
    format_epoch(until_ms.to_epoch(), "later")
}

/// A moment, for a person to read.
///
/// **The month is named rather than numbered, and that is the whole of the
/// decision.** This printed `%d/%m` — the Spanish original's convention,
/// surviving the July 2026 translation — in a tool whose standing rule is
/// English with US spelling, and whose audience therefore reads `03/08` as the
/// eighth of March. It is the one formatting decision in this file that had no
/// reasoning written at it, which is presumably why it survived. Naming the
/// month removes the ambiguity for everybody instead of moving it from one half
/// of the readership to the other; `%-d` rather than `%d` because "Aug 3" is
/// how the date is said.
///
/// **In the local zone**, because everything else the person reads is. The
/// schedule is evaluated in `chrono::Local`, the README says "times are
/// your local ones", and a cooldown "until 15:46" is read against the clock
/// on the wall -- while this printed UTC with no label, so a monitor on
/// `--at 09:00` in Madrid reported having "last run on Aug 22 at 07:00",
/// and a cooldown ending at four looked like it ended at two. The zone is
/// taken at the call, so the test can pin the arithmetic with a fixed one.
fn format_epoch(at: Epoch, unknown: &str) -> String {
    format_epoch_in(at, unknown, &chrono::Local)
}

fn format_epoch_in<Z: chrono::TimeZone>(at: Epoch, unknown: &str, zone: &Z) -> String
where
    Z::Offset: std::fmt::Display,
{
    chrono::DateTime::from_timestamp(at.get(), 0)
        .map(|t| t.with_timezone(zone).format("%b %-d at %H:%M").to_string())
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
/// - **`--offline`** is the user's own doing, and dropping it is the fix.
/// - A **failed poll** is neither. Nobody asked for storage — the request to
///   check went out and did not come back — so advising them to drop a flag
///   they never typed sends them looking for something that is not there. This
///   arm used to fall in with `--offline` because the only question asked was
///   whether either side was a cooldown.
pub fn refuse_different_moments(
    a: Provenance,
    b: Provenance,
    a_at: Epoch,
    b_at: Epoch,
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
            "Run it again without --offline, so both lists are checked against the account.",
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
pub fn refuse_in_cooldown(until_ms: EpochMs, blocked: Blocked<'_>) -> anyhow::Error {
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
pub fn refuse_cooldown_mid_walk(until_ms: EpochMs) -> anyhow::Error {
    ExitError::new(
        ExitCode::RateLimited,
        format!(
            "the account is in cooldown until {}; nothing can be walked until it lifts",
            cooldown_ends_at(until_ms)
        ),
    )
    .into()
}

/// What somebody is agreeing to when they let a run read a stranger's lists.
///
/// Three facts about the request rather than about the account, which is why
/// there is one of these rather than one per target.
pub const READING_SOMEBODY_ELSES_LIST: &str = "this reads a list that belongs to somebody else, and lands their followers \
     in your local database. It is also a heavier request than reading your own, \
     and Instagram is readier to refuse it";

/// The consent question, with the account named the way the warning above
/// named it.
pub fn ask_to_continue(shown: &str) -> String {
    format!("Continue with {shown}?")
}

/// Being unable to ask and being told no are two different events, and this is
/// the first one, shaped the same way everywhere a question finds no terminal:
/// what was not done, why nothing could be asked, and how to answer in
/// advance. Exit 130, which is what the README's table and `--help` both
/// promise for a confirmation that was not given.
///
/// The advice rides in the message rather than on a hint, and that is
/// load-bearing: [`rendered`] returns early for this exit code, so a hint set
/// on an interrupted error is advice nobody is ever shown — which is exactly
/// what happened to `follow`'s for as long as it carried one. Three commands
/// wrote this sentence themselves, each a little differently, before it lived
/// here; the scheduled monitor keeps its own variant, because "a scheduled run
/// has nobody to ask" is a different fact from "there is no terminal".
pub fn refuse_unattended(refused: String, in_advance: String) -> anyhow::Error {
    ExitError::new(
        ExitCode::Interrupted,
        format!("{refused}, and there is no terminal to ask at. {in_advance}"),
    )
    .into()
}

/// Nobody is there to be asked, so nothing is enumerated.
///
/// Which way to answer in advance is the **caller's** fact rather than this
/// sentence's, and it arrives as [`ConsentInAdvance`]. Both commands that reach
/// here take an answer beforehand and they do not take it the same way, and one
/// sentence named `-y` for both — so `snob watch once someone` refused with
/// advice that then failed to parse, because `watch once` deliberately has no
/// `-y`.
///
/// `shown` is `target::label`'s answer, so it is already the at sign and the
/// filtered name.
pub fn refuse_unconsented(shown: &str, in_advance: ConsentInAdvance) -> anyhow::Error {
    let in_advance = match in_advance {
        ConsentInAdvance::Flag => "Pass -y to confirm in advance.".to_string(),
        ConsentInAdvance::WatchConfig => format!(
            "Run \"snob watch setup\" to answer it once, or ask about {shown} \
             while you are here."
        ),
    };
    refuse_unattended(
        format!("reading {shown}'s lists needs confirmation"),
        in_advance,
    )
}

/// They were asked, and they said no.
///
/// No mention of `-y` here, and that is the whole difference from
/// [`refuse_unconsented`]: they have just said no, and answering that with
/// "pass the flag that skips the question" is telling them to do it anyway.
pub fn refuse_declined(shown: &str) -> anyhow::Error {
    ExitError::new(
        ExitCode::Interrupted,
        format!("nothing was done: {shown} was not confirmed"),
    )
    .into()
}

/// Serving a stored list because nothing may be spent.
///
/// The list is named because a crossing serves two of them, and two identical
/// warnings in a row read like the same one printed twice.
pub fn serving_stored_in_cooldown(until_ms: EpochMs, kind: ListKind, taken_at: Epoch) -> String {
    format!(
        "the account is in cooldown until {}; serving the {kind} list stored on {}",
        cooldown_ends_at(until_ms),
        stored_on(taken_at)
    )
}

/// The counter poll failed and there is a stored list to fall back on.
///
/// Walking the whole list right when Instagram is already having trouble is the
/// worst possible reaction, so the warning says what was served rather than
/// what was refused.
pub fn poll_failed_serving_stored(error: &anyhow::Error) -> String {
    format!(
        "could not check for changes ({}); using the stored list",
        what_went_wrong(error)
    )
}

/// The counter poll failed and nothing is stored, so the walk goes ahead
/// without a number to check it against.
pub fn poll_failed(error: &anyhow::Error) -> String {
    format!("could not read the profile ({})", what_went_wrong(error))
}

/// A failure as one clause inside a sentence.
///
/// Interpolating the error would print [`snob_ig::error::IgError`]'s diagnosis
/// and drop the advice that used to be part of the same message, on the two
/// warnings a dead session reaches most often. Only the head of the chain is
/// asked, because that is the one whose text is about to be printed.
fn what_went_wrong(error: &anyhow::Error) -> String {
    match error
        .chain()
        .next()
        .and_then(|head| head.downcast_ref::<snob_ig::error::IgError>())
    {
        Some(instagram) => what_instagram_said(instagram),
        None => error.to_string(),
    }
}

/// A private account nobody here can read the lists of.
///
/// Two answers rather than one, because a pending follow request is a different
/// situation from never having asked: the first is waiting on somebody else and
/// the second is waiting on the reader.
///
/// Filtered here rather than at the call site: the name came off Instagram and
/// this sentence is written to a terminal.
pub fn refuse_private(username: &str, requested: bool) -> anyhow::Error {
    if requested {
        return anyhow::anyhow!(
            "@{} is private and your follow request has not been accepted yet, \
             so its lists cannot be read",
            printable(username)
        );
    }
    anyhow::anyhow!(
        "@{} is a private account you do not follow, so its lists cannot be read",
        printable(username)
    )
}

/// The account whose profile Instagram will not serve, and what that costs the
/// run.
///
/// Two things people expect quietly stop happening — the truncation wall cannot
/// be detected without a declared size, and `--offline` has nothing to weigh
/// freshness against — so both are said out loud rather than left to be
/// discovered.
pub fn counters_unknowable(username: &str) -> String {
    format!(
        "Instagram would not serve the profile of @{}, so its id came from search \
         instead. That route carries no follower or following counts, so this run \
         cannot tell a truncated list from a complete one, and cannot judge whether \
         a cached list is still current.",
        printable(username)
    )
}

/// A run that concluded nothing about an account nothing is stored for.
///
/// It is the tick's own refusal rather than a report, so it names no command:
/// there was nothing wrong with what the user asked for, and the next run may
/// well answer it.
pub fn refuse_nothing_looked_at(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "nothing could be looked at for @{} this time, and nothing is stored \
         about that account yet to report against",
        printable(name)
    )
}

/// A name the monitor was pointed at and has never walked.
///
/// Deliberately not [`refuse_nothing_stored`], which talks about `--offline` — a
/// flag the commands that reach this do not have. What the user has to do here
/// is walk the account once, and the sentence says so.
pub fn refuse_never_walked(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "nothing is stored about @{}. Run \"snob followers {}\" once and the monitor \
         will have something to compare against from then on.",
        printable(name),
        printable(name),
    )
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
/// makes once pagination has already ended, where there is no cursor to save.
/// The
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

/// What to do about something Instagram's client reported, when there is
/// anything to do about it.
///
/// Two of `IgError`'s messages used to end in advice — "run \"snob login\"
/// again", and the two ways to get a CSRF token — which named subcommands of a
/// binary `snob-ig` does not know it is part of, from a crate that has no
/// terminal and no exit codes. The diagnosis stays in the variant, because only
/// the client knows what happened; the advice lives here, where the rest of the
/// tool's advice lives, and reaches the reader as the `hint:` line that every
/// other refusal already puts it on.
///
/// `&'static str` and not a sentence built per call: none of this depends on
/// what was being asked for. Where a cooldown lifts is the counter-example, and
/// it is a date rather than advice — [`rendered`] adds that one separately.
pub fn advice_for(error: &snob_ig::error::IgError) -> Option<&'static str> {
    use snob_ig::error::IgError;
    match error {
        IgError::SessionExpired => Some("run \"snob login\" again"),
        IgError::NoCsrfToken => Some(
            "run \"snob login --browser\", or pass the token with \
             \"snob login --paste --csrftoken\"",
        ),
        _ => None,
    }
}

/// One line: what Instagram's client said, and what to do about it.
///
/// The `hint:` line is what a **reported failure** gets, and the two places
/// that print an `IgError` are not that: `engine::walk` warns about what
/// stopped a walk while the walk's own refusal is still to come, and
/// `engine::check` puts the answer in a column of its own. Both used to print
/// the message whole, advice included, so this rejoins the two halves exactly
/// where they were joined before.
pub fn what_instagram_said(error: &snob_ig::error::IgError) -> String {
    match advice_for(error) {
        Some(advice) => format!("{error}; {advice}"),
        None => error.to_string(),
    }
}

/// What the walker noticed, in the words the person watching it reads.
///
/// These six sentences were authored inside `snob_ig::pager` and printed
/// unmodified by `progress.rs`, which put the wording of the walk's most
/// alarming lines in the HTTP crate — the one part of the tool that has no
/// terminal, no format and no business having an opinion about either. The
/// pager reports the condition now; the words are decided here, beside every
/// other sentence the tool prints.
///
/// Two of them carry numbers, which is why this returns a `String` rather than
/// a `&'static str`: what makes a shortfall worth reading is how big it is.
pub fn pager_warning(warning: Warning) -> String {
    match warning {
        Warning::SameCursorTwice => {
            "Instagram returned the same cursor twice; stopping so the request is not repeated"
                .to_string()
        }
        Warning::TwoEmptyPages => "Instagram returned two empty pages in a row".to_string(),
        Warning::GoingInCircles => {
            "several pages in a row with no new accounts; the list is going in circles".to_string()
        }
        Warning::EmptyAndNoCounter => {
            "the list came back empty and the profile counter could not be read, so there is \
             no way to tell an empty list from one Instagram did not serve; treating it as \
             incomplete rather than risking the comparison"
                .to_string()
        }
        Warning::StoppedShort { walked, declared } => format!(
            "Instagram stopped serving pages at {walked} of the {declared} accounts it declared; \
             the list is incomplete and cannot be compared against"
        ),
        Warning::ShortOfDeclared { walked, declared } => format!(
            "walked {walked} accounts while Instagram declared {declared}; \
             the difference is usually deleted accounts"
        ),
    }
}

/// Nothing stored to answer with, and `--offline` said not to look.
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
        format!("no {kind} list is stored, and --offline says not to look for one"),
    )
    .with_hint(format!("run \"snob {kind}\" once, or drop --offline"))
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
    use snob_core::Pk;

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
                pk: Pk::new(i as u64 + 1),
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
        let error = refuse_in_cooldown(EpochMs::new(1_000), Blocked::AccountUnknown(name));
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
            started_at: Epoch::default(),
            taken_at: Epoch::default(),
            account_pk: Pk::new(1),
            snapshot_id: 1,
            stopped_by,
            resumable: false,
        }
    }

    /// A timestamp that makes no sense still has to read as something, because
    /// the alternative is a message with a hole in the middle of it.
    #[test]
    fn a_date_out_of_range_still_reads_as_something() {
        assert_eq!(stored_on(Epoch::new(i64::MAX)), "earlier");
        assert_eq!(cooldown_ends_at(EpochMs::new(i64::MAX)), "later");
        assert_eq!(
            format_epoch_in(Epoch::new(1_722_700_000), "earlier", &chrono::Utc),
            "Aug 3 at 15:46"
        );
        // The same moment in milliseconds reads as the same second, which is
        // what `EpochMs::to_epoch` is for.
        assert_eq!(
            format_epoch_in(
                EpochMs::new(1_722_700_000_000).to_epoch(),
                "later",
                &chrono::Utc
            ),
            "Aug 3 at 15:46"
        );
    }

    /// What is printed is the wall clock, not UTC with no label.
    #[test]
    fn a_moment_is_printed_in_the_zone_the_reader_is_in() {
        let madrid_in_august = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            format_epoch_in(Epoch::new(1_722_700_000), "earlier", &madrid_in_august),
            "Aug 3 at 17:46"
        );
    }

    /// A failure told to a program carries what the prose carries: the code
    /// the exit status names, the message, the advice set apart, and the
    /// address a challenge is cleared at -- filtered, since the consumer may
    /// print it.
    #[test]
    fn a_failure_in_json_carries_the_code_the_hint_and_the_address() {
        let hostile = "gh\u{1b}[2K";
        let error: anyhow::Error = anyhow::Error::new(snob_ig::error::IgError::Challenge {
            url: Some("https://www.instagram.com/challenge/".into()),
        })
        .context(format!("could not read @{hostile}'s followers list"));
        let json = error_json(&error);
        let error = &json["error"];

        assert_eq!(error["code"], "challenge");
        assert_eq!(error["exit"], 4);
        assert_eq!(error["message"], "could not read @gh[2K's followers list");
        assert_eq!(error["url"], "https://www.instagram.com/challenge/");
        assert_eq!(error["causes"].as_array().map(Vec::len), Some(1));

        let advised: anyhow::Error = ExitError::new(ExitCode::NoSession, "no session")
            .with_hint("run \"snob login\"")
            .into();
        let json = error_json(&advised);
        assert_eq!(json["error"]["code"], "no_session");
        assert_eq!(json["error"]["hint"], "run \"snob login\"");
        assert!(json["error"]["url"].is_null());
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
        let throttled = refuse_different_moments(
            Provenance::Cooldown,
            Provenance::Cooldown,
            Epoch::new(0),
            Epoch::new(1),
        );
        assert!(hint_of(&throttled).unwrap().contains("cooldown lifts"));
        assert_eq!(
            ExitCode::from_chain(&throttled),
            Some(ExitCode::RateLimited)
        );

        let asked_for = refuse_different_moments(
            Provenance::CacheFlag,
            Provenance::CacheFlag,
            Epoch::new(0),
            Epoch::new(1),
        );
        let hint = hint_of(&asked_for).unwrap();
        assert!(hint.contains("--offline"), "{hint}");
        assert!(!hint.contains("cooldown"), "{hint}");
        assert_eq!(ExitCode::from_chain(&asked_for), Some(ExitCode::Error));

        // A failed poll is neither of the two. Nobody asked for storage — the
        // request to check went out and did not come back — so it used to be
        // told to drop a flag it never passed, which sends somebody looking for
        // something that is not in their command line.
        let nobody_could_check = refuse_different_moments(
            Provenance::PollFailed,
            Provenance::PollFailed,
            Epoch::new(0),
            Epoch::new(1),
        );
        let hint = hint_of(&nobody_could_check).unwrap();
        assert!(!hint.contains("--offline"), "{hint}");
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
            let error = refuse_in_cooldown(EpochMs::new(1_722_700_000_000), blocked);
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

    /// Advice that names a `snob` subcommand belongs to the binary, not to its
    /// HTTP client — and it still has to reach the person reading.
    ///
    /// It used to be the tail of two `IgError` messages, so it arrived wherever
    /// the message did. Moved to the hint, the thing to check is that every
    /// path it took before still carries it: the labeled failure, the JSON
    /// shape, and the two places that print an `IgError` as one line.
    #[test]
    fn the_advice_an_ig_error_carried_still_reaches_the_reader() {
        use snob_ig::error::IgError;

        for (error, advice) in [
            (IgError::SessionExpired, "run \"snob login\" again"),
            (
                IgError::NoCsrfToken,
                "run \"snob login --browser\", or pass the token with \
                 \"snob login --paste --csrftoken\"",
            ),
        ] {
            // One line, exactly as the message used to read.
            assert_eq!(
                what_instagram_said(&error),
                format!("{error}; {advice}"),
                "the two halves have to rejoin where they were joined before"
            );

            let said = error.to_string();
            let error: anyhow::Error = anyhow::Error::new(error);
            let out = rendered(&error);
            assert!(out.contains(&said), "{out}");
            assert!(out.contains("hint:"), "{out}");
            assert!(out.contains(advice), "{out}");

            let json = error_json(&error);
            assert_eq!(json["error"]["hint"], advice);
            assert_eq!(json["error"]["message"], said);
        }

        // Everything else has nothing to advise, and an invented hint would be
        // worse than none.
        assert_eq!(advice_for(&IgError::TooManyRedirects), None);
        assert_eq!(
            what_instagram_said(&IgError::TooManyRedirects),
            IgError::TooManyRedirects.to_string()
        );
    }

    /// One hint, whoever wrote it. A reader is being told what to do, and two
    /// answers to that is worse than either of them alone.
    #[test]
    fn a_failure_carries_one_piece_of_advice() {
        let from_the_client: anyhow::Error =
            anyhow::Error::new(snob_ig::error::IgError::SessionExpired);
        assert_eq!(rendered(&from_the_client).matches("hint:").count(), 1);

        // A command's own advice is the one that shows, and the client's is
        // not consulted. They cannot in fact meet — an `ExitError` carries no
        // source, so nothing of Instagram's is ever underneath one — but the
        // order is written down rather than left to that.
        let from_the_command: anyhow::Error =
            ExitError::new(ExitCode::NoSession, "no session is stored")
                .with_hint("run \"snob login\"")
                .into();
        let out = rendered(&from_the_command);
        assert_eq!(out.matches("hint:").count(), 1, "{out}");
        assert!(out.contains("run \"snob login\"\n"), "{out}");
    }

    /// The pager reports a condition and this is where it becomes a sentence,
    /// so this is where the sentence is asserted on.
    ///
    /// `pager.rs` used to write these itself and its own tests read them back
    /// with `contains`. They match on the variant now, which is the right test
    /// over there and leaves the words untested unless something checks them
    /// here.
    #[test]
    fn every_warning_the_walk_raises_says_something() {
        assert!(
            pager_warning(Warning::EmptyAndNoCounter).contains("came back empty"),
            "{}",
            pager_warning(Warning::EmptyAndNoCounter)
        );
        for warning in [
            Warning::SameCursorTwice,
            Warning::TwoEmptyPages,
            Warning::GoingInCircles,
            Warning::EmptyAndNoCounter,
        ] {
            assert!(!pager_warning(warning).is_empty(), "{warning:?}");
        }

        // The two that carry numbers name both of them: a shortfall with only
        // one of its halves shown says nothing about how big it is.
        let short = pager_warning(Warning::StoppedShort {
            walked: 39,
            declared: 21_631,
        });
        assert!(short.contains("39") && short.contains("21631"), "{short}");

        let deleted = pager_warning(Warning::ShortOfDeclared {
            walked: 80,
            declared: 100,
        });
        assert!(
            deleted.contains("80") && deleted.contains("100"),
            "{deleted}"
        );
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
