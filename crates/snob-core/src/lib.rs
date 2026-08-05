//! snob-ig domain: models, diffing, storage and notifications.
//!
//! Identity rule: an account is always identified by its numeric `pk`, never by
//! its username. Usernames change, and detecting that change is itself an event
//! the tool reports.

pub mod filters;
pub mod model;
pub mod paths;
pub mod secret;
pub mod secrets;
pub mod session;
pub mod sets;
pub mod store;

/// Stable numeric identifier of an Instagram account.
pub type Pk = u64;
