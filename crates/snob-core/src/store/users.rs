//! User persistence.
//!
//! `users` is a metadata cache, not a claim of membership in any list: someone
//! being here only means we have seen them at some point. Membership lives in
//! `snapshot_members`.

use rusqlite::{Connection, OptionalExtension, params};

use super::{StoreError, now, pk_from_sql, pk_to_sql};
use crate::Pk;
use crate::model::User;

/// Inserts or updates a user and records the rename if there was one.
///
/// Returns the previous username when it changed, which is an event worth
/// reporting in its own right.
pub fn upsert(conn: &Connection, u: &User) -> Result<Option<String>, StoreError> {
    let pk = pk_to_sql(u.pk);
    let now = now();

    let previous: Option<String> = conn
        .query_row(
            "SELECT username FROM users WHERE pk = ?1",
            params![pk],
            |row| row.get(0),
        )
        .optional()?;

    conn.execute(
        "INSERT INTO users (pk, username, full_name, is_verified, is_private, pfp_url,
                            first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT(pk) DO UPDATE SET
             username    = excluded.username,
             -- Metadata is only overwritten when the new response carries it: a
             -- page arriving without full_name must not wipe the one we had.
             full_name   = coalesce(excluded.full_name,   users.full_name),
             is_verified = coalesce(excluded.is_verified, users.is_verified),
             is_private  = coalesce(excluded.is_private,  users.is_private),
             pfp_url     = coalesce(excluded.pfp_url,     users.pfp_url),
             last_seen   = excluded.last_seen",
        params![
            pk,
            u.username,
            u.full_name,
            u.is_verified,
            u.is_private,
            u.pfp_url,
            now,
        ],
    )?;

    match previous {
        Some(old) if old != u.username => {
            conn.execute(
                "INSERT INTO username_history (pk, username, changed_at) VALUES (?1, ?2, ?3)",
                params![pk, old, now],
            )?;
            Ok(Some(old))
        }
        _ => Ok(None),
    }
}

pub fn find(conn: &Connection, pk: Pk) -> Result<Option<User>, StoreError> {
    let u = conn
        .query_row(
            "SELECT pk, username, full_name, is_private, is_verified, pfp_url
             FROM users WHERE pk = ?1",
            params![pk_to_sql(pk)],
            row_to_user,
        )
        .optional()?;
    Ok(u)
}

pub fn previous_usernames(conn: &Connection, pk: Pk) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn
        .prepare("SELECT username FROM username_history WHERE pk = ?1 ORDER BY changed_at DESC")?;
    let rows = stmt.query_map(params![pk_to_sql(pk)], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn row_to_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        pk: pk_from_sql(row.get(0)?),
        username: row.get(1)?,
        full_name: row.get(2)?,
        is_private: row.get(3)?,
        is_verified: row.get(4)?,
        pfp_url: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn user(pk: Pk, name: &str) -> User {
        User {
            pk,
            username: name.into(),
            full_name: Some("Full Name".into()),
            is_private: Some(false),
            is_verified: Some(false),
            pfp_url: None,
        }
    }

    #[test]
    fn inserting_twice_leaves_a_single_row() {
        let db = Store::in_memory().unwrap();
        upsert(db.conn(), &user(1, "one")).unwrap();
        upsert(db.conn(), &user(1, "one")).unwrap();

        let rows: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM users", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_rename_is_recorded() {
        let db = Store::in_memory().unwrap();
        upsert(db.conn(), &user(1, "old_name")).unwrap();

        let previous = upsert(db.conn(), &user(1, "new_name")).unwrap();
        assert_eq!(previous.as_deref(), Some("old_name"));
        assert_eq!(previous_usernames(db.conn(), 1).unwrap(), vec!["old_name"]);

        let unchanged = upsert(db.conn(), &user(1, "new_name")).unwrap();
        assert_eq!(unchanged, None);
    }

    #[test]
    fn a_partial_response_does_not_wipe_existing_metadata() {
        let db = Store::in_memory().unwrap();
        upsert(db.conn(), &user(1, "one")).unwrap();

        let sparse = User {
            pk: 1,
            username: "one".into(),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        };
        upsert(db.conn(), &sparse).unwrap();

        let stored = find(db.conn(), 1).unwrap().unwrap();
        assert_eq!(stored.full_name.as_deref(), Some("Full Name"));
        assert_eq!(stored.is_verified, Some(false));
    }

    #[test]
    fn first_seen_stays_put_and_last_seen_moves() {
        let db = Store::in_memory().unwrap();
        upsert(db.conn(), &user(1, "one")).unwrap();
        db.conn()
            .execute("UPDATE users SET first_seen = 100, last_seen = 100", [])
            .unwrap();

        upsert(db.conn(), &user(1, "one")).unwrap();

        let (first, last): (i64, i64) = db
            .conn()
            .query_row("SELECT first_seen, last_seen FROM users", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(first, 100, "first_seen should not move");
        assert!(last > 100, "last_seen should have been updated");
    }

    #[test]
    fn a_user_that_does_not_exist_returns_nothing() {
        let db = Store::in_memory().unwrap();
        assert!(find(db.conn(), 999).unwrap().is_none());
    }
}
