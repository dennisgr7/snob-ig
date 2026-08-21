//! `snob followers` and `snob following`.
//!
//! Both are the same command with the list named differently. What they do is
//! ask [`crate::engine`] for a list and print it; the deciding — cache, walk,
//! cooldown — happens there.

use anyhow::Result;
use snob_core::model::{ListKind, StopReason, User};
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;

use crate::cli::ListArgs;
use crate::commands::common::{self, Session};
use crate::engine::{self, ListOutcome, ResultSource};
use crate::exit::ExitCode;
use crate::report;
use crate::ui;

pub async fn run(
    args: ListArgs,
    secrets: SecretStore,
    paths: &AppPaths,
    kind: ListKind,
) -> Result<ExitCode> {
    let filter = common::filter_from(&args)?;
    let destination = common::destination(&args)?;

    let Session::Open(mut app) = common::open(&args, &secrets, paths)? else {
        return Ok(ExitCode::NoSession);
    };

    // Named before the engine runs, so the bar says what it is about during
    // consent, resolution and the counter poll rather than only once pages
    // start arriving.
    let subject = engine::target::label(&app, &args);
    let result = common::walk_named(&mut app, &args, kind, &subject, |_| Ok(())).await;
    app.progress().finish();
    let (found, outcome) = result?;

    let total = found.len();
    let mut found = filter.apply(found);
    let kept = found.len();
    if let Some(cap) = args.limit {
        found.truncate(cap);
    }

    destination.write(&found)?;
    print_summary(&found, kept, total, &outcome, kind);
    Ok(exit_code(&outcome))
}

/// A plain list is the one place a partial answer is still worth having: every
/// account in it really is in the list, only some are missing. So it prints,
/// says so, and reports what stopped it.
///
/// A stored list needs no arm of its own. `ListOutcome::cached` is the only way
/// to a provenance other than `Walked` and it records `Completed`, so anything
/// out of storage arrives at the first arm anyway — and an arm that reads as
/// policy while deciding nothing is one a later change would edit to no effect.
fn exit_code(outcome: &ListOutcome) -> ExitCode {
    match outcome.reason {
        // A cap was asked for by the user, so it is not a failure.
        StopReason::Completed | StopReason::PageLimit => ExitCode::Ok,
        _ => outcome.exit_code(),
    }
}

/// The singular of a list's name. `Display` gives the plural, and for
/// `Following` the two differ by more than a letter. It lives here rather than
/// on `ListKind` because wording belongs in the commands, not in the domain.
fn one_of(kind: ListKind) -> &'static str {
    match kind {
        ListKind::Followers => "follower",
        ListKind::Following => "account you follow",
    }
}

fn print_summary(found: &[User], kept: usize, total: usize, outcome: &ListOutcome, kind: ListKind) {
    let mut line = report::counted(found.len(), kept, total, one_of(kind), &kind.to_string());

    match outcome.source() {
        ResultSource::Cached => {
            line.push_str(&format!(
                " - list stored on {}",
                report::stored_on(outcome.taken_at)
            ));
            if outcome.requests > 0 {
                line.push_str(&format!(" - {}", report::requests(outcome.requests)));
            } else {
                line.push_str(" - without touching the network");
            }
        }
        ResultSource::Fetched => {
            line.push_str(&format!(" - {}", report::requests(outcome.requests)));
            if let Some(why) = report::why_incomplete(outcome.reason) {
                line.push_str(&format!(" - INCOMPLETE: {why}"));
            }
        }
    }

    ui::info(&line);

    if !outcome.is_complete() && outcome.source() == ResultSource::Fetched {
        ui::warn(&format!(
            "the list is incomplete, so it cannot be compared against another one. {}",
            report::try_again_advice(outcome.reason, outcome.resumable)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(source: ResultSource, reason: StopReason) -> ListOutcome {
        ListOutcome {
            provenance: match source {
                ResultSource::Fetched => engine::Provenance::Walked,
                ResultSource::Cached => engine::Provenance::CounterVerified,
            },
            reason,
            requests: 1,
            started_at: 0,
            taken_at: 0,
            account_pk: 1,
            snapshot_id: 1,
            stopped_by: None,
            resumable: false,
        }
    }

    /// A cap the user asked for is not a failure, and neither is anything
    /// served from storage. Everything else keeps the code that says what
    /// happened, so a script can tell "wait" from "log in again".
    #[test]
    fn the_exit_code_reports_what_stopped_the_walk() {
        for reason in [StopReason::Completed, StopReason::PageLimit] {
            assert_eq!(
                exit_code(&outcome(ResultSource::Fetched, reason)),
                ExitCode::Ok
            );
        }
        assert_eq!(
            exit_code(&outcome(ResultSource::Fetched, StopReason::RateLimit)),
            ExitCode::RateLimited
        );
        assert_eq!(
            exit_code(&outcome(ResultSource::Fetched, StopReason::Canceled)),
            ExitCode::Interrupted
        );
        assert_eq!(
            exit_code(&outcome(ResultSource::Fetched, StopReason::SessionInvalid)),
            ExitCode::NoSession
        );
        // The two the README used to promise were a 0, on the strength of "a
        // plain list prints what it got". It does print it, and it still exits
        // with what stopped it — a wrapper written against that paragraph
        // treated the documented truncation wall as a success.
        for stopped in [StopReason::Truncated, StopReason::Network] {
            assert_eq!(
                exit_code(&outcome(ResultSource::Fetched, stopped)),
                ExitCode::Error,
                "{stopped:?} is not something the user asked for"
            );
        }
        // A stored list is a stored list, whatever ended the walk that made it.
        assert_eq!(
            exit_code(&outcome(ResultSource::Cached, StopReason::Completed)),
            ExitCode::Ok
        );
    }
}
