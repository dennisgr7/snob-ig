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
use crate::exit::ExitCode;

/// Walks the list, resuming an interrupted one when there is a usable one.
pub async fn fetch(
    app: &mut App,
    args: &ListArgs,
    kind: ListKind,
    target: &Target,
    declared: Option<u64>,
) -> Result<(Vec<User>, ListOutcome)> {
    let opened = open_snapshot(app, args, kind, target, declared)?;
    let id = opened.id;

    let request = ListRequest {
        pk: target.pk,
        // Empty when the name was never learned, which the pager documents as
        // allowed and simply leaves the referer generic. It used to be the
        // numeric id, so the walk announced
        // `Referer: https://www.instagram.com/4340136074/followers/` — a page
        // no browser would ever have been on.
        username: target.username.as_deref().unwrap_or_default(),
        direction: kind.into(),
        from: opened.cursor.as_deref(),
        estimated: declared,
        max_pages: args.max_pages,
        already_stored: opened.already_stored,
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
            return Err(crate::report::refuse_cooldown_mid_walk(until_ms));
        }
        // A crossing walks two lists in the same run, so "the walk failed" did
        // not say which one stopped. The name goes through `printable` for the
        // same reason every other account name this tool prints does: it came
        // off Instagram, not out of anybody's keyboard.
        Err(error) => {
            let who = crate::app::target_label(target.username.as_deref());
            return Err(
                anyhow::Error::new(error).context(format!("could not read {who}'s {kind} list"))
            );
        }
    };

    snapshots::close(app.db().conn(), id, summary.reason)?;

    // Asked of the store rather than inferred from the stop reason, and asked
    // with the same predicate the next run will use — so "it can be continued"
    // means the next run really would, cursor and resume window included.
    //
    // `is_resumable` and not `resumable`: the latter takes the claim in the
    // statement that finds the row, so asking it here handed this process the
    // claim `close` had just released, moments before it exited. See its
    // doc-comment for what that cost.
    let resumable = snapshots::is_resumable(app.db().conn(), target.pk, kind)?;

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
            started_at: opened.started_at,
            taken_at: now(),
            account_pk: target.pk,
            snapshot_id: id,
            stopped_by,
            resumable,
        },
    ))
}

/// The snapshot this walk will fill, and what is already known about it.
///
/// A struct rather than a tuple because the fourth field is what tipped it: two
/// of them are now numbers, and `(i64, i64, Option<String>, usize)` at a call
/// site says nothing about which is which.
struct Opened {
    id: i64,
    /// Read from the row rather than taken now, so a resumed walk keeps the
    /// moment its **first** page was asked for. That is the honest start of the
    /// interval this list covers: it does reflect everything from that page
    /// onward, and `snapshots::RESUME_WINDOW_SECS` has already decided a pause
    /// of that length is one capture.
    started_at: i64,
    cursor: Option<String>,
    already_stored: usize,
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
) -> Result<Opened> {
    let conn = app.db().conn();

    let pending = if args.no_resume {
        None
    } else {
        snapshots::resumable(conn, target.pk, kind)?
    };

    if let Some(snapshot) = pending {
        snapshots::mark_resumed(conn, snapshot.id)?;
        return Ok(Opened {
            id: snapshot.id,
            started_at: snapshot.started_at,
            cursor: snapshot.next_cursor,
            already_stored: snapshot.member_count as usize,
        });
    }

    snapshots::delete_partials(conn, target.pk, kind)?;
    let fresh = snapshots::begin(conn, target.pk, kind, declared)?;
    Ok(Opened {
        id: fresh.id,
        started_at: fresh.started_at,
        cursor: None,
        already_stored: 0,
    })
}
