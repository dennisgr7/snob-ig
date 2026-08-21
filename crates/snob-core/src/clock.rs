//! What time it is, in the two units this project counts in.
//!
//! Two functions and a rule: **seconds are the domain unit and milliseconds are
//! the rate-control unit**, and nothing converts between them by hand. A
//! capture's `taken_at`, a run's `started_at` and a rename's `at` are seconds; a
//! cooldown's `until_ms` and the budget's theoretical arrival time are
//! milliseconds, because a pace of one request every 3.83 seconds cannot be
//! expressed in whole seconds at all.
//!
//! They lived in `store::mod` when storage was the only thing that read a clock.
//! It is not: `snob-ig` reads one to say how long is left of a cooldown, and
//! reaching it through the persistence module was the same accidental edge the
//! request budget's trait had.

/// Current timestamp in seconds, the domain unit.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Current timestamp in milliseconds, the rate-control unit.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
