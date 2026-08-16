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
