//! The walk itself: opening a snapshot, filling it page by page, closing it.
//!
//! Each page commits in its own transaction, which is what makes saving partial
//! progress not an action that has to run in time but simply a matter of
//! stopping — and that matters because neither `exit()` nor `panic = "abort"`
//! run destructors.

use anyhow::Result;
use snob_core::model::{ListKind, User};
use snob_core::store::{now, snapshots};
use snob_ig::pace::Pace;
use snob_ig::pager::{ListRequest, ListWalker, WalkError};

use crate::app::App;
use crate::cli::ListArgs;
use crate::engine::target::Target;
use crate::engine::{ListOutcome, Provenance};
use crate::exit::{ExitCode, ExitError};

/// Walks the list, resuming an interrupted one when there is a usable one.
pub async fn fetch(
    app: &mut App,
    args: &ListArgs,
    kind: ListKind,
    target: &Target,
    declared: Option<u64>,
) -> Result<(Vec<User>, ListOutcome)> {
    let (id, cursor, already_stored) = open_snapshot(app, args, kind, target, declared)?;

    let request = ListRequest {
        pk: target.pk,
        username: &target.username,
        direction: kind.into(),
        from: cursor.as_deref(),
        estimated: declared,
        max_pages: args.max_pages,
        already_stored,
    };

    let cancel = app.cancel().clone();
    // Somebody else's lists are walked more slowly. This is the one place that
    // decides it, so the set commands and `scan` inherit it by coming through
    // here rather than each remembering to ask.
    let pace = if target.is_self {
        Pace::default()
    } else {
        Pace::third_party()
    };

    // The borrow of the store is handed to the callback for the duration of the
    // walk, which is why the three come out together.
    let (client, db, progress) = app.parts();
    let walker = ListWalker::new(client).with_cancel(cancel).with_pace(pace);

    let summary = match walker
        .walk(
            request,
            |page, _| {
                let batch: Vec<User> = page.users.iter().map(User::from).collect();
                Ok(snapshots::save_page(db, id, &batch, page.next_cursor())?.added)
            },
            |event| progress.event(&event),
        )
        .await
    {
        Ok(summary) => summary,
        // Reachable only when the cooldown lands between the check in
        // `engine::list` and the walk, e.g. set by another process.
        Err(WalkError::Cooldown { until_ms, .. }) => {
            return Err(ExitError::new(
                ExitCode::RateLimited,
                format!(
                    "the account is in cooldown until {}; nothing can be walked until it lifts",
                    crate::report::cooldown_ends_at(until_ms)
                ),
            )
            .into());
        }
        Err(error) => return Err(anyhow::Error::new(error).context("the walk failed")),
    };

    snapshots::close(app.db().conn(), id, summary.reason)?;

    // What Instagram said, said out loud. The walker keeps the error next to
    // the stop reason and nothing used to read it, so a checkpoint arrived as
    // "the session stopped working" — with the address that would have cleared
    // it, which `IgError::Checkpoint` carries precisely so it can be shown,
    // dropped on the way.
    let stopped_by = summary.error.map(|error| {
        app.warn(&error.to_string());
        ExitCode::from_ig_error(&error)
    });

    Ok((
        snapshots::members(app.db().conn(), id)?,
        ListOutcome {
            provenance: Provenance::Walked,
            reason: summary.reason,
            // Filled in by `engine::list` from the pacer.
            requests: 0,
            taken_at: now(),
            account_pk: target.pk,
            stopped_by,
        },
    ))
}

/// Continues an interrupted walk when one is still usable, and starts a fresh
/// snapshot otherwise. Either way, half-finished ones that no longer serve are
/// cleared out rather than piling up.
fn open_snapshot(
    app: &App,
    args: &ListArgs,
    kind: ListKind,
    target: &Target,
    declared: Option<u64>,
) -> Result<(i64, Option<String>, usize)> {
    let conn = app.db().conn();

    let pending = if args.no_resume {
        None
    } else {
        snapshots::resumable(conn, target.pk, kind)?
    };

    if let Some(snapshot) = pending {
        snapshots::mark_resumed(conn, snapshot.id)?;
        return Ok((
            snapshot.id,
            snapshot.next_cursor,
            snapshot.member_count as usize,
        ));
    }

    snapshots::delete_partials(conn, target.pk, kind)?;
    Ok((snapshots::begin(conn, target.pk, kind, declared)?, None, 0))
}
