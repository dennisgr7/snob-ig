//! User persistence.
//!
//! `users` is a metadata cache, not a claim of membership in any list: someone
//! being here only means we have seen them at some point. Membership lives in
//! `snapshot_members`.

use rusqlite::{Connection, OptionalExtension, params};

use super::{StoreError, now, pk_from_sql, pk_to_sql};
use crate::Pk;
use crate::model::User;

/// Records that an account exists, without claiming to know its name.
///
/// `users.username` is `TEXT NOT NULL` in a `STRICT` table and
/// `accounts.pk REFERENCES users(pk)`, so a row has to exist before anything
/// else about the account can be stored — even on the paths where the name has
/// never been learned. Those paths used to write the numeric id into the name
/// column, which is a different and worse thing than admitting ignorance.
///
/// The empty string is what "not known" looks like here. No Instagram account
/// has one, so it cannot collide with a real name — but it is not
/// self-defending: `find_pk_by_username` refuses it explicitly, because a
/// lookup for `""` would otherwise match whichever unnamed account was polled
/// last. [`upsert`] reads it as never having known rather than as a name that
/// changed.
pub fn ensure(conn: &Connection, pk: Pk) -> Result<(), StoreError> {
    let now = now();
    conn.execute(
        "INSERT INTO users (pk, username, first_seen, last_seen)
         VALUES (?1, '', ?2, ?2)
         ON CONFLICT(pk) DO UPDATE SET last_seen = excluded.last_seen",
        params![pk_to_sql(pk), now],
    )?;
    Ok(())
}

/// Inserts or updates a user and records the rename if there was one.
///
/// Returns the previous username when it changed, which is an event worth
/// reporting in its own right.
///
/// An empty stored name is not a rename. It is what [`ensure`] writes when the
/// account was seen but never named, and filing `"" -> realname` in
/// `username_history` would put a change that never happened into the table the
/// monitor is meant to read.
/// `prepare_cached` throughout, and that is not a micro-optimization here: this
/// runs once per account inside `snapshots::save_page`, which is the only
/// per-account loop in the program. `Connection::execute` and `query_row`
/// compile their statement every call, so a six-thousand-follower walk was
/// paying twelve thousand `sqlite3_prepare_v2` calls for two distinct statements.
/// The cache lives on the connection, so they survive across pages and
/// transactions.
pub fn upsert(conn: &Connection, u: &User) -> Result<Option<String>, StoreError> {
    let pk = pk_to_sql(u.pk);
    let now = now();

    let previous: Option<String> = conn
        .prepare_cached("SELECT username FROM users WHERE pk = ?1")?
        .query_row(params![pk], |row| row.get(0))
        .optional()?;

    conn.prepare_cached(
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
    )?
    .execute(params![
        pk,
        u.username,
        u.full_name,
        u.is_verified,
        u.is_private,
        u.pfp_url,
        now,
    ])?;

    match previous {
        Some(old) if !old.is_empty() && old != u.username => {
            // Not cached: a rename is rare, so keeping a third statement in the
            // cache would only push out one of the two that run every account.
            conn.execute(
                "INSERT INTO username_history (pk, username, changed_at) VALUES (?1, ?2, ?3)",
                params![pk, old, now],
            )?;
            Ok(Some(old))
        }
        _ => Ok(None),
    }
}

/// The stored name, when one was ever learned.
///
/// The translation from the empty-string placeholder to `None` happens here, at
/// the boundary that writes it, so callers never have to know the encoding.
/// [`find`] hands the raw row back — it is the metadata cache — and every reader
/// that wanted a *name* was doing its own `is_empty` check against a placeholder
/// documented on [`ensure`] in another crate.
pub fn name(conn: &Connection, pk: Pk) -> Result<Option<String>, StoreError> {
    Ok(find(conn, pk)?
        .map(|u| u.username)
        .filter(|name| !name.is_empty()))
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

    /// The names filed against an account, oldest first.
    ///
    /// A test helper rather than public API. It used to be one, kept alive by
    /// these three assertions and sitting where somebody looking for "show me
    /// this account's old names" would find it first — while ordering by
    /// `changed_at`, which the production reader deliberately does not:
    /// `watch::renames_since` orders by `id`, because two names filed in the
    /// same second have no order at all by the clock and one by arrival.
    fn previous_usernames(conn: &Connection, pk: Pk) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT username FROM username_history WHERE pk = ?1 ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map(params![pk_to_sql(pk)], |row| row.get::<_, String>(0))
            .unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    }

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

    /// The sequence that used to corrupt a real name.
    ///
    /// A session stored without a username reaches `ensure`; a later run online
    /// learns the name; a run with `--cache` reaches `ensure` again. The last
    /// step used to write the numeric id over the name, so the account could no
    /// longer be found by it and a rename that never happened was filed.
    #[test]
    fn a_run_that_does_not_know_the_name_never_overwrites_one() {
        let db = Store::in_memory().unwrap();

        ensure(db.conn(), 7).unwrap();
        upsert(db.conn(), &user(7, "realname")).unwrap();
        ensure(db.conn(), 7).unwrap();

        assert_eq!(find(db.conn(), 7).unwrap().unwrap().username, "realname");
        assert!(
            previous_usernames(db.conn(), 7).is_empty(),
            "nothing was renamed, so nothing may be filed as a rename"
        );
    }

    /// Learning the name for the first time is not a rename either: the empty
    /// string is what `ensure` writes for "never knew it".
    #[test]
    fn learning_a_name_for_the_first_time_is_not_a_rename() {
        let db = Store::in_memory().unwrap();
        ensure(db.conn(), 7).unwrap();

        assert_eq!(upsert(db.conn(), &user(7, "realname")).unwrap(), None);
        assert!(previous_usernames(db.conn(), 7).is_empty());

        // A genuine rename still is one.
        assert_eq!(
            upsert(db.conn(), &user(7, "newname")).unwrap().as_deref(),
            Some("realname")
        );
    }

    /// The empty name must never be findable: it is a placeholder, not a
    /// username, and matching it would hand back the wrong account.
    #[test]
    fn an_unnamed_account_cannot_be_looked_up_by_name() {
        let db = Store::in_memory().unwrap();
        ensure(db.conn(), 7).unwrap();
        crate::store::accounts::upsert(db.conn(), 7, true).unwrap();

        let found = crate::store::accounts::find_pk_by_username(db.conn(), "").unwrap();
        assert_eq!(found, None);
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
        assert_eq!(previous_usernames(db.conn(), 1), vec!["old_name"]);

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
