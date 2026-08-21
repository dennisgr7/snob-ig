//! The request budget, as an interface rather than as a database.
//!
//! **This is a port, and it used to live inside the persistence module it is
//! implemented by.** `snob-ig` needs the trait — `Pacer` cannot be built
//! without one — and reaching it through `store::rate_budget` meant the client
//! crate depended on the module that opens SQLite, so a change to the schema
//! recompiled the Instagram client and `cargo tree -i rusqlite` showed an edge
//! that had no business existing.
//!
//! What is here is everything a caller has to know: how to ask for a slot, how
//! to ask whether the account is in cooldown, and how long each kind of
//! cooldown lasts. How any of that is stored is `snob_store::store::rate_budget`.
//!
//! The cooldown lengths are here rather than there because they are policy and
//! not storage: `snob-ig` decides *which* of the three a refusal earns, from a
//! status code and a body, and it does that without knowing there is a database
//! at all.

use std::time::Duration;

const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(2 * 3600);
const ACTION_BLOCK_COOLDOWN: Duration = Duration::from_secs(12 * 3600);

/// After Instagram asks for the account to be verified.
///
/// Deliberately much shorter than the other two, because it is the only one
/// waiting does not fix: a challenge is cleared by the user opening the link,
/// and the account is usable again the moment they do. Twelve hours would
/// strand someone who cleared it in thirty seconds, and would do nothing extra
/// about the case this exists for — a scheduled run knocking again on an
/// account Instagram has just asked to verify itself. Half an hour stops the
/// second without stranding the first, and repeats still escalate.
const CHALLENGE_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// How long to wait before firing a request.
pub trait RateBudget: Send + Sync {
    /// Reserves a request and returns how long to wait before sending it. Zero
    /// means go ahead.
    ///
    /// The reservation is committed even if the request is never made:
    /// overcharging is the safe direction to be wrong in.
    fn reserve(&self) -> Result<Duration, RateBudgetError>;

    /// Reserves a **write** — a follow or an unfollow — and returns how long to
    /// wait before sending it.
    ///
    /// A write pays everything a read pays and then the write bucket on top, so
    /// this can never come back with a shorter wait than [`Self::reserve`]
    /// would have. That ordering is the whole point of it being a separate
    /// method: a caller cannot reach the cheaper one by mistake, because
    /// `IgClient::post` calls this one and `IgClient::get` calls the other, and
    /// neither takes an argument that could pick the wrong one.
    fn reserve_write(&self) -> Result<Duration, RateBudgetError>;

    /// Until when the account is in cooldown, as an epoch in milliseconds.
    fn cooldown(&self) -> Result<Option<i64>, RateBudgetError>;

    /// Puts the account in cooldown and returns until when.
    fn start_cooldown(&self, reason: &str, minimum: Duration) -> Result<i64, RateBudgetError>;
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RateBudgetError(pub String);

// **No `From` impls, and that is a consequence of the split rather than a
// preference.** This used to carry `From<StoreError>` and `From<rusqlite::Error>`
// so that a `?` inside the SQLite implementation converted on its own. Both of
// those types now live in `snob-store`, and an `impl From<A> for B` where
// neither `A` nor `B` is local to the crate writing it is what the orphan rule
// exists to refuse.
//
// What replaced them is `budget_err` in `snob_store::store::rate_budget`, one
// line per fallible call. Less convenient and more honest: the conversion is now
// visible exactly where a storage failure becomes a budget failure, which is the
// crate boundary.

/// Cooldown length for each cause.
pub fn rate_limit_cooldown() -> Duration {
    RATE_LIMIT_COOLDOWN
}

pub fn action_block_cooldown() -> Duration {
    ACTION_BLOCK_COOLDOWN
}

pub fn challenge_cooldown() -> Duration {
    CHALLENGE_COOLDOWN
}

/// Grants everything and records nothing.
/// Grants everything and counts nothing. **Tests only**: using it against
/// Instagram skips rate control entirely.
#[doc(hidden)]
pub struct UnlimitedRateBudget;

impl RateBudget for UnlimitedRateBudget {
    fn reserve(&self) -> Result<Duration, RateBudgetError> {
        Ok(Duration::ZERO)
    }
    fn reserve_write(&self) -> Result<Duration, RateBudgetError> {
        Ok(Duration::ZERO)
    }
    fn cooldown(&self) -> Result<Option<i64>, RateBudgetError> {
        Ok(None)
    }
    fn start_cooldown(&self, _reason: &str, _minimum: Duration) -> Result<i64, RateBudgetError> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three cooldowns, written out rather than derived.
    ///
    /// `AGENTS.md` promises that an action block earns twelve hours rather than
    /// the throttle's two. Both constants were referenced only by name, so both
    /// could have become one second with the suite green and the promise would
    /// have read exactly the same.
    ///
    /// It followed the constants across the crate boundary. It used to sit in
    /// `store::rate_budget`'s tests, next to the numbers when the numbers were
    /// there; a test asserting a value from another crate through its accessor
    /// asserts the accessor.
    #[test]
    fn the_cooldowns_are_the_documented_ones() {
        assert_eq!(rate_limit_cooldown(), Duration::from_secs(2 * 3600));
        assert_eq!(action_block_cooldown(), Duration::from_secs(12 * 3600));
        assert_eq!(challenge_cooldown(), Duration::from_secs(30 * 60));
        assert!(
            action_block_cooldown() > rate_limit_cooldown(),
            "an action block is not a throttle and must not be treated as the lighter one"
        );
    }
}
