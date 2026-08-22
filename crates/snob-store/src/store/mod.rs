//! SQLite storage.
//!
//! A single file in the local data directory. Synchronous on purpose:
//! transactions take microseconds and it is not worth dragging in an async
//! wrapper, especially since none of them is currently kept up to date with the
//! version of `rusqlite` we use.

pub mod accounts;
pub mod deliveries;
pub mod migrations;
pub mod rate_budget;
pub mod snapshots;
pub mod users;
pub mod watch;

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use rusqlite_migration::Migrations;
use thiserror::Error;

use crate::paths::{AppPaths, PathError};
use snob_core::Pk;

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
    /// The other direction, and the one that is reachable today.
    ///
    /// v0.1.1 is tagged and installed through Scoop and Homebrew with one
    /// migration while this branch writes eight, so a downgrade -- or a
    /// leftover `~/.cargo/bin/snob` earlier on the `PATH` -- meets a database
    /// it cannot read. Without this the user gets
    /// `rusqlite_migration`'s own wording, "Attempt to migrate a database with
    /// a migration number that is too high", about a file they have no reason
    /// to think is broken; the plausible response is deleting it, which throws
    /// away every capture and every mark.
    #[error(
        "the database at {path} was written by a newer version of snob: it is at schema \
         {found} and this build knows {known}.\nUpgrade snob, or point this one at a \
         different data directory."
    )]
    SchemaFromNewerSnob {
        path: String,
        found: i64,
        known: usize,
    },
    /// A page arrived for a walk this process no longer holds.
    ///
    /// Its own variant rather than a [`StoreError::Data`] string because it is
    /// the one store error that is not a fault: another process decided this
    /// walk had been abandoned and adopted it, and the honest response is to
    /// stop rather than to write a second stream of pages into one capture.
    #[error("another process took over this walk")]
    ClaimTaken,
    #[error("{0}")]
    Data(String),
}

/// The clock, re-exported so the hundreds of `store::now()` call sites in this
/// crate did not all have to move when it did.
///
/// It lives in `snob_core::clock` now: `snob-ig` reads one too, and reaching a
/// clock through the module that opens SQLite was the same accidental edge the
/// request budget's trait had.
pub use snob_core::clock::{now, now_ms};

/// SQLite only has signed 64-bit integers, and `rusqlite` stopped converting
/// `u64` in 0.38. Instagram ids are at most thirteen digits, eight orders of
/// magnitude below the ceiling, so the round trip is exact for any value this
/// tool will ever see.
///
/// **These two functions are the only road, and nothing can be written that
/// goes around them.** [`Pk`] implements neither `ToSql` nor `FromSql`, so
/// `params![pk]` and `row.get::<_, Pk>(0)` do not compile at all: the
/// discipline is the compiler's rather than code review's. It used to rest on
/// leaving `rusqlite`'s `fallible_uint` feature off, which said the same thing
/// about a bare `u64` — that is still true and still deliberate, for every
/// other `u64` in the schema, and the manifest says so next to the dependency.
///
/// Implementing the two traits on [`Pk`] would be better still, because the
/// bit-cast would live in one place instead of two functions that have to
/// agree. It is not available: `snob-core` does **no I/O** and must not
/// compile SQLite — that is what the crate split is for — and the orphan rule
/// stops this crate writing an impl of `rusqlite`'s trait for a type it does
/// not own. Two functions in one module is the closest reachable shape.
#[inline]
pub(crate) fn pk_to_sql(pk: Pk) -> i64 {
    pk.get() as i64
}

#[inline]
pub(crate) fn pk_from_sql(value: i64) -> Pk {
    Pk::new(value as u64)
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
        reject_newer_schema(&conn, path)?;
        migrate(&mut conn, &migrations::MIGRATIONS)?;
        Ok(Self { conn })
    }

    /// Throwaway database for tests.
    #[doc(hidden)]
    pub fn in_memory() -> Result<Self, StoreError> {
        let mut conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrate(&mut conn, &migrations::MIGRATIONS)?;
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

    /// One remembered fact, by name. See [`meta_get`].
    ///
    /// Here as well as free-standing so that a caller with a `Store` does not
    /// need `rusqlite` in its own manifest to reach the `meta` table. `snob-cli`
    /// is that caller, and adding a database crate to it for two lines would put
    /// the connection type in a place that has no business naming it.
    pub fn remembered(&self, key: &str) -> Result<Option<String>, StoreError> {
        meta_get(&self.conn, key)
    }

    /// Writes one. See [`meta_set`].
    pub fn remember(&self, key: &str, value: &str) -> Result<(), StoreError> {
        meta_set(&self.conn, key, value)
    }
}

/// Runs the migration chain with foreign keys out of the way.
///
/// This is the **only** place they can be turned off for a migration.
/// `rusqlite_migration` runs the whole chain inside one transaction, and SQLite
/// documents `PRAGMA foreign_keys` as a no-op inside one — so a migration that
/// writes the pragma itself, the way SQLite's own table-rebuild recipe says to,
/// would look correct and do nothing.
///
/// What that costs is not a failed migration, it is silent data loss. The first
/// migration to rebuild `snapshots` by create-copy-drop-rename would have its
/// `DROP TABLE` fire `ON DELETE CASCADE` on `snapshot_members` and empty it.
/// The rebuilt snapshots still read `complete = 1`, so `usable_snapshots` keeps
/// serving them, `members()` returns nothing, and `snob unfollowers` reports
/// everyone you follow as an unfollower.
///
/// It takes the migrations rather than reaching for the static so that a test
/// can run its own chain through the very function production uses. Checking
/// this against a hand-written copy of the wrapping would prove nothing.
fn migrate(conn: &mut Connection, migrations: &Migrations<'_>) -> Result<(), StoreError> {
    let before = user_version(conn).unwrap_or(0);

    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let outcome = migrations.to_latest(conn);
    // Back on even if the chain failed: the connection is handed back to the
    // caller either way, and `open_at` only stops on the `?` below.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    outcome?;

    // **Once, and only when a migration actually ran.**
    //
    // `DROP INDEX` moves the index's pages to the freelist; the file only
    // shrinks on `VACUUM`, and on the index 010 removes that is 42% of it.
    // `VACUUM` cannot run inside a transaction, so it cannot live in the
    // migration, and it rewrites the whole file, so it must not run on every
    // open — hence the version comparison rather than a flag somebody has to
    // remember to clear.
    //
    // Best-effort. A database that could not be compacted is a database that
    // works and takes more disk, which is not worth failing an open over: the
    // space comes back on the next migration, or never, and either way the
    // user's command runs.
    if user_version(conn).unwrap_or(0) > before
        && let Err(e) = conn.execute_batch("VACUUM")
    {
        tracing::debug!(error = %e, "the database could not be compacted after migrating");
    }

    Ok(())
}

/// One remembered value, from the `meta` table.
///
/// The table has existed since the first migration and nothing had ever read
/// or written it. It is for facts about the database itself rather than about
/// an account -- the first of them being when retention last ran.
pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// What the migration runner counts up to. `0` on a database that has none.
fn user_version(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Settings that have to be applied on every open.
///
/// `journal_mode` is the exception: it is written into the file header and
/// persists across opens. The rest are per connection.
///
/// `foreign_keys` is deliberately **not** here: it belongs around the migration
/// run, which is the one thing that needs it off, and [`migrate`] leaves it on
/// afterwards. (The bundled SQLite is compiled with
/// `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`, so in this binary it is on before anyone
/// asks — but the schema's foreign keys are load-bearing, so it is set rather
/// than assumed.)
///
/// Also called by `SqliteRateBudget::open`, which opens its own connection to
/// the same file and would otherwise miss every protection below. It overrides
/// `synchronous` afterwards, and says there why.
fn configure(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(5_000))?;

    // `PRAGMA journal_mode` returns a row with the resulting mode, so it has to
    // be queried. With `pragma_update` it fails with "Execute returned results".
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;

    conn.pragma_update(None, "synchronous", "NORMAL")?;

    // Not only a speed setting. Without it SQLite may spill a temporary
    // b-tree into TMPDIR, which is outside every directory `purge` knows
    // about — so a query's working copy of the follower list would outlive
    // the command whose whole job is to leave nothing behind.
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -8_000)?; // 8 MiB

    // Deleted rows are overwritten rather than merely unlinked from the page.
    // This is what any pruning inside the file needs — `delete_partials` on
    // every walk today, and whatever the monitor ends up expiring: they remove
    // content without removing the file, and the default leaves it legible in
    // the freed pages. A database of a few megabytes does not notice the cost.
    // (`logout` used to be named here. It never opens the database: it takes
    // the session and the browser profile, and `purge` deletes the file whole.)
    conn.pragma_update(None, "secure_delete", "ON")?;

    // In WAL mode the log is reused rather than truncated, so it keeps the
    // pre-image of everything `secure_delete` just scrubbed from the database
    // proper. Bounding it bounds how much of that history survives.
    conn.pragma_update(None, "journal_size_limit", 4 * 1024 * 1024)?;

    // Nothing here uses a virtual table or a function inside the schema, so
    // this costs nothing — and it is SQLite's own advice for any application
    // that can manage without them, because the schema of a database file is
    // executable content and this file sits at a fixed, guessable path.
    conn.pragma_update(None, "trusted_schema", "OFF")?;
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;

    Ok(())
}

/// Refuses a database written by a pre-English build.
///
/// The schema version is 1 in both, so the migration runner would consider it
/// up to date and every query would then fail with a cryptic SQL error. The
/// missing view is the cheapest tell.
/// Refuses a database written by a **newer** build, before the migration runner
/// says so in its own words.
///
/// `rusqlite_migration` answers "Attempt to migrate a database with a migration
/// number that is too high", wrapped in "could not apply migrations", which
/// reads as an internal fault in a library the user has never heard of. It is
/// reachable with no hypothetical build at all: v0.1.1 ships one migration and
/// this branch writes eight.
fn reject_newer_schema(conn: &Connection, path: &Path) -> Result<(), StoreError> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let known = migrations::COUNT;
    if found as usize > known {
        return Err(StoreError::SchemaFromNewerSnob {
            path: path.display().to_string(),
            found,
            known,
        });
    }
    Ok(())
}

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
    use rusqlite_migration::M;

    use super::*;

    #[test]
    fn an_id_survives_the_round_trip() {
        for n in [0, 1, 4_340_136_074, 71_234_567_890, i64::MAX as u64] {
            let pk = Pk::new(n);
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

    /// Proves the foreign keys are live once the store is open. `migrate` turns
    /// them off for the chain and has to turn them back on.
    #[test]
    fn foreign_keys_are_enforced() {
        let db = Store::in_memory().unwrap();
        let result = db.conn().execute(
            "INSERT INTO snapshot_members (snapshot_id, user_pk, ordinal) VALUES (999, 999, 0)",
            [],
        );
        assert!(result.is_err(), "foreign_keys is not active");
    }

    /// A migration that rebuilds a table must not take the rows of the tables
    /// that reference it.
    ///
    /// SQLite's own recipe for changing a table is create-copy-drop-rename, and
    /// with foreign keys on, the `DROP` fires `ON DELETE CASCADE` on every
    /// child. Here that is `snapshot_members`. The migration would succeed, the
    /// rebuilt snapshots would still read `complete = 1`, `usable_snapshots`
    /// would keep serving them, and `snob unfollowers` would report everyone
    /// you follow as an unfollower.
    ///
    /// A migration cannot protect itself: the chain runs in one transaction and
    /// SQLite makes `PRAGMA foreign_keys` a no-op inside one. So this drives the
    /// real `migrate`, with a second migration shaped like the one somebody will
    /// eventually write.
    #[test]
    fn a_migration_that_rebuilds_a_table_keeps_its_children() {
        // The view has to go first and come back afterwards: SQLite checks
        // every view when a table is renamed, and `usable_snapshots` selects
        // from `snapshots`. Worth knowing before writing the real 002.
        let rebuild_snapshots = "
            DROP VIEW usable_snapshots;
            CREATE TABLE snapshots_new (
              id             INTEGER PRIMARY KEY,
              account_pk     INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
              kind           TEXT    NOT NULL,
              source         TEXT    NOT NULL DEFAULT 'live',
              started_at     INTEGER NOT NULL,
              taken_at       INTEGER,
              complete       INTEGER NOT NULL DEFAULT 0,
              member_count   INTEGER NOT NULL DEFAULT 0,
              declared_count INTEGER,
              pages          INTEGER NOT NULL DEFAULT 0,
              requests       INTEGER NOT NULL DEFAULT 0,
              next_cursor    TEXT,
              resumes        INTEGER NOT NULL DEFAULT 0,
              stopped_by     TEXT
            ) STRICT;
            INSERT INTO snapshots_new SELECT
              id, account_pk, kind, source, started_at, taken_at, complete,
              member_count, declared_count, pages, requests, next_cursor,
              resumes, stopped_by FROM snapshots;
            DROP TABLE snapshots;
            ALTER TABLE snapshots_new RENAME TO snapshots;
            CREATE VIEW usable_snapshots AS
              SELECT * FROM snapshots WHERE complete = 1 AND taken_at IS NOT NULL;";

        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();

        // The database as it is today, with one snapshot and one member in it.
        let first = Migrations::new(vec![M::up(include_str!("sql/001_initial.sql"))]);
        migrate(&mut conn, &first).unwrap();
        conn.execute_batch(
            "INSERT INTO users (pk, username, first_seen, last_seen) VALUES (1, 'someone', 0, 0);
             INSERT INTO accounts (pk, is_self, added_at) VALUES (1, 1, 0);
             INSERT INTO snapshots (id, account_pk, kind, source, started_at, taken_at, complete)
               VALUES (1, 1, 'followers', 'live', 0, 0, 1);
             INSERT INTO snapshot_members (snapshot_id, user_pk, ordinal) VALUES (1, 1, 0);",
        )
        .unwrap();

        let second = Migrations::new(vec![
            M::up(include_str!("sql/001_initial.sql")),
            M::up(rebuild_snapshots),
        ]);
        migrate(&mut conn, &second).unwrap();

        let members: i64 = conn
            .query_row("SELECT count(*) FROM snapshot_members", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            members, 1,
            "the rebuild cascaded the members away and left the snapshot claiming to be complete"
        );

        // And the keys are live again afterwards, or the next write is unguarded.
        let orphan = conn.execute(
            "INSERT INTO snapshot_members (snapshot_id, user_pk, ordinal) VALUES (999, 999, 0)",
            [],
        );
        assert!(orphan.is_err(), "migrate left foreign_keys off");
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

    /// A database from a newer snob says so, rather than letting the migration
    /// runner say it in its own words.
    ///
    /// Reachable with no hypothetical build: v0.1.1 is tagged and installed
    /// through Scoop and Homebrew with one migration while this branch writes
    /// eight, so a downgrade — or a leftover `~/.cargo/bin/snob` earlier on the
    /// `PATH` — meets a file it cannot read. What came out was "could not apply
    /// migrations: … Attempt to migrate a database with a migration number that
    /// is too high", which reads as an internal fault in a library the user has
    /// never heard of, about a file they have no reason to think is broken. The
    /// plausible response is deleting it, and that throws away every capture and
    /// every mark.
    #[test]
    fn a_database_from_a_newer_snob_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ahead.db");

        // A real database, then wound forward the way a newer build would leave
        // it. The view has to be there or the *older*-schema check fires first.
        Store::open_at(&path).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", (migrations::COUNT + 3) as i64)
                .unwrap();
        }

        let Err(error) = Store::open_at(&path) else {
            panic!("a database from a newer snob should be refused");
        };
        assert!(
            matches!(error, StoreError::SchemaFromNewerSnob { .. }),
            "{error}"
        );
        let said = error.to_string();
        assert!(said.contains("newer version of snob"), "{said}");
        assert!(
            !said.contains("Delete"),
            "deleting it throws away every capture: {said}"
        );
    }
}
