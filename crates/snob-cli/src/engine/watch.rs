//! What the monitor can say from storage alone.
//!
//! This is the read half of `snob watch`: given what is already on disk, work
//! out what has changed since the last time anything was reported. It spends no
//! request and opens no socket, which is what makes `snob watch diff` free to
//! run as often as anybody likes.
//!
//! It returns data and the interval that data covers. What any of it looks like
//! is [`crate::commands::watch`]'s question.

use anyhow::{Result, bail};
use snob_core::Pk;
use snob_core::model::{ListKind, User};
use snob_core::store::{accounts, snapshots, users, watch as store};
use snob_core::watch::{Basis, Changes, ListDiff, Rename};

use crate::app::App;
use crate::engine::target;

/// What one list has to report.
#[derive(Debug, Clone)]
pub struct ListReport {
    pub kind: ListKind,
    pub basis: Basis,
    /// When the last report about this list was made. `None` when there has
    /// never been one.
    ///
    /// The receipt's moment, not the marked capture's `taken_at`. Those come
    /// apart the moment somebody types `snob followers` between two runs, and
    /// this is the one that says what has actually been said out loud.
    pub since: Option<i64>,
    /// The rename history row the last report stopped at. Zero when there has
    /// never been one, which is also what an empty history reads as.
    pub history_cursor: i64,
    /// When the newest capture was taken.
    pub until: i64,
    pub diff: ListDiff,
    /// How many accounts the newest capture holds, so a report can say "three
    /// left, of a hundred and forty" without the caller counting again.
    pub total: usize,
}

impl ListReport {
    /// Whether this is a comparison that actually happened, as opposed to a
    /// baseline being laid down or a list nothing has touched.
    pub fn compared(&self) -> bool {
        matches!(self.basis, Basis::Compare { .. })
    }
}

/// Everything storage can say about one account.
#[derive(Debug, Clone)]
pub struct WatchReport {
    pub account_pk: Pk,
    /// The name, when one was ever learned. Filtered by whoever draws it, not
    /// here: this module returns data.
    pub username: Option<String>,
    pub is_self: bool,
    /// `None` when nothing of that list has ever been walked to completion.
    pub followers: Option<ListReport>,
    pub following: Option<ListReport>,
    pub renamed: Vec<Rename>,
}

impl WatchReport {
    /// The changes, in the shape the payload and the printer both want.
    pub fn changes(&self) -> Changes {
        Changes {
            followers: self.diff_of(ListKind::Followers),
            following: self.diff_of(ListKind::Following),
            renamed: self.renamed.clone(),
        }
    }

    fn diff_of(&self, kind: ListKind) -> ListDiff {
        self.report(kind)
            .map(|r| r.diff.clone())
            .unwrap_or_default()
    }

    pub fn report(&self, kind: ListKind) -> Option<&ListReport> {
        match kind {
            ListKind::Followers => self.followers.as_ref(),
            ListKind::Following => self.following.as_ref(),
        }
    }

    /// Whether anything at all has ever been walked for this account.
    ///
    /// Told apart from "nothing changed", which is what the caller would
    /// otherwise print at somebody who has never run the tool on this account.
    pub fn has_anything_stored(&self) -> bool {
        self.followers.is_some() || self.following.is_some()
    }
}

/// Reads the report without touching the network.
///
/// `advance` decides whether the marks move. `snob watch diff` looks and leaves
/// them alone — a question that changes the answer to the next question is not
/// a question anybody can ask twice — while a tick commits what it has
/// reported.
pub fn from_store(app: &App, typed: Option<&str>, advance: bool) -> Result<WatchReport> {
    let (pk, username) = resolve(app, typed)?;

    // One reading of each, taken once for the whole report. Every list marked
    // by this report carries the same two numbers, so there is no sliver
    // between two marks in which a rename is filed and then belongs to neither
    // this report nor the next.
    //
    // The head is read **before** anything is compared, so a rename filed while
    // this runs stays on the next report's side rather than being marked as
    // said without having been.
    let at = snob_core::store::now();
    let head = store::history_head(app.db().conn())?;

    let followers = list_report(app, pk, ListKind::Followers)?;
    let following = list_report(app, pk, ListKind::Following)?;

    // Looked for in one capture, not both. An account can be in the followers
    // list and the following list at once — that is what a friend is — and
    // asking about each list separately would report their rename twice.
    let renamed = match followers.as_ref().or(following.as_ref()) {
        Some(report) if report.compared() => store::renames_since(
            app.db().conn(),
            report.basis.mark_to(),
            report.history_cursor,
        )?,
        _ => Vec::new(),
    };

    if advance {
        for report in [followers.as_ref(), following.as_ref()]
            .into_iter()
            .flatten()
        {
            store::set_mark(
                app.db().conn(),
                pk,
                report.kind,
                report.basis.mark_to(),
                at,
                head,
            )?;
        }
    }

    Ok(WatchReport {
        account_pk: pk,
        username,
        is_self: app.viewer().pk == pk,
        followers,
        following,
        renamed,
    })
}

fn list_report(app: &App, pk: Pk, kind: ListKind) -> Result<Option<ListReport>> {
    let conn = app.db().conn();

    // From the view, so an interrupted walk cannot become a basis for
    // comparison. It is the same guard the static crossings already lean on,
    // and it matters more here: the accounts missing from a half-walked list
    // would be reported as people who left.
    let Some(latest) = snapshots::latest_complete(conn, pk, kind)? else {
        return Ok(None);
    };

    let mark = store::mark(conn, pk, kind)?;
    // A receipt whose capture has been pruned carries no baseline, so it is
    // handed to `decide` as the absence it is and the next report starts over.
    // What survives is `compared_at`, which is where the rename window starts.
    let Some(basis) = Basis::decide(mark.and_then(|m| m.snapshot_id), Some(latest.id)) else {
        return Ok(None);
    };

    let diff = match basis {
        // Nothing is reported and nothing is read: a baseline has no earlier
        // capture, and an unchanged list has nothing new in it. Not reading the
        // members is the whole reason the common case is cheap.
        Basis::Baseline { .. } | Basis::Unchanged { .. } => ListDiff::default(),
        Basis::Compare { before, after } => ListDiff::between(
            &snapshots::members(conn, before)?,
            &snapshots::members(conn, after)?,
        ),
    };

    Ok(Some(ListReport {
        kind,
        basis,
        since: mark.map(|m| m.compared_at),
        history_cursor: mark.map(|m| m.history_cursor).unwrap_or_default(),
        until: latest.taken_at.unwrap_or_default(),
        diff,
        total: latest.member_count as usize,
    }))
}

/// Which account this is about, from storage alone.
///
/// Deliberately not `target::from_store`: that one refuses with a sentence
/// about `--cache`, which is a flag this command does not have. What the user
/// has to do here is walk the account once, and the refusal says so.
fn resolve(app: &App, typed: Option<&str>) -> Result<(Pk, Option<String>)> {
    let Some(typed) = typed else {
        let viewer = app.viewer();
        return Ok((viewer.pk, viewer.username.clone()));
    };

    let name = target::clean(typed);
    let Some(pk) = accounts::find_pk_by_username(app.db().conn(), name)? else {
        bail!(
            "nothing is stored about @{}. Run \"snob followers {}\" once and the monitor \
             will have something to compare against from then on.",
            snob_core::model::printable(name),
            snob_core::model::printable(name),
        );
    };

    Ok((pk, users::name(app.db().conn(), pk)?))
}

/// Users in a capture, for a caller that has an id and wants the people.
///
/// Re-exported rather than reached for through `snapshots` so that the monitor
/// has one door to the members and the guard about which captures are readable
/// stays on this side of it.
pub fn members(app: &App, snapshot_id: i64) -> Result<Vec<User>> {
    Ok(snapshots::members(app.db().conn(), snapshot_id)?)
}
