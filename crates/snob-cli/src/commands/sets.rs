//! `snob unfollowers`, `snob fans` and `snob friends`.
//!
//! All three cross an account's two lists, so they cost two walks instead of
//! one. The cache policy makes them just as cheap as `followers` and
//! `following`: if neither counter has moved, the cross comes out of storage
//! and costs two requests.

use anyhow::Result;
use snob_core::model::{ListKind, User};
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;
use snob_core::sets;

use crate::cli::ListArgs;
use crate::commands::common::{self, Session};
use crate::engine::{self, ListOutcome};
use crate::exit::ExitCode;
use crate::report;
use crate::ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// Those you follow who do not follow you back.
    Unfollowers,
    /// Those who follow you and you do not follow.
    Fans,
    /// Those you follow each other.
    Friends,
}

impl SetOp {
    /// What the result is, singular first. Both are written out because the
    /// singular of these is not the plural with its `s` taken off.
    fn description(self) -> (&'static str, &'static str) {
        match self {
            Self::Unfollowers => (
                "account you follow that does not follow you back",
                "accounts you follow that do not follow you back",
            ),
            Self::Fans => (
                "account that follows you and you do not follow",
                "accounts that follow you and you do not follow",
            ),
            Self::Friends => (
                "friend, you follow each other",
                "friends, following each other",
            ),
        }
    }

    /// The list the results come out of.
    fn base(self) -> ListKind {
        match self {
            Self::Unfollowers => ListKind::Following,
            Self::Fans | Self::Friends => ListKind::Followers,
        }
    }

    /// The list it is crossed against.
    fn against(self) -> ListKind {
        match self {
            Self::Unfollowers => ListKind::Followers,
            Self::Fans | Self::Friends => ListKind::Following,
        }
    }

    /// What an account missing from the crossed-against list would be made to
    /// look like. This is the sentence that explains why a partial one is
    /// refused rather than reported.
    fn misreads_as(self) -> &'static str {
        match self {
            Self::Unfollowers => "they did not follow you",
            Self::Fans => "you did not follow them",
            Self::Friends => "you were not friends",
        }
    }
}

pub async fn run(
    args: ListArgs,
    secrets: SecretStore,
    paths: &AppPaths,
    op: SetOp,
) -> Result<ExitCode> {
    let filter = common::filter_from(&args)?;
    let destination = common::destination(&args)?;

    let Session::Open(mut app) = common::open(&args, &secrets, paths)? else {
        return Ok(ExitCode::NoSession);
    };

    // The list being crossed against comes first. If it turns out incomplete
    // there is no result to give, so it is worth finding out before spending
    // the second walk.
    let subject = engine::target::label(&app, &args);
    let (against, against_outcome) =
        common::walk_named(&mut app, &args, op.against(), &subject, |outcome| {
            check_against_list(op, outcome)
        })
        .await?;

    // The second walk renames the bar: a crossing is two lists, and without
    // this the slower half looked exactly like the first.
    let second = common::walk_named(&mut app, &args, op.base(), &subject, |_| Ok(())).await;
    app.progress().finish();
    let (base, base_outcome) = second?;

    engine::cooldown::check_same_moment(&against_outcome, &base_outcome)?;

    let mut result = match op {
        SetOp::Unfollowers | SetOp::Fans => sets::difference(&base, &against),
        SetOp::Friends => sets::intersection(&base, &against),
    };

    let total = result.len();
    result = filter.apply(result);
    let kept = result.len();
    if let Some(cap) = args.limit {
        result.truncate(cap);
    }

    destination.write(&result)?;

    print_summary(
        op,
        &result,
        kept,
        total,
        &base,
        &base_outcome,
        &against_outcome,
    );

    Ok(exit_code(&base_outcome))
}

/// The check that stops a false result from being reported.
///
/// This is where the two lists differ in a way that matters. If the list being
/// crossed against is incomplete, every account missing from it **shows up in
/// the result without deserving to**: in `unfollowers`, someone who does follow
/// you but was never read out of your followers would be presented as not
/// following you. That is not a partial result, it is a wrong one, and it is
/// exactly the failure tools of this kind carry. So it stops.
///
/// The base list being incomplete is a different matter: results are missing,
/// but the ones shown are true. That only gets a warning.
fn check_against_list(op: SetOp, outcome: &ListOutcome) -> Result<()> {
    if outcome.is_complete() {
        return Ok(());
    }
    Err(report::refuse_incomplete(
        op.against(),
        outcome,
        op.misreads_as(),
    ))
}

fn exit_code(base: &ListOutcome) -> ExitCode {
    if base.is_complete() {
        ExitCode::Ok
    } else {
        base.exit_code()
    }
}

fn print_summary(
    op: SetOp,
    result: &[User],
    kept: usize,
    total: usize,
    base: &[User],
    base_outcome: &ListOutcome,
    against_outcome: &ListOutcome,
) {
    let total_requests = base_outcome.requests + against_outcome.requests;

    let (one, many) = op.description();
    let mut line = report::counted(result.len(), kept, total, one, many);
    line.push_str(&format!(
        " - {} - {}",
        // The proportion describes the crossing, so it is the count before any
        // filter or cap: "3 of 412 accounts you follow".
        proportion(total, base.len()),
        report::requests(total_requests)
    ));
    ui::info(&line);

    if !base_outcome.is_complete() {
        ui::warn(
            "the starting list is incomplete, so results are missing. \
             The ones shown are correct.",
        );
    }
}

fn proportion(part: usize, total: usize) -> String {
    if total == 0 {
        return "no data".into();
    }
    format!("{part} of {total}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::model::StopReason;

    fn outcome(reason: StopReason) -> ListOutcome {
        ListOutcome {
            provenance: engine::Provenance::Walked,
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

    #[test]
    fn a_complete_against_list_lets_it_continue() {
        assert!(check_against_list(SetOp::Unfollowers, &outcome(StopReason::Completed)).is_ok());
    }

    /// The central rule of these commands: without a complete list to cross
    /// against, the result would not be partial, it would be wrong.
    #[test]
    fn a_partial_against_list_blocks_the_result() {
        for reason in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Truncated,
            StopReason::RateLimit,
            StopReason::Network,
            StopReason::SessionInvalid,
        ] {
            let result = check_against_list(SetOp::Unfollowers, &outcome(reason));
            assert!(result.is_err(), "with {reason:?} no result can be given");
            assert!(result.unwrap_err().to_string().contains("wrong"));
        }
    }

    #[test]
    fn each_operation_crosses_the_lists_the_right_way_round() {
        assert_eq!(SetOp::Unfollowers.base(), ListKind::Following);
        assert_eq!(SetOp::Unfollowers.against(), ListKind::Followers);
        assert_eq!(SetOp::Fans.base(), ListKind::Followers);
        assert_eq!(SetOp::Fans.against(), ListKind::Following);
        assert_eq!(SetOp::Friends.base(), ListKind::Followers);
        assert_eq!(SetOp::Friends.against(), ListKind::Following);
    }

    /// The refusal has to name the mistake the user would otherwise have made,
    /// and each crossing produces a different one.
    #[test]
    fn each_operation_explains_its_own_misreading() {
        let text = |op| {
            check_against_list(op, &outcome(StopReason::Truncated))
                .unwrap_err()
                .to_string()
        };
        assert!(text(SetOp::Unfollowers).contains("they did not follow you"));
        assert!(text(SetOp::Fans).contains("you did not follow them"));
        assert!(text(SetOp::Friends).contains("you were not friends"));
    }

    #[test]
    fn the_proportion_does_not_divide_by_zero() {
        assert_eq!(proportion(0, 0), "no data");
        assert_eq!(proportion(3, 10), "3 of 10");
    }
}
