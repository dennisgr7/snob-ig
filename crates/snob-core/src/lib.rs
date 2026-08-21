//! snob-ig's domain: what the things are, and what follows from them.
//!
//! Models, set arithmetic, the diff between two captures, the schedule a
//! monitor runs on, the signature a report carries, and the interface the
//! request budget is asked through. **No I/O.** Nothing here opens a database,
//! makes a request, reads a file or names a directory — [`clock`] is the single
//! exception, and it is here because both of the crates above need one.
//!
//! That line is what the split is for. `snob_store` is the other side of it:
//! SQLite, the platform's directories, the keyring, and the monitor's
//! configuration file. This crate used to be both, which meant `snob-ig` — an
//! HTTP client — compiled SQLite, three keyring backends and a TOML parser it
//! never called, and a change to the database schema recompiled it.
//!
//! Identity rule: an account is always identified by its numeric `pk`, never by
//! its username. Usernames change, and detecting that change is itself an event
//! the tool reports.

pub mod budget;
pub mod clock;
pub mod duration;
pub mod filters;
pub mod model;
pub mod secret;
pub mod session;
pub mod sets;
pub mod watch;

/// Stable numeric identifier of an Instagram account.
pub type Pk = u64;
