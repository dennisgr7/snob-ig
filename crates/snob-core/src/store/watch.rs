//! The monitor's own tables: what has been reported, and what each run did.
//!
//! The distinction this file exists to keep is between a *capture* and a
//! *receipt*. `snapshots` records that a list was walked; `watch_marks` records
//! that a walk was reported. They come apart the moment anybody runs
//! `snob followers` by hand between two ticks, and the whole correctness of the
//! diff rests on reading the receipt rather than guessing at it from the
//! captures.

use rusqlite::{Connection, OptionalExtension, params};

use super::{StoreError, pk_from_sql, pk_to_sql};
use crate::Pk;
use crate::model::ListKind;
use crate::watch::Rename;

/// The receipt for one account and list: what was reported, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// The capture that was reported against. `None` once it has been pruned,
    /// which means the baseline is gone and the next run has to lay a new one.
    pub snapshot_id: Option<i64>,
    /// When the report was made. What gets shown to a person; the rename window
    /// is bounded by [`Mark::history_cursor`] instead, for the reason the
    /// column's comment in `002_watch.sql` gives.
    pub compared_at: i64,
    /// The last `username_history` row that has been reported.
    pub history_cursor: i64,
}

/// The receipt for this account and list, if there is one.
pub fn mark(conn: &Connection, account_pk: Pk, kind: ListKind) -> Result<Option<Mark>, StoreError> {
    let mark = conn
        .query_row(
            "SELECT snapshot_id, compared_at, history_cursor FROM watch_marks
             WHERE account_pk = ?1 AND kind = ?2",
            params![pk_to_sql(account_pk), kind.as_str()],
            |row| {
                Ok(Mark {
                    snapshot_id: row.get(0)?,
                    compared_at: row.get(1)?,
                    history_cursor: row.get(2)?,
                })
            },
        )
        .optional()?;
    Ok(mark)
}

/// How long a capture is kept once nothing needs it.
///
/// A monitor on a six-hour schedule leaves four captures a day per list, each
/// holding a row per account. On a thousand-follower account that is millions
/// of rows a year, for a history nobody reads: the diff only ever compares
/// against the last reported capture, and everything older is there in case
/// somebody wants to look back. Thirty days is enough to look back over.
pub const KEEP_FOR_SECS: i64 = 30 * 24 * 3_600;

/// How many settled deliveries to keep, for `status` and for anybody wondering
/// where a report went. Older ones are only a record that something arrived.
pub const KEEP_DELIVERIES_FOR_SECS: i64 = 7 * 24 * 3_600;

/// Removes captures nothing needs any more.
///
/// Three things are kept whatever their age, and each is load-bearing:
///
/// - **The capture every mark points at.** It is the baseline of the next diff.
///   Deleting it sets `watch_marks.snapshot_id` to NULL, and the next run then
///   lays a new baseline and reports nothing — one silently missed report per
///   pruned mark.
/// - **The newest complete capture of each account and list**, which is what
///   the cache serves and what a crossing reads.
/// - **Anything an interrupted walk could still be resumed from**, which is any
///   incomplete capture: `delete_partials` already clears those when a walk
///   starts, and taking one here would end a resume somebody is mid-way through.
///
/// `secure_delete` is on for the whole connection — `store::configure` says it
/// is there for "whatever the monitor ends up expiring", which is this — so the
/// rows go rather than being unlinked with their contents still readable.
pub fn prune(conn: &Connection, now: i64) -> Result<usize, StoreError> {
    let cutoff = now - KEEP_FOR_SECS;

    let removed = conn.execute(
        "DELETE FROM snapshots
         WHERE complete = 1
           AND taken_at IS NOT NULL
           AND taken_at < ?1
           AND id NOT IN (SELECT snapshot_id FROM watch_marks WHERE snapshot_id IS NOT NULL)
           AND id NOT IN (
             SELECT id FROM (
               SELECT id, row_number() OVER (
                 PARTITION BY account_pk, kind ORDER BY taken_at DESC, id DESC
               ) AS rank
               FROM usable_snapshots
             ) WHERE rank = 1
           )",
        params![cutoff],
    )?;

    // A report too old to be news stops being owed.
    //
    // `MAX_AGE_SECS` used to be applied only inside `deliveries::failed`, which
    // is only reached by a run that tries the report — so a report nothing ever
    // retried aged without limit. Three ways that showed: a year-old row was
    // still `due` and went out as news on the next run; removing `[webhook]`
    // from the configuration left rows owed forever with `status` promising the
    // next run would try them; and a walk that failed before the delivery step
    // stopped the queue draining even when the webhook was fine.
    //
    // Marked rather than deleted, so `status` can still say what became of it.
    conn.execute(
        "UPDATE watch_deliveries
         SET state = 'expired', settled_at = ?1,
             next_try_at = NULL,
             last_error = coalesce(last_error, 'it grew too old to be news')
         WHERE state = 'pending' AND created_at < ?2",
        params![now, now - super::deliveries::MAX_AGE_SECS],
    )?;

    // Settled deliveries are a record, not work. Pending ones are never deleted
    // here: `deliveries::failed` is what decides when one stops being owed, and
    // removing one from under it would lose a report that was still going to be
    // tried.
    conn.execute(
        "DELETE FROM watch_deliveries
         WHERE state != 'pending' AND settled_at IS NOT NULL AND settled_at < ?1",
        params![now - KEEP_DELIVERIES_FOR_SECS],
    )?;

    // Runs are a log, and nothing else read them back. `--every 30m` over three
    // accounts writes some fifty thousand rows a year; the table postdates the
    // rest of retention, which is how it came to have none.
    conn.execute(
        "DELETE FROM watch_runs WHERE started_at < ?1",
        params![now - KEEP_RUNS_FOR_SECS],
    )?;

    Ok(removed)
}

/// How long the run log is kept. Long enough for `status` to describe a bad
/// week, short enough that a monitor on a half-hourly schedule does not
/// accumulate rows forever.
pub const KEEP_RUNS_FOR_SECS: i64 = 30 * 24 * 3_600;

/// What one run of the monitor did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub account_pk: Pk,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub requests: u32,
    /// `ExitCode::as_str()`: the same vocabulary as the README's table and as
    /// `$?`, so a caller is told the same thing by the same name wherever it
    /// reads it.
    pub outcome: Option<String>,
    pub changes: u32,
}

/// Records a run, whatever came of it.
///
/// **Including the ones that reported nothing**, which is the whole reason this
/// table is not just `watch_marks` again. A mark only moves when a list was
/// actually compared, so a monitor sitting in a cooldown for two days moves
/// nothing — and from outside that is identical to a monitor that was killed on
/// Monday. `snob watch status` needs to tell those apart, and this is what lets
/// it: the marks say when something was last *reported*, this says when the
/// thing last *ran*.
pub fn record_run(conn: &Connection, run: &Run) -> Result<i64, StoreError> {
    conn.execute(
        "INSERT INTO watch_runs (account_pk, started_at, finished_at, requests, outcome, changes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            pk_to_sql(run.account_pk),
            run.started_at,
            run.finished_at,
            run.requests,
            run.outcome,
            run.changes,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The most recent run of one account.
///
/// Per account, because a run covers every configured one and each writes its
/// own row. Without the predicate the answer was always whichever account came
/// last in the file, so `status` said "found nothing" on a tick where an earlier
/// account had found five, and reported `ok` for a tick where another was rate
/// limited. It also made `watch_runs_lookup` — the index 002 added for exactly
/// this query — unusable, so the query scanned and sorted in a temp b-tree.
pub fn last_run(conn: &Connection, account_pk: Pk) -> Result<Option<Run>, StoreError> {
    let run = conn
        .query_row(
            "SELECT account_pk, started_at, finished_at, requests, outcome, changes
             FROM watch_runs WHERE account_pk = ?1
             ORDER BY started_at DESC, id DESC LIMIT 1",
            params![pk_to_sql(account_pk)],
            |row| {
                Ok(Run {
                    account_pk: pk_from_sql(row.get(0)?),
                    started_at: row.get(1)?,
                    finished_at: row.get(2)?,
                    requests: row.get(3)?,
                    outcome: row.get(4)?,
                    changes: row.get(5)?,
                })
            },
        )
        .optional()?;
    Ok(run)
}

/// When the monitor last started a run, for any account.
///
/// Not per account, unlike [`last_run`], and that is the question being asked:
/// one loop covers every watched account, so "when did this last run" is one
/// moment. The scheduled loop seeds its `--every` clock from it.
///
/// It has to, and it did not. The clock started again at every process start, so
/// `--every 24h` on a machine powered on from eight to six, or under a
/// supervisor restarting more often than the interval, never reached its first
/// run — while `status` read this same table and said "it has not run yet".
pub fn last_started(conn: &Connection) -> Result<Option<i64>, StoreError> {
    let started = conn
        .query_row(
            "SELECT started_at FROM watch_runs ORDER BY started_at DESC, id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(started)
}

/// One receipt, with what it is a receipt for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountMark {
    pub account_pk: Pk,
    pub kind: ListKind,
    pub snapshot_id: Option<i64>,
    pub compared_at: i64,
}

/// Every receipt there is, newest first.
///
/// What `snob watch status` reads. It is the only way to tell a monitor that
/// has been running and finding nothing from one that stopped weeks ago, and
/// those look identical from outside.
pub fn all_marks(conn: &Connection) -> Result<Vec<AccountMark>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT account_pk, kind, snapshot_id, compared_at
         FROM watch_marks ORDER BY compared_at DESC, account_pk, kind",
    )?;
    let rows = stmt.query_map([], |row| {
        let kind: String = row.get(1)?;
        Ok((row.get::<_, i64>(0)?, kind, row.get(2)?, row.get(3)?))
    })?;

    let mut marks = Vec::new();
    for row in rows {
        let (pk, kind, snapshot_id, compared_at) = row?;
        // The column has a CHECK that only allows the two, so anything else
        // means the file was written by something that is not this program.
        // Skipped rather than guessed at: reporting a row as "followers"
        // because it could not be read would answer about a list nobody asked
        // about.
        let Ok(kind) = kind.parse::<ListKind>() else {
            tracing::warn!(kind, "a watch mark names a list this version does not know");
            continue;
        };
        marks.push(AccountMark {
            account_pk: pk_from_sql(pk),
            kind,
            snapshot_id,
            compared_at,
        });
    }
    Ok(marks)
}

/// The newest row in the rename history, as an id.
///
/// Read once when a report is made and stored on the mark, so that the next
/// report asks for everything after it. Anything filed later has a larger id by
/// construction, which is what makes "already reported" a decidable question
/// rather than a comparison of two whole-second timestamps.
///
/// Zero on an empty history, which is also what a mark that has never reported
/// carries — so the first window is "everything", with no special case.
pub fn history_head(conn: &Connection) -> Result<i64, StoreError> {
    let head = conn.query_row(
        "SELECT coalesce(MAX(id), 0) FROM username_history",
        [],
        |row| row.get(0),
    )?;
    Ok(head)
}

/// Records that everything up to this capture has been reported.
///
/// Deliberately takes a `&Connection` rather than opening its own transaction:
/// the caller has to be able to put this and the queued notice in **one**
/// transaction, and a function that begins its own makes that impossible. What
/// order they go in, and why, is `engine::watch`'s to explain.
///
/// `at` and `history_cursor` are passed in rather than read here, so that every
/// list marked by one report carries the same two numbers. Read inside, two
/// lists marked a moment apart would leave a sliver between them in which a
/// rename is filed and then belongs to neither report.
pub fn set_mark(
    conn: &Connection,
    account_pk: Pk,
    kind: ListKind,
    snapshot_id: i64,
    at: i64,
    history_cursor: i64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO watch_marks (account_pk, kind, snapshot_id, compared_at, history_cursor)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(account_pk, kind) DO UPDATE SET
             snapshot_id    = excluded.snapshot_id,
             compared_at    = excluded.compared_at,
             history_cursor = excluded.history_cursor",
        params![
            pk_to_sql(account_pk),
            kind.as_str(),
            snapshot_id,
            at,
            history_cursor
        ],
    )?;
    Ok(())
}

/// Commits a report: queues it, and moves the marks it makes stale.
///
/// **One transaction, and the order inside it is the point.** A change that has
/// been reported is one the next run will not find, because the next run
/// compares against the mark this moves. So the report has to be durable before
/// the mark that retires it — and both have to land together, or a process
/// killed in between loses a window nobody will ever report.
///
/// The direction it fails in is deliberate. If the queue row commits and the
/// send never happens, the report goes out late. If the mark moved without the
/// row, the report is gone. At-least-once is the only defensible choice here,
/// which is why every report carries an id the receiver can deduplicate on.
///
/// `marks` names only the lists this report actually spoke about. A list that
/// was refused is left out by the caller and its mark stays where it was.
pub fn commit_report(
    store: &mut super::Store,
    account_pk: Pk,
    marks: &[(ListKind, i64)],
    at: i64,
    history_cursor: i64,
    delivery: Option<(&str, &str)>,
) -> Result<Option<i64>, StoreError> {
    let tx = store.conn_mut().transaction()?;

    let queued = match delivery {
        Some((run_id, body)) => Some(super::deliveries::enqueue(
            &tx, run_id, account_pk, body, at,
        )?),
        None => None,
    };

    for &(kind, snapshot_id) in marks {
        set_mark(&tx, account_pk, kind, snapshot_id, at, history_cursor)?;
    }

    tx.commit()?;
    Ok(queued)
}

/// Renames filed in an interval, for the accounts in one capture.
///
/// Scoped to the capture's members on purpose. `username_history` accumulates
/// every account this tool has ever seen, including people who left years ago
/// and accounts that only ever turned up in somebody else's list; reporting all
/// of them would bury the handful the user actually follows.
///
/// The window is everything filed **after** `since`, which is the id the last
/// report stopped at. Ids rather than timestamps because `changed_at` is in
/// whole seconds: a rename filed in the same second as a report is neither
/// clearly inside the last window nor clearly inside the next, so a timestamp
/// bound either announces it twice or loses it. The id decides.
///
/// `from` is the name they had at the start of the window — the **earliest**
/// entry in it, not the latest, so somebody who changed their name twice is
/// reported as one move from where they started rather than only their last
/// hop. `to` is what they are called *now*, read from `users`: if they have
/// renamed again since, that is still the true and more useful answer, because
/// the reader is going to go and look them up.
pub fn renames_since(
    conn: &Connection,
    snapshot_id: i64,
    since: i64,
) -> Result<Vec<Rename>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT h.pk, h.username, u.username, h.changed_at
         FROM username_history h
         JOIN users u            ON u.pk = h.pk
         JOIN snapshot_members m ON m.user_pk = h.pk AND m.snapshot_id = ?1
         WHERE h.id > ?2
           AND h.id = (
             SELECT MIN(e.id) FROM username_history e
             WHERE e.pk = h.pk AND e.id > ?2
           )
         ORDER BY h.id",
    )?;

    let rows = stmt.query_map(params![snapshot_id, since], |row| {
        Ok(Rename {
            pk: pk_from_sql(row.get(0)?),
            from: row.get(1)?,
            to: row.get(2)?,
            at: row.get(3)?,
        })
    })?;

    // A name that has not actually moved is not a rename. The query cannot rule
    // this out on its own: somebody who changed their name and changed it back
    // has two history rows and a current name equal to the first one, and
    // reporting "@someone is now @someone" is noise that reads as a bug.
    Ok(rows
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|r| r.from != r.to)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::User;
    use crate::store::{Store, accounts, snapshots, users};

    fn user(pk: Pk, name: &str) -> User {
        User {
            pk,
            username: name.into(),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    /// An account with one capture holding `members`, ready to be marked.
    fn account_with_capture(db: &mut Store, pk: Pk, members: &[User]) -> i64 {
        users::ensure(db.conn(), pk).unwrap();
        accounts::upsert(db.conn(), pk, true).unwrap();
        let opened = snapshots::begin(db.conn(), pk, ListKind::Followers, None).unwrap();
        snapshots::save_page(db, opened.id, members, None).unwrap();
        snapshots::close(db.conn(), opened.id, crate::model::StopReason::Completed).unwrap();
        opened.id
    }

    #[test]
    fn an_account_that_was_never_watched_has_no_mark() {
        let db = Store::in_memory().unwrap();
        assert_eq!(mark(db.conn(), 7, ListKind::Followers).unwrap(), None);
    }

    #[test]
    fn a_mark_is_written_and_read_back() {
        let mut db = Store::in_memory().unwrap();
        let id = account_with_capture(&mut db, 7, &[user(1, "one")]);

        set_mark(db.conn(), 7, ListKind::Followers, id, 1_700, 12).unwrap();
        assert_eq!(
            mark(db.conn(), 7, ListKind::Followers).unwrap(),
            Some(Mark {
                snapshot_id: Some(id),
                compared_at: 1_700,
                history_cursor: 12,
            })
        );
    }

    /// The two lists are marked separately. Sharing a row would make walking
    /// the followers list look like the following list had been reported too,
    /// and the changes in it would never be announced.
    #[test]
    fn the_two_lists_are_marked_apart() {
        let mut db = Store::in_memory().unwrap();
        let id = account_with_capture(&mut db, 7, &[user(1, "one")]);

        set_mark(db.conn(), 7, ListKind::Followers, id, 1_700, 0).unwrap();
        assert_eq!(mark(db.conn(), 7, ListKind::Following).unwrap(), None);
    }

    #[test]
    fn marking_again_moves_the_mark_rather_than_failing() {
        let mut db = Store::in_memory().unwrap();
        let first = account_with_capture(&mut db, 7, &[user(1, "one")]);
        let second = account_with_capture(&mut db, 7, &[user(1, "one")]);

        set_mark(db.conn(), 7, ListKind::Followers, first, 1_700, 3).unwrap();
        set_mark(db.conn(), 7, ListKind::Followers, second, 1_800, 9).unwrap();
        assert_eq!(
            mark(db.conn(), 7, ListKind::Followers).unwrap(),
            Some(Mark {
                snapshot_id: Some(second),
                compared_at: 1_800,
                history_cursor: 9,
            })
        );
    }

    /// What happens when retention takes the capture a mark points at.
    ///
    /// The row has to survive with its `compared_at` intact: the baseline is
    /// gone, so the next run lays down a new one and reports nothing — but the
    /// left edge of the rename window is still known, which is why this is
    /// `ON DELETE SET NULL` and not `CASCADE`. With CASCADE the whole receipt
    /// would go and the next run would have no idea which renames it had
    /// already announced.
    #[test]
    fn pruning_the_marked_capture_leaves_the_receipt_behind() {
        let mut db = Store::in_memory().unwrap();
        let id = account_with_capture(&mut db, 7, &[user(1, "one")]);
        set_mark(db.conn(), 7, ListKind::Followers, id, 1_700, 5).unwrap();

        db.conn()
            .execute("DELETE FROM snapshots WHERE id = ?1", params![id])
            .unwrap();

        assert_eq!(
            mark(db.conn(), 7, ListKind::Followers).unwrap(),
            Some(Mark {
                snapshot_id: None,
                compared_at: 1_700,
                history_cursor: 5,
            }),
            "the baseline is gone but what has already been reported is not"
        );
    }

    #[test]
    fn a_rename_inside_the_window_is_reported_with_both_names() {
        let mut db = Store::in_memory().unwrap();
        let id = account_with_capture(&mut db, 7, &[user(1, "before")]);

        // The walk that sees the new name is what files the history row.
        users::upsert(db.conn(), &user(1, "after")).unwrap();

        let found = renames_since(db.conn(), id, 0).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pk, 1);
        assert_eq!(found[0].from, "before");
        assert_eq!(found[0].to, "after");
    }

    /// The bound that stops one event being announced by two runs in a row.
    ///
    /// The whole point of bounding by id: this holds even though both the
    /// rename and both reports happen inside the same second, which is exactly
    /// the case a `changed_at` comparison cannot decide.
    #[test]
    fn a_rename_already_reported_is_not_reported_again() {
        let mut db = Store::in_memory().unwrap();
        let id = account_with_capture(&mut db, 7, &[user(1, "before")]);
        users::upsert(db.conn(), &user(1, "after")).unwrap();

        assert_eq!(renames_since(db.conn(), id, 0).unwrap().len(), 1);

        let head = history_head(db.conn()).unwrap();
        assert!(
            renames_since(db.conn(), id, head).unwrap().is_empty(),
            "the run that reported it stopped at that id, so the next window starts after it"
        );
    }

    /// Zero is what a mark that has never reported carries, and an empty
    /// history has to agree with it — otherwise the first window is off by one
    /// row in whichever direction the disagreement goes.
    #[test]
    fn an_empty_history_starts_where_an_unreported_mark_does() {
        let db = Store::in_memory().unwrap();
        assert_eq!(history_head(db.conn()).unwrap(), 0);
    }

    /// Back-dates a capture, so retention can be tested without waiting a
    /// month. Raw SQL for the same reason `tests/cache.rs` uses it: there is no
    /// legitimate way to write a `taken_at` in the past.
    fn age(db: &Store, id: i64, seconds: i64) {
        db.conn()
            .execute(
                "UPDATE snapshots SET taken_at = taken_at - ?2, started_at = started_at - ?2
                 WHERE id = ?1",
                params![id, seconds],
            )
            .unwrap();
    }

    #[test]
    fn an_old_capture_nothing_needs_is_removed_with_its_members() {
        let mut db = Store::in_memory().unwrap();
        let old = account_with_capture(&mut db, 7, &[user(1, "one")]);
        let newer = account_with_capture(&mut db, 7, &[user(1, "one")]);
        age(&db, old, KEEP_FOR_SECS + 1);

        assert_eq!(prune(db.conn(), crate::store::now()).unwrap(), 1);
        assert!(snapshots::find_usable(db.conn(), old).unwrap().is_none());
        assert!(snapshots::find_usable(db.conn(), newer).unwrap().is_some());

        let members: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM snapshot_members WHERE snapshot_id = ?1",
                params![old],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(members, 0, "the members go with the capture");
    }

    /// The rule the whole of retention has to respect. Taking the marked
    /// capture leaves the next run with no baseline, so it reports nothing —
    /// one silently missed report, and no error anywhere.
    #[test]
    fn the_capture_a_mark_points_at_is_kept_however_old_it_is() {
        let mut db = Store::in_memory().unwrap();
        let marked = account_with_capture(&mut db, 7, &[user(1, "one")]);
        // A newer one, so the marked capture is not kept merely for being last.
        account_with_capture(&mut db, 7, &[user(1, "one")]);

        set_mark(db.conn(), 7, ListKind::Followers, marked, 1_000, 0).unwrap();
        age(&db, marked, KEEP_FOR_SECS * 10);

        prune(db.conn(), crate::store::now()).unwrap();
        assert!(
            snapshots::find_usable(db.conn(), marked).unwrap().is_some(),
            "the baseline of the next diff was pruned"
        );
    }

    /// However old everything is, the newest of each list stays: it is what the
    /// cache serves and what a crossing reads.
    #[test]
    fn the_newest_capture_of_each_list_is_always_kept() {
        let mut db = Store::in_memory().unwrap();
        let followers = account_with_capture(&mut db, 7, &[user(1, "one")]);

        users::ensure(db.conn(), 7).unwrap();
        let opened = snapshots::begin(db.conn(), 7, ListKind::Following, None).unwrap();
        snapshots::save_page(&mut db, opened.id, &[user(2, "two")], None).unwrap();
        snapshots::close(db.conn(), opened.id, crate::model::StopReason::Completed).unwrap();

        age(&db, followers, KEEP_FOR_SECS * 5);
        age(&db, opened.id, KEEP_FOR_SECS * 5);

        assert_eq!(prune(db.conn(), crate::store::now()).unwrap(), 0);
        assert!(
            snapshots::find_usable(db.conn(), followers)
                .unwrap()
                .is_some()
        );
        assert!(
            snapshots::find_usable(db.conn(), opened.id)
                .unwrap()
                .is_some()
        );
    }

    /// An interrupted walk is resumed from its rows. Taking them would end a
    /// resume somebody is in the middle of, and the resume window is what
    /// decides when a partial stops being useful — not this.
    #[test]
    fn an_unfinished_walk_is_left_alone() {
        let mut db = Store::in_memory().unwrap();
        account_with_capture(&mut db, 7, &[user(1, "one")]);

        let partial = snapshots::begin(db.conn(), 7, ListKind::Followers, None)
            .unwrap()
            .id;
        snapshots::save_page(&mut db, partial, &[user(3, "three")], Some("cursor")).unwrap();
        age(&db, partial, KEEP_FOR_SECS * 5);

        prune(db.conn(), crate::store::now()).unwrap();
        let still_there: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM snapshots WHERE id = ?1",
                params![partial],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(still_there, 1);
    }

    /// A report still owed is work, not history. Deleting one would lose a
    /// change that was still going to be delivered.
    #[test]
    fn a_report_still_waiting_is_never_pruned() {
        let mut db = Store::in_memory().unwrap();
        account_with_capture(&mut db, 7, &[user(1, "one")]);

        let now = crate::store::now();
        let long_ago = now - KEEP_DELIVERIES_FOR_SECS * 10;
        // Owed, and young enough to still be news — the other half of the rule
        // is the test below.
        let owed = crate::store::deliveries::enqueue(db.conn(), "owed", 7, "{}", now - 60).unwrap();
        let done = crate::store::deliveries::enqueue(db.conn(), "done", 7, "{}", long_ago).unwrap();
        crate::store::deliveries::delivered(db.conn(), done, 200, long_ago).unwrap();

        prune(db.conn(), now).unwrap();

        assert_eq!(
            crate::store::deliveries::state(db.conn(), owed)
                .unwrap()
                .as_deref(),
            Some("pending"),
            "a report that has not been delivered is still owed"
        );
        assert!(
            crate::store::deliveries::state(db.conn(), done)
                .unwrap()
                .is_none(),
            "a settled one from a week ago is only a record"
        );
    }

    /// The other half: a report nothing ever retried does not stay owed forever.
    ///
    /// `MAX_AGE_SECS` used to be applied only by `deliveries::failed`, which is
    /// reached only by a run that *tries* the report — so removing `[webhook]`
    /// from the configuration, or a walk that failed before the delivery step,
    /// left rows owed indefinitely while `status` promised the next run would
    /// try them. Marked rather than deleted, so `status` can still say why.
    #[test]
    fn a_report_too_old_to_be_news_stops_being_owed() {
        let mut db = Store::in_memory().unwrap();
        account_with_capture(&mut db, 7, &[user(1, "one")]);

        let now = crate::store::now();
        let stale = now - crate::store::deliveries::MAX_AGE_SECS - 1;
        let id = crate::store::deliveries::enqueue(db.conn(), "stale", 7, "{}", stale).unwrap();
        assert_eq!(crate::store::deliveries::pending(db.conn()).unwrap(), 1);

        prune(db.conn(), now).unwrap();

        assert_eq!(
            crate::store::deliveries::state(db.conn(), id)
                .unwrap()
                .as_deref(),
            Some("expired")
        );
        assert!(
            crate::store::deliveries::due(db.conn(), now, 10)
                .unwrap()
                .is_empty(),
            "and it does not go out as news"
        );
    }

    /// Runs are a log. `--every 30m` over three accounts writes some fifty
    /// thousand rows a year, and nothing was removing any of them: the table
    /// arrived after the rest of retention had been written.
    #[test]
    fn the_run_log_does_not_grow_without_end() {
        let mut db = Store::in_memory().unwrap();
        account_with_capture(&mut db, 7, &[user(1, "one")]);

        let now = crate::store::now();
        for age in [KEEP_RUNS_FOR_SECS + 1, 60] {
            record_run(
                db.conn(),
                &Run {
                    account_pk: 7,
                    started_at: now - age,
                    finished_at: Some(now - age),
                    requests: 1,
                    outcome: Some("ok".to_string()),
                    changes: 0,
                },
            )
            .unwrap();
        }

        prune(db.conn(), now).unwrap();

        let left: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM watch_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 1, "the old one goes and the recent one stays");
        assert_eq!(
            last_run(db.conn(), 7).unwrap().unwrap().started_at,
            now - 60
        );
    }

    /// A run belongs to an account, and `status` speaks per account.
    ///
    /// One query with no predicate always answered with whichever account came
    /// last in the configuration, so `status` said "found nothing" on a tick
    /// where an earlier account had found five.
    #[test]
    fn the_last_run_is_the_last_run_of_that_account() {
        let mut db = Store::in_memory().unwrap();
        account_with_capture(&mut db, 7, &[user(1, "one")]);
        account_with_capture(&mut db, 8, &[user(2, "two")]);

        let now = crate::store::now();
        for (pk, changes) in [(7u64, 5u32), (8, 0)] {
            record_run(
                db.conn(),
                &Run {
                    account_pk: pk,
                    started_at: now,
                    finished_at: Some(now),
                    requests: 1,
                    outcome: Some("ok".to_string()),
                    changes,
                },
            )
            .unwrap();
        }

        assert_eq!(last_run(db.conn(), 7).unwrap().unwrap().changes, 5);
        assert_eq!(last_run(db.conn(), 8).unwrap().unwrap().changes, 0);
    }

    /// `username_history` holds everybody this tool has ever seen. A diff about
    /// one list may only speak about the people in it.
    #[test]
    fn somebody_outside_the_capture_is_not_reported() {
        let mut db = Store::in_memory().unwrap();
        let id = account_with_capture(&mut db, 7, &[user(1, "one")]);

        users::upsert(db.conn(), &user(2, "stranger")).unwrap();
        users::upsert(db.conn(), &user(2, "stranger_renamed")).unwrap();

        assert!(renames_since(db.conn(), id, 0).unwrap().is_empty());
    }

    /// Somebody who changed their name twice moved once, from where they
    /// started. Reading the latest history row instead would report the middle
    /// name as the old one, which is a name the user never saw.
    #[test]
    fn two_renames_in_one_window_are_one_move_from_the_first_name() {
        let mut db = Store::in_memory().unwrap();
        let id = account_with_capture(&mut db, 7, &[user(1, "first")]);

        users::upsert(db.conn(), &user(1, "second")).unwrap();
        users::upsert(db.conn(), &user(1, "third")).unwrap();

        let found = renames_since(db.conn(), id, 0).unwrap();
        assert_eq!(found.len(), 1, "one account moved, so one line about it");
        assert_eq!(found[0].from, "first");
        assert_eq!(found[0].to, "third");
    }

    /// Renamed and renamed back is not news. The history has two rows and the
    /// current name equals the original, which would otherwise be announced as
    /// "@someone is now @someone".
    #[test]
    fn a_name_that_came_back_to_where_it_started_is_not_a_rename() {
        let mut db = Store::in_memory().unwrap();
        let id = account_with_capture(&mut db, 7, &[user(1, "original")]);

        users::upsert(db.conn(), &user(1, "briefly")).unwrap();
        users::upsert(db.conn(), &user(1, "original")).unwrap();

        assert!(renames_since(db.conn(), id, 0).unwrap().is_empty());
    }
}
