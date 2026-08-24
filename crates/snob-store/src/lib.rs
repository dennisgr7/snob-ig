//! Everything snob-ig keeps on the machine it runs on.
//!
//! The other side of the line [`snob_core`] draws. That crate is what the
//! things *are*; this one is where they are put: the SQLite database and its
//! migrations, the platform directories the database and the configuration live
//! in, the credential store, and the monitor's `watch.toml`.
//!
//! It was all one crate called `snob-core`, described in its own manifest as
//! "domain". The description was right about the half of it that is; what this
//! separation buys is that `snob-ig` now depends on that half alone, so an HTTP
//! client no longer compiles SQLite, three keyring backends and a TOML parser it
//! never calls.
//!
//! **The direction is one-way and load-bearing**: this crate depends on
//! `snob_core`, never the reverse. A type that both a walk and a database need
//! belongs over there.
//!
//! Everything here is per user and never per directory — the session, the
//! database and the configuration are one machine account's, and `AppPaths` is
//! the only thing that decides where.

pub mod config;
pub mod paths;
pub mod secrets;
pub mod store;
