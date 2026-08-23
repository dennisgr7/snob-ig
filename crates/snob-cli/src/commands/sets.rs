//! `snob unfollowers`, `snob fans` and `snob friends`.
//!
//! All three cross an account's two lists, so they cost two walks instead of
//! one. The cache policy makes them just as cheap as `followers` and
//! `following`: if neither counter has moved, the cross comes out of storage
//! and costs two requests.

use anyhow::Result;
use snob_core::model::{ListKind, User};
use snob_core::sets;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::ListArgs;
use crate::commands::common;
use crate::engine::{self, ListOutcome, ResultSource};
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
    let filter = common::filter_from(&args.filter)?;
    let destination = common::destination(&args.output)?;

    let mut app = common::open(&args.walk, &secrets, paths)?;

    // The list being crossed against comes first. If it turns out incomplete
    // there is no result to give, so it is worth finding out before spending
    // the second walk.
    let subject = engine::target::label(&app, args.target.as_deref());
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

    let result = match op {
        SetOp::Unfollowers | SetOp::Fans => sets::difference(&base, &against),
        SetOp::Friends => sets::intersection(&base, &against),
    };

    let common::Narrowed {
        shown: result,
        kept,
        total,
    } = common::narrow(result, &filter, args.limit);

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

    // Decided by what stopped the **base** list alone. The list crossed
    // against is not consulted: it was refused outright by
    // `check_against_list`, well before there was a result to code.
    Ok(base_outcome.exit_code_for_a_printed_result())
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

fn print_summary(
    op: SetOp,
    result: &[User],
    kept: usize,
    total: usize,
    base: &[User],
    base_outcome: &ListOutcome,
    against_outcome: &ListOutcome,
) {
    ui::info(&summary_line(
        op,
        result.len(),
        kept,
        total,
        base.len(),
        base_outcome,
        against_outcome,
    ));

    if !base_outcome.is_complete() {
        ui::warn(
            "the starting list is incomplete, so results are missing. \
             The ones shown are correct.",
        );
    }
}

/// The one line a crossing prints about itself.
///
/// Built rather than printed, so a test can read it. It could not: everything
/// here went straight to `ui::info`, which is why the omission below survived —
/// nothing in the suite could see what this said.
fn summary_line(
    op: SetOp,
    found: usize,
    kept: usize,
    total: usize,
    base_len: usize,
    base_outcome: &ListOutcome,
    against_outcome: &ListOutcome,
) -> String {
    let total_requests = base_outcome.requests + against_outcome.requests;

    let (one, many) = op.description();
    let mut line = report::counted(found, kept, total, one, many);
    line.push_str(&format!(
        " - {}",
        // The proportion describes the crossing, so it is the count before any
        // filter or cap: "3 of 412 accounts you follow".
        proportion(total, base_len)
    ));

    // When it is stored, say so and say from when. This read only the counts,
    // so `snob unfollowers --cache` a month later printed a line that could not
    // be told apart from a crossing walked five minutes ago — while `snob
    // followers --cache` says "list stored on 07/07 at 14:12" for the very same
    // capture. `Provenance`'s own doc names an answer that does not say where it
    // came from as half of the defect it was written for.
    //
    // The older of the two dates, because a crossing is only as recent as its
    // staler half. `check_same_moment` is what stops the two being far apart at
    // all, so this is completeness rather than a correction.
    if base_outcome.source() == ResultSource::Cached
        || against_outcome.source() == ResultSource::Cached
    {
        line.push_str(&format!(
            " - lists stored on {}",
            report::stored_on(base_outcome.taken_at.min(against_outcome.taken_at))
        ));
    }

    if total_requests > 0 {
        line.push_str(&format!(" - {}", report::requests(total_requests)));
    } else {
        line.push_str(" - without touching the network");
    }
    line
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
    use snob_core::{Epoch, Pk};

    fn outcome(reason: StopReason) -> ListOutcome {
        ListOutcome {
            provenance: engine::Provenance::Walked,
            reason,
            requests: 1,
            started_at: Epoch::default(),
            taken_at: Epoch::default(),
            account_pk: Pk::new(1),
            snapshot_id: 1,
            stopped_by: None,
            resumable: false,
        }
    }

    fn stored(taken_at: Epoch) -> ListOutcome {
        ListOutcome {
            provenance: engine::Provenance::CacheFlag,
            requests: 0,
            taken_at,
            ..outcome(StopReason::Completed)
        }
    }

    /// A cap the user asked for is not a failure, on either side of a crossing.
    ///
    /// The result is written and the shortfall warned about, `cli.rs` promises
    /// 0 for it in as many words — so `snob unfollowers --max-pages 2` exiting
    /// 1 while `snob following --max-pages 2` exits 0 was two answers to one
    /// stop reason. The two commands ask one method now; this pins the
    /// crossing's side of it.
    #[test]
    fn a_base_list_stopped_by_the_page_cap_still_exits_zero() {
        let exit_code = ListOutcome::exit_code_for_a_printed_result;
        assert_eq!(exit_code(&outcome(StopReason::PageLimit)), ExitCode::Ok);
        assert_eq!(exit_code(&outcome(StopReason::Completed)), ExitCode::Ok);

        // And a stop nobody asked for still carries its own code, which is the
        // half that has to keep working.
        assert_ne!(exit_code(&outcome(StopReason::Truncated)), ExitCode::Ok);
        assert_eq!(
            exit_code(&outcome(StopReason::RateLimit)),
            ExitCode::RateLimited
        );
    }

    /// A crossing served from storage says so, and says from when.
    ///
    /// The line read only the counts, so `snob unfollowers --cache` a month
    /// later was indistinguishable from a crossing walked five minutes ago —
    /// while `snob followers --cache` says "list stored on 07/07 at 14:12" for
    /// the very same capture. The counts are right and `check_same_moment`
    /// blocks the dangerous case, so this is completeness rather than a
    /// correction; but an answer that does not say where it came from is half
    /// of what `Provenance` was written for.
    #[test]
    fn a_crossing_served_from_storage_says_when_it_is_from() {
        // Two captures a day apart. The line has to name the older.
        let older = Epoch::new(1_700_000_000);
        let line = summary_line(
            SetOp::Unfollowers,
            3,
            3,
            3,
            412,
            &stored(older),
            &stored(older + std::time::Duration::from_secs(24 * 3_600)),
        );

        assert!(
            line.contains(&report::stored_on(older)),
            "a crossing is only as recent as its staler half: {line}"
        );
        assert!(
            line.contains("without touching the network"),
            "nothing was spent, and that is worth saying: {line}"
        );
    }

    /// And a freshly walked one does not claim to be stored.
    #[test]
    fn a_crossing_that_was_walked_says_what_it_spent() {
        let line = summary_line(
            SetOp::Unfollowers,
            3,
            3,
            3,
            412,
            &outcome(StopReason::Completed),
            &outcome(StopReason::Completed),
        );

        assert!(!line.contains("stored on"), "{line}");
        assert!(!line.contains("without touching the network"), "{line}");
        assert!(line.contains(&report::requests(2)), "{line}");
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
