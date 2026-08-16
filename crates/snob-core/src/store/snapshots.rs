//! Snapshots: a follower or following list captured at one moment.
//!
//! A snapshot is opened when the walk begins, filled in page by page, and
//! closed at the end. Each page commits in its own transaction, and that is
//! what makes "save partial progress" not an action to be executed in time but
//! simply a matter of stopping: whatever committed is durable even if the
//! process dies outright.

use rusqlite::{Connection, OptionalExtension, params};

use super::{Store, StoreError, now, pk_from_sql, pk_to_sql};
use crate::Pk;
use crate::model::{ListKind, StopReason, User};

/// How long an interrupted walk may still be resumed.
///
/// Past the window it is not resumed: stitching two separate moments together
/// produces a list that reflects no single instant, and comparing it against
/// another invents arrivals and departures that never happened.
pub const RESUME_WINDOW_SECS: i64 = 15 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: i64,
    pub account_pk: Pk,
    pub kind: ListKind,
    pub started_at: i64,
    pub taken_at: Option<i64>,
    pub complete: bool,
    pub member_count: u64,
    pub declared_count: Option<u64>,
    pub pages: u32,
    pub requests: u32,
    pub next_cursor: Option<String>,
    pub resumes: u32,
}

/// What changed when a page was saved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedPage {
    /// How many users were not already in the snapshot. Instagram's pagination
    /// repeats accounts across pages, so this rarely matches what arrived.
    pub added: usize,
    /// Accounts that have renamed themselves since we last saw them.
    pub renamed: Vec<(Pk, String)>,
}

/// The snapshot a walk has just opened: its id, and the moment it opened.
///
/// A struct rather than a pair because the caller needs both and they are not
/// interchangeable. The timestamp is handed back rather than left in the row
/// because it is one end of the interval the finished list describes, and the
/// caller has to have the number the row actually holds — not one it read a
/// moment before or after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened {
    pub id: i64,
    pub started_at: i64,
}

/// Starts a new snapshot.
pub fn begin(
    conn: &Connection,
    account_pk: Pk,
    kind: ListKind,
    declared_count: Option<u64>,
) -> Result<Opened, StoreError> {
    let started_at = now();
    conn.execute(
        "INSERT INTO snapshots (account_pk, kind, source, started_at, declared_count)
         VALUES (?1, ?2, 'live', ?3, ?4)",
        params![
            pk_to_sql(account_pk),
            kind.as_str(),
            started_at,
            declared_count.map(|v| v as i64),
        ],
    )?;
    Ok(Opened {
        id: conn.last_insert_rowid(),
        started_at,
    })
}

/// Looks for an interrupted walk that can still be continued.
pub fn resumable(
    conn: &Connection,
    account_pk: Pk,
    kind: ListKind,
) -> Result<Option<Snapshot>, StoreError> {
    let cutoff = now() - RESUME_WINDOW_SECS;
    let snapshot = conn
        .query_row(
            "SELECT id, account_pk, kind, started_at, taken_at, complete, member_count,
                    declared_count, pages, requests, next_cursor, resumes
             FROM snapshots
             WHERE account_pk = ?1 AND kind = ?2 AND complete = 0
               AND next_cursor IS NOT NULL AND started_at >= ?3
             ORDER BY started_at DESC LIMIT 1",
            params![pk_to_sql(account_pk), kind.as_str(), cutoff],
            row_to_snapshot,
        )
        .optional()?;
    Ok(snapshot)
}

/// Records that an existing snapshot is being continued.
pub fn mark_resumed(conn: &Connection, id: i64) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE snapshots SET resumes = resumes + 1 WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Saves a whole page in a single transaction: the users, their membership in
/// the snapshot, and the cursor advance.
///
/// Committing here rather than at the end is what makes partial progress
/// durable.
pub fn save_page(
    store: &mut Store,
    id: i64,
    users: &[User],
    cursor: Option<&str>,
) -> Result<SavedPage, StoreError> {
    let tx = store.conn_mut().transaction()?;

    let first_ordinal: i64 = tx.query_row(
        "SELECT member_count FROM snapshots WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;

    let mut result = SavedPage::default();

    {
        // Compiled once for the whole page rather than once per account. This is
        // the only per-account loop in the program, and `Connection::execute`
        // prepares its statement every call.
        let mut member = tx.prepare_cached(
            "INSERT OR IGNORE INTO snapshot_members (snapshot_id, user_pk, ordinal)
             VALUES (?1, ?2, ?3)",
        )?;

        for u in users {
            if let Some(old) = super::users::upsert(&tx, u)? {
                result.renamed.push((u.pk, old));
            }

            // The ordinal counts only the rows that go in, so it advances with
            // `added` alone. Adding the position within the page as well used to
            // inflate it on every repeat — and Instagram repeats accounts across
            // pages, as the OR IGNORE above exists to absorb — which left later
            // pages starting at a lower ordinal than earlier ones had reached, so
            // `members` returned them out of the order Instagram served them in.
            //
            // The insert's own answer rather than `tx.changes()`: it is the count
            // for this statement, where the connection-wide one is whatever ran
            // last and would follow `upsert` if a statement were ever added
            // between the two.
            let inserted = member.execute(params![
                id,
                pk_to_sql(u.pk),
                first_ordinal + result.added as i64
            ])?;
            if inserted > 0 {
                result.added += 1;
            }
        }
    }

    tx.execute(
        "UPDATE snapshots
         SET member_count = member_count + ?2,
             pages        = pages + 1,
             requests     = requests + 1,
             next_cursor  = ?3
         WHERE id = ?1",
        params![id, result.added as i64, cursor],
    )?;

    tx.commit()?;
    Ok(result)
}

/// Closes the snapshot. Only a full walk leaves it usable for comparison.
pub fn close(conn: &Connection, id: i64, reason: StopReason) -> Result<(), StoreError> {
    let complete = reason.yields_complete_list();
    conn.execute(
        "UPDATE snapshots
         SET complete = ?2, taken_at = ?3, stopped_by = ?4,
             next_cursor = CASE WHEN ?2 = 1 THEN NULL ELSE next_cursor END
         WHERE id = ?1",
        params![id, complete, now(), reason.as_str()],
    )?;
    Ok(())
}

/// The most recent usable snapshot.
///
/// Reads from the view, not the table, so it is impossible to return an
/// incomplete one even if someone gets the condition wrong.
///
/// `id DESC` breaks the tie, and it is not decoration. `taken_at` is in whole
/// seconds, so two walks that close inside the same second are equally recent
/// as far as the sort is concerned and SQLite may hand back either — which
/// makes "the newest capture" a question with two answers. Nothing noticed
/// while the only readers were the cache and the crossings, where either answer
/// is the same list; the monitor compares this id against the one it last
/// reported, so an arbitrary answer means a run that silently finds no changes.
/// `id` is the rowid, so it increases with every insert and orders the two the
/// way they actually happened.
pub fn latest_complete(
    conn: &Connection,
    account_pk: Pk,
    kind: ListKind,
) -> Result<Option<Snapshot>, StoreError> {
    let snapshot = conn
        .query_row(
            "SELECT id, account_pk, kind, started_at, taken_at, complete, member_count,
                    declared_count, pages, requests, next_cursor, resumes
             FROM usable_snapshots
             WHERE account_pk = ?1 AND kind = ?2
             ORDER BY taken_at DESC, id DESC LIMIT 1",
            params![pk_to_sql(account_pk), kind.as_str()],
            row_to_snapshot,
        )
        .optional()?;
    Ok(snapshot)
}

/// One usable snapshot by id.
///
/// From the view, like [`latest_complete`], so a caller holding the id of a
/// walk that stopped short gets `None` rather than a capture with accounts
/// missing from it. That is the difference between a monitor saying nothing and
/// a monitor announcing two hundred departures that never happened, and it is
/// held here rather than by whoever remembers to check `complete`.
pub fn find_usable(conn: &Connection, id: i64) -> Result<Option<Snapshot>, StoreError> {
    let snapshot = conn
        .query_row(
            "SELECT id, account_pk, kind, started_at, taken_at, complete, member_count,
                    declared_count, pages, requests, next_cursor, resumes
             FROM usable_snapshots WHERE id = ?1",
            params![id],
            row_to_snapshot,
        )
        .optional()?;
    Ok(snapshot)
}

/// The users in a snapshot, in the order Instagram served them.
pub fn members(conn: &Connection, id: i64) -> Result<Vec<User>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT u.pk, u.username, u.full_name, u.is_private, u.is_verified, u.pfp_url
         FROM snapshot_members m JOIN users u ON u.pk = m.user_pk
         WHERE m.snapshot_id = ?1
         ORDER BY m.ordinal",
    )?;
    let rows = stmt.query_map(params![id], super::users::row_to_user)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Discards half-finished walks for this account and list. Called when starting
/// a new one, so partials that no longer serve do not pile up.
pub fn delete_partials(
    conn: &Connection,
    account_pk: Pk,
    kind: ListKind,
) -> Result<usize, StoreError> {
    let deleted = conn.execute(
        "DELETE FROM snapshots WHERE account_pk = ?1 AND kind = ?2 AND complete = 0",
        params![pk_to_sql(account_pk), kind.as_str()],
    )?;
    Ok(deleted)
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<Snapshot> {
    let kind: String = row.get(2)?;
    Ok(Snapshot {
        id: row.get(0)?,
        account_pk: pk_from_sql(row.get(1)?),
        kind: if kind == "followers" {
            ListKind::Followers
        } else {
            ListKind::Following
        },
        started_at: row.get(3)?,
        taken_at: row.get(4)?,
        complete: row.get(5)?,
        member_count: row.get::<_, i64>(6)? as u64,
        declared_count: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        pages: row.get::<_, i64>(8)? as u32,
        requests: row.get::<_, i64>(9)? as u32,
        next_cursor: row.get(10)?,
        resumes: row.get::<_, i64>(11)? as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{accounts, users};

    fn user(pk: Pk) -> User {
        User {
            pk,
            username: format!("user{pk}"),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    fn base() -> Store {
        let db = Store::in_memory().unwrap();
        users::upsert(db.conn(), &user(1)).unwrap();
        accounts::upsert(db.conn(), 1, true).unwrap();
        db
    }

    #[test]
    fn a_fresh_snapshot_is_not_complete() {
        let db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        assert!(
            latest_complete(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_none()
        );
        assert_eq!(members(db.conn(), id).unwrap().len(), 0);
    }

    #[test]
    fn saving_pages_accumulates_members_in_order() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(4))
            .unwrap()
            .id;

        save_page(&mut db, id, &[user(10), user(11)], Some("c1")).unwrap();
        save_page(&mut db, id, &[user(12), user(13)], None).unwrap();

        let pks: Vec<_> = members(db.conn(), id)
            .unwrap()
            .into_iter()
            .map(|u| u.pk)
            .collect();
        assert_eq!(pks, vec![10, 11, 12, 13]);
    }

    #[test]
    fn repeats_across_pages_are_not_counted_twice() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;

        let first = save_page(&mut db, id, &[user(10), user(11)], Some("c")).unwrap();
        // Instagram repeats accounts across pages when the list shifts.
        let second = save_page(&mut db, id, &[user(11), user(12)], None).unwrap();

        assert_eq!(first.added, 2);
        assert_eq!(second.added, 1);
        assert_eq!(members(db.conn(), id).unwrap().len(), 3);
    }

    /// Instagram repeats accounts across pages when the list shifts under it.
    /// A repeat must not push the ordinal forward, or the accounts after it
    /// come back in a different order than they were served in — and that
    /// order is the chronological one the user reads.
    #[test]
    fn repeats_do_not_disturb_the_order_of_what_follows() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;

        save_page(&mut db, id, &[user(10), user(11)], Some("c1")).unwrap();
        // The first two come round again, with one genuinely new behind them.
        save_page(&mut db, id, &[user(10), user(11), user(12)], Some("c2")).unwrap();
        save_page(&mut db, id, &[user(13)], None).unwrap();

        let pks: Vec<_> = members(db.conn(), id)
            .unwrap()
            .into_iter()
            .map(|u| u.pk)
            .collect();
        assert_eq!(pks, vec![10, 11, 12, 13]);
    }

    #[test]
    fn saving_a_page_advances_the_cursor() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10)], Some("next")).unwrap();

        let pending = resumable(db.conn(), 1, ListKind::Followers)
            .unwrap()
            .unwrap();
        assert_eq!(pending.id, id);
        assert_eq!(pending.next_cursor.as_deref(), Some("next"));
        assert_eq!(pending.pages, 1);
    }

    #[test]
    fn renames_are_reported_when_saving() {
        let mut db = base();
        users::upsert(db.conn(), &user(10)).unwrap();

        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        let mut renamed = user(10);
        renamed.username = "brand_new_name".into();

        let saved = save_page(&mut db, id, &[renamed], None).unwrap();
        assert_eq!(saved.renamed, vec![(10, "user10".to_string())]);
    }

    /// The most important rule of the store: a half-finished snapshot can never
    /// be the basis of a comparison, because everything missing from it would
    /// show up as a departure.
    #[test]
    fn latest_complete_never_returns_a_partial_one() {
        let mut db = base();

        for reason in [
            StopReason::Canceled,
            StopReason::Truncated,
            StopReason::RateLimit,
            StopReason::Network,
            StopReason::SessionInvalid,
            StopReason::PageLimit,
        ] {
            let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
            save_page(&mut db, id, &[user(10)], None).unwrap();
            close(db.conn(), id, reason).unwrap();

            assert!(
                latest_complete(db.conn(), 1, ListKind::Followers)
                    .unwrap()
                    .is_none(),
                "a snapshot cut short by {reason:?} should not be usable"
            );
        }

        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10)], None).unwrap();
        close(db.conn(), id, StopReason::Completed).unwrap();
        assert!(
            latest_complete(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_some()
        );
    }

    /// "The newest capture" has to have one answer, not either of two.
    ///
    /// `taken_at` is in whole seconds, so two walks closing inside the same
    /// second are equally recent as far as the sort can tell, and without the
    /// tie-break SQLite may hand back the older one. Nothing noticed while the
    /// only readers were the cache and the crossings, where either answer is
    /// the same list. The monitor compares this id against the one it last
    /// reported, so an arbitrary answer is a run that finds no changes and says
    /// nothing about why.
    #[test]
    fn the_newest_of_two_captures_taken_in_one_second_is_the_later_one() {
        let mut db = base();

        let mut ids = Vec::new();
        for _ in 0..2 {
            let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
            save_page(&mut db, id, &[user(10)], None).unwrap();
            close(db.conn(), id, StopReason::Completed).unwrap();
            ids.push(id);
        }

        let both_at_once: i64 = db
            .conn()
            .query_row(
                "SELECT count(DISTINCT taken_at) FROM snapshots WHERE complete = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            both_at_once, 1,
            "the two have to share a second to be a test"
        );

        assert_eq!(
            latest_complete(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .unwrap()
                .id,
            ids[1]
        );
    }

    /// Guards against the enum and the schema drifting apart: every variant has
    /// to be accepted by the CHECK constraint.
    #[test]
    fn every_stop_reason_is_accepted_by_the_schema() {
        let mut db = base();
        for reason in StopReason::ALL {
            let id = begin(db.conn(), 1, ListKind::Following, None).unwrap().id;
            save_page(&mut db, id, &[user(10)], None).unwrap();
            close(db.conn(), id, reason)
                .unwrap_or_else(|e| panic!("the schema rejected {reason:?}: {e}"));
        }
    }

    #[test]
    fn closing_cleanly_clears_the_pending_cursor() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10)], Some("leftover")).unwrap();
        close(db.conn(), id, StopReason::Completed).unwrap();

        let snapshot = latest_complete(db.conn(), 1, ListKind::Followers)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.next_cursor, None);
        assert!(
            resumable(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn an_interrupted_one_keeps_its_cursor_to_resume() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10)], Some("from_here")).unwrap();
        close(db.conn(), id, StopReason::Canceled).unwrap();

        let pending = resumable(db.conn(), 1, ListKind::Followers)
            .unwrap()
            .unwrap();
        assert_eq!(pending.next_cursor.as_deref(), Some("from_here"));
    }

    #[test]
    fn something_too_old_is_not_resumed() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10)], Some("stale")).unwrap();

        let long_ago = now() - RESUME_WINDOW_SECS - 1;
        db.conn()
            .execute(
                "UPDATE snapshots SET started_at = ?1 WHERE id = ?2",
                params![long_ago, id],
            )
            .unwrap();

        assert!(
            resumable(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_two_lists_do_not_mix() {
        let mut db = base();
        let followers = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, followers, &[user(10)], None).unwrap();
        close(db.conn(), followers, StopReason::Completed).unwrap();

        assert!(
            latest_complete(db.conn(), 1, ListKind::Following)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn deleting_a_snapshot_deletes_its_members() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10), user(11)], None).unwrap();

        db.conn()
            .execute("DELETE FROM snapshots WHERE id = ?1", params![id])
            .unwrap();

        let left: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM snapshot_members", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(left, 0, "the foreign key cascade did not fire");
    }

    #[test]
    fn deleting_partials_spares_the_complete_ones() {
        let mut db = base();
        let good = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, good, &[user(10)], None).unwrap();
        close(db.conn(), good, StopReason::Completed).unwrap();

        let bad = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, bad, &[user(11)], Some("c")).unwrap();

        assert_eq!(
            delete_partials(db.conn(), 1, ListKind::Followers).unwrap(),
            1
        );
        assert!(
            latest_complete(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_some()
        );
    }
}
