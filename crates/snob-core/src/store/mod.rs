//! SQLite storage.
//!
//! A single file in the local data directory. Synchronous on purpose:
//! transactions take microseconds and it is not worth dragging in an async
//! wrapper, especially since none of them is currently kept up to date with the
//! version of `rusqlite` we use.

pub mod accounts;
pub mod migrations;
pub mod rate_budget;
pub mod snapshots;
pub mod users;

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;
use thiserror::Error;

use crate::Pk;
use crate::paths::{AppPaths, PathError};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not apply migrations: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error(transparent)]
    Paths(#[from] PathError),
    #[error(
        "the database at {path} was created by an older version of snob and its schema is no \
         longer compatible.\nDelete that file and run the command again."
    )]
    OutdatedSchema { path: String },
    #[error("{0}")]
    Data(String),
}

/// Current timestamp in seconds, the domain unit.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Current timestamp in milliseconds, the rate-control unit.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// SQLite only has signed 64-bit integers, and `rusqlite` stopped converting
/// `u64` in 0.38. Instagram ids are at most thirteen digits, eight orders of
/// magnitude below the ceiling, so the round trip is exact for any value this
/// tool will ever see.
///
/// This is preferred over the `fallible_uint` feature because without it
/// `params![pk]` simply does not compile: the discipline is enforced by the
/// compiler rather than by code review.
#[inline]
pub(crate) fn pk_to_sql(pk: Pk) -> i64 {
    pk as i64
}

#[inline]
pub(crate) fn pk_from_sql(value: i64) -> Pk {
    value as Pk
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(paths: &AppPaths) -> Result<Self, StoreError> {
        paths.ensure_dirs()?;
        Self::open_at(&paths.db_file())
    }

    pub fn open_at(path: &Path) -> Result<Self, StoreError> {
        let mut conn = Connection::open(path)?;
        configure(&conn)?;
        reject_outdated_schema(&conn, path)?;
        migrations::MIGRATIONS.to_latest(&mut conn)?;
        Ok(Self { conn })
    }

    /// Throwaway database for tests.
    #[doc(hidden)]
    pub fn in_memory() -> Result<Self, StoreError> {
        let mut conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrations::MIGRATIONS.to_latest(&mut conn)?;
        Ok(Self { conn })
    }

    /// Read access for the child modules and the CLI. Writes that span several
    /// tables go through functions that manage their own transaction.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// Settings that have to be applied on every open.
///
/// `journal_mode` is the exception: it is written into the file header and
/// persists across opens. The rest are per connection, and `foreign_keys` in
/// particular defaults to off, so without this the schema's foreign keys would
/// be decorative.
fn configure(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(5_000))?;

    // `PRAGMA journal_mode` returns a row with the resulting mode, so it has to
    // be queried. With `pragma_update` it fails with "Execute returned results".
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;

    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -8_000)?; // 8 MiB
    Ok(())
}

/// Refuses a database written by a pre-English build.
///
/// The schema version is 1 in both, so the migration runner would consider it
/// up to date and every query would then fail with a cryptic SQL error. The
/// missing view is the cheapest tell.
fn reject_outdated_schema(conn: &Connection, path: &Path) -> Result<(), StoreError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0 {
        return Ok(()); // brand new file, nothing applied yet
    }

    let has_view: bool = conn.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'view' AND name = 'usable_snapshots'",
        [],
        |row| row.get(0),
    )?;

    if has_view {
        Ok(())
    } else {
        Err(StoreError::OutdatedSchema {
            path: path.display().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_survives_the_round_trip() {
        for pk in [0, 1, 4_340_136_074, 71_234_567_890, i64::MAX as Pk] {
            assert_eq!(pk_from_sql(pk_to_sql(pk)), pk);
        }
    }

    #[test]
    fn migrations_are_applied_on_open() {
        let db = Store::in_memory().unwrap();
        let tables: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'users'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
    }

    /// Proves STRICT really applied: without it SQLite would accept the text in
    /// an INTEGER column thanks to its loose typing.
    #[test]
    fn tables_reject_the_wrong_types() {
        let db = Store::in_memory().unwrap();
        let result = db.conn().execute(
            "INSERT INTO users (pk, username, first_seen, last_seen)
             VALUES ('not a number', 'x', 0, 0)",
            [],
        );
        assert!(result.is_err(), "STRICT is not active");
    }

    /// Proves foreign_keys=ON applied: it defaults to off, which would leave
    /// the foreign keys decorative.
    #[test]
    fn foreign_keys_are_enforced() {
        let db = Store::in_memory().unwrap();
        let result = db.conn().execute(
            "INSERT INTO snapshot_members (snapshot_id, user_pk, ordinal) VALUES (999, 999, 0)",
            [],
        );
        assert!(result.is_err(), "foreign_keys is not active");
    }

    #[test]
    fn the_file_uses_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Store::open_at(&tmp.path().join("test.db")).unwrap();
        let mode: String = db
            .conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    /// A database from a pre-English build has to be refused with an
    /// explanation, not with a cryptic SQL error further down the line.
    #[test]
    fn an_outdated_schema_is_refused_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("old.db");

        // Fake a database that claims to be migrated but lacks the view.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE snapshots (id INTEGER PRIMARY KEY);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }

        let Err(error) = Store::open_at(&path) else {
            panic!("an outdated schema should be refused");
        };
        assert!(matches!(error, StoreError::OutdatedSchema { .. }));
        assert!(error.to_string().contains("Delete that file"));
    }
}
