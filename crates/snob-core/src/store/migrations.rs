//! Schema migrations.
//!
//! Rules for the ones that come next:
//!
//! - A published migration is never edited; a new one is added.
//! - Any migration that rebuilds tables runs with `foreign_keys` off and turns
//!   it back on afterwards, which is SQLite's documented recipe.
//! - The `validate()` test is mandatory: it runs the whole chain from scratch
//!   and fails if one of them breaks.

use std::sync::LazyLock;

use rusqlite_migration::{M, Migrations};

pub static MIGRATIONS: LazyLock<Migrations<'static>> =
    LazyLock::new(|| Migrations::new(vec![M::up(include_str!("sql/001_initial.sql"))]));

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
