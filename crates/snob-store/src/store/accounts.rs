//! Tracked accounts and the counter poll the cache rests on.

use rusqlite::{Connection, OptionalExtension, params};

use super::{StoreError, now, pk_to_sql};
use snob_core::Pk;
use snob_core::model::ListKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub pk: Pk,
    pub is_self: bool,
    pub polled_at: Option<i64>,
    pub follower_count: Option<u64>,
    pub following_count: Option<u64>,
}

impl Account {
    /// The counter for the side we care about, if it has ever been polled.
    pub fn counter(&self, kind: ListKind) -> Option<u64> {
        match kind {
            ListKind::Followers => self.follower_count,
            ListKind::Following => self.following_count,
        }
    }
}

/// Registers the account if it was not there. Does not touch the counters.
pub fn upsert(conn: &Connection, pk: Pk, is_self: bool) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO accounts (pk, is_self, added_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(pk) DO UPDATE SET is_self = excluded.is_self",
        params![pk_to_sql(pk), is_self, now()],
    )?;
    Ok(())
}

/// Stores the result of a counter poll.
pub fn record_poll(
    conn: &Connection,
    pk: Pk,
    followers: Option<u64>,
    following: Option<u64>,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE accounts
         SET polled_at = ?2,
             follower_count = coalesce(?3, follower_count),
             following_count = coalesce(?4, following_count)
         WHERE pk = ?1",
        params![
            pk_to_sql(pk),
            now(),
            followers.map(|v| v as i64),
            following.map(|v| v as i64),
        ],
    )?;
    Ok(())
}

/// The pk behind a username, restricted to accounts the tool tracks.
///
/// Usernames are not unique over time — someone renames and another takes the
/// old name — so the join keeps the lookup to tracked accounts, and the most
/// recently polled one wins. Case-insensitive, so `@Ghost` finds `ghost`.
pub fn find_pk_by_username(conn: &Connection, username: &str) -> Result<Option<Pk>, StoreError> {
    // The empty name is `users::ensure`'s placeholder for "seen but never
    // named", not a username — no Instagram account has one. Without this,
    // `snob followers "" --cache` matches whichever unnamed account was polled
    // last and answers about somebody else's lists.
    if username.is_empty() {
        return Ok(None);
    }

    let pk = conn
        .query_row(
            "SELECT u.pk FROM users u JOIN accounts a ON a.pk = u.pk
             WHERE u.username = ?1 COLLATE NOCASE
             ORDER BY a.polled_at DESC, u.pk LIMIT 1",
            params![username],
            |row| row.get(0),
        )
        .optional()?;
    Ok(pk.map(super::pk_from_sql))
}

/// The account the session belongs to, as this database has recorded it.
///
/// `None` on a machine that has never run anything against a session, which is
/// a real state rather than an error: `snob watch status` opens the store
/// without one and has to answer anyway.
///
/// One row is expected — `is_self` is written by `upsert` from what the engine
/// resolved — but logging in as somebody else leaves the old row behind, so the
/// most recently polled one wins, the way `find_pk_by_username` settles the
/// same kind of tie.
pub fn own(conn: &Connection) -> Result<Option<Pk>, StoreError> {
    let pk = conn
        .query_row(
            "SELECT pk FROM accounts WHERE is_self = 1 ORDER BY polled_at DESC, pk LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(pk.map(super::pk_from_sql))
}

pub fn find(conn: &Connection, pk: Pk) -> Result<Option<Account>, StoreError> {
    let account = conn
        .query_row(
            "SELECT pk, is_self, polled_at, follower_count, following_count
             FROM accounts WHERE pk = ?1",
            params![pk_to_sql(pk)],
            |row| {
                Ok(Account {
                    pk: super::pk_from_sql(row.get(0)?),
                    is_self: row.get(1)?,
                    polled_at: row.get(2)?,
                    follower_count: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    following_count: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                })
            },
        )
        .optional()?;
    Ok(account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, users};
    use snob_core::model::User;

    fn with_account(db: &Store, pk: Pk) {
        users::upsert(
            db.conn(),
            &User {
                pk,
                username: format!("account{pk}"),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            },
        )
        .unwrap();
        upsert(db.conn(), pk, true).unwrap();
    }

    #[test]
    fn a_new_account_has_no_counters() {
        let db = Store::in_memory().unwrap();
        with_account(&db, 1);

        let a = find(db.conn(), 1).unwrap().unwrap();
        assert!(a.is_self);
        assert_eq!(a.polled_at, None);
        assert_eq!(a.counter(ListKind::Followers), None);
    }

    #[test]
    fn a_poll_stores_both_counters() {
        let db = Store::in_memory().unwrap();
        with_account(&db, 1);
        record_poll(db.conn(), 1, Some(1200), Some(340)).unwrap();

        let a = find(db.conn(), 1).unwrap().unwrap();
        assert_eq!(a.counter(ListKind::Followers), Some(1200));
        assert_eq!(a.counter(ListKind::Following), Some(340));
        assert!(a.polled_at.is_some());
    }

    #[test]
    fn a_partial_poll_does_not_wipe_the_counter_it_omits() {
        let db = Store::in_memory().unwrap();
        with_account(&db, 1);
        record_poll(db.conn(), 1, Some(1200), Some(340)).unwrap();
        record_poll(db.conn(), 1, Some(1201), None).unwrap();

        let a = find(db.conn(), 1).unwrap().unwrap();
        assert_eq!(a.counter(ListKind::Followers), Some(1201));
        assert_eq!(a.counter(ListKind::Following), Some(340));
    }

    #[test]
    fn an_account_requires_its_user_to_exist() {
        let db = Store::in_memory().unwrap();
        // Without the users row, the foreign key must refuse it.
        assert!(upsert(db.conn(), 999, false).is_err());
    }

    #[test]
    fn find_pk_by_username_ignores_case() {
        let db = Store::in_memory().unwrap();
        with_account(&db, 7);

        assert_eq!(find_pk_by_username(db.conn(), "ACCOUNT7").unwrap(), Some(7));
    }

    #[test]
    fn an_unknown_username_is_not_found() {
        let db = Store::in_memory().unwrap();
        assert_eq!(find_pk_by_username(db.conn(), "nobody").unwrap(), None);
    }

    #[test]
    fn a_user_without_an_accounts_row_is_not_found() {
        let db = Store::in_memory().unwrap();
        users::upsert(
            db.conn(),
            &User {
                pk: 5,
                username: "loose".into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            },
        )
        .unwrap();

        assert_eq!(find_pk_by_username(db.conn(), "loose").unwrap(), None);
    }

    /// Usernames are not unique across pks; the account with the most recent
    /// poll has the freshest claim to the name.
    #[test]
    fn a_duplicated_username_prefers_the_polled_account() {
        let db = Store::in_memory().unwrap();
        for pk in [1, 2] {
            users::upsert(
                db.conn(),
                &User {
                    pk,
                    username: "ghost".into(),
                    full_name: None,
                    is_private: None,
                    is_verified: None,
                    pfp_url: None,
                },
            )
            .unwrap();
            upsert(db.conn(), pk, false).unwrap();
        }
        record_poll(db.conn(), 2, Some(10), None).unwrap();

        assert_eq!(find_pk_by_username(db.conn(), "ghost").unwrap(), Some(2));
    }
}
