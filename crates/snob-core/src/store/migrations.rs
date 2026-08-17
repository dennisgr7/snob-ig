//! Schema migrations.
//!
//! Rules for the ones that come next:
//!
//! - A published migration is never edited; a new one is added.
//! - **A migration never writes `PRAGMA foreign_keys` itself.** The whole chain
//!   runs inside one transaction, and SQLite documents that pragma as a no-op
//!   there, so writing it would look like the recipe while doing nothing at
//!   all. [`super::migrate`] turns it off around the run instead, which is the
//!   only place it can be turned off.
//! - **Rebuild a table by create-copy-drop-rename.** The other recipe, writing
//!   `sqlite_schema` directly, needs `PRAGMA writable_schema = ON`, and
//!   `super::configure` sets `SQLITE_DBCONFIG_DEFENSIVE`, which forbids it.
//! - The `validate()` test is mandatory: it runs the whole chain from scratch
//!   and fails if one of them breaks.
//!
//! One trap worth knowing before chasing the wrong error: `foreign_key_check`
//! **misreports** on `snapshot_members`. That table is `WITHOUT ROWID`, SQLite
//! returns NULL in the rowid column for such children, and `rusqlite_migration`
//! deserializes that column as a non-optional `i64` — so a genuinely dangling
//! row surfaces as a rusqlite `InvalidColumnType`, not as a foreign-key error.
//! The transaction still rolls back, so it is safe; it just does not say what
//! happened.

use std::sync::LazyLock;

use rusqlite_migration::{M, Migrations};

pub static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        M::up(include_str!("sql/001_initial.sql")),
        M::up(include_str!("sql/002_watch.sql")),
        M::up(include_str!("sql/003_deliveries.sql")),
        M::up(include_str!("sql/004_claims.sql")),
    ])
});

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs every migration from scratch against an empty database. It is the
    /// safety net that makes adding migrations cheap.
    #[test]
    fn the_migration_chain_is_valid() {
        MIGRATIONS.validate().unwrap();
    }
}
