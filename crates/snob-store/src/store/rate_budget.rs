//! Rate control that persists across runs.
//!
//! An in-memory limiter is no good here: the budget has to outlive the process,
//! because running the command twice in a row must not spend twice without
//! anyone noticing. No Rust crate does this over local storage, so it is
//! hand-written.
//!
//! The algorithm is GCRA, an exact token bucket expressed as a single integer:
//! instead of storing how many tokens are left and when they were refilled, it
//! stores the theoretical instant from which the next request is legitimate.
//! Integer arithmetic, no floating-point drift, and no row to update per tick.

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use snob_core::EpochMs;
use snob_core::budget::{RateBudget, RateBudgetError};
use snob_core::clock::now_ms;

use super::StoreError;

/// A storage failure said in the budget's terms.
///
/// One line per fallible call, where a `?` used to convert on its own. The two
/// `From` impls that did it are not writable any more: `RateBudgetError` lives in
/// `snob_core::budget` and `rusqlite::Error` in rusqlite, so an impl here would
/// be between two foreign types. What is lost is brevity; what is gained is that
/// the crate boundary is visible at every place a storage failure crosses it.
fn budget_err<E: std::fmt::Display>(e: E) -> RateBudgetError {
    RateBudgetError(e.to_string())
}
use crate::paths::AppPaths;

/// Bucket names, as stored in `rate_budget.bucket`.
///
/// Constants rather than inline literals: they used to be repeated as loose
/// strings in five places, and missing one of them silently restarts the budget.
const PACE_BUCKET: &str = "pace";
const DAILY_BUCKET: &str = "daily";
const WRITE_BUCKET: &str = "writes";
/// The only value of `cooldowns.scope`. A cooldown covers the whole session.
const SESSION_SCOPE: &str = "session";

/// Sustained pace: one request every 3.83 seconds, which is what the project
/// ours is modeled on actually averages.
///
/// **It said 2.4 seconds, and that was the average of two of the three waits.**
/// The reference project pauses 1,250 ms before a request and 1,150 ms after a
/// page, and then a long 10,000 ms pause every seventh page — so its mean per
/// page is 1250 + 1150 + 10000/7 = 3,829 ms. Leaving the long pause out of the
/// average is how 2,400 was arrived at. `pace.rs` has all five of those numbers
/// right; only this summary of them was wrong, and it is the one the budget
/// enforces.
///
/// What it cost: the ceiling was 295 requests per eleven minutes where a single
/// walker uses 172, so the 123-request gap was headroom for nobody — except in
/// the case the project explicitly supports, two processes sharing this budget,
/// where the budget is the only thing holding the combined rate down and it was
/// holding it at 26.8 a minute instead of the designed 15.7. Checked against
/// `pace.rs`'s constants in August 2026.
const PACE_EMISSION_MS: i64 = 3_830;
/// Burst tolerance of the pace bucket: twenty requests.
///
/// Kept at twenty rather than raised with the emission, deliberately. With the
/// emission now equal to a single walker's own mean, the GCRA is a zero-drift
/// random walk around it, and a wider tolerance would not change what a walker
/// experiences — but a narrower ratio would: at seven emissions the per-block
/// jitter of the reference cadence exceeds the tolerance often enough that over
/// a long walk the budget would start pacing the walker, which is a design
/// change nobody asked for. Twenty preserves the "twenty requests of slack"
/// this has always meant.
const PACE_BURST_MS: i64 = PACE_EMISSION_MS * 20;

/// Daily ceiling of roughly two thousand requests.
const DAILY_EMISSION_MS: i64 = 43_200;
const DAILY_BURST_MS: i64 = 86_400_000;

/// Sustained pace of **writes**: one follow or unfollow every fifteen minutes,
/// which is ninety-six a day if somebody keeps it up around the clock.
///
/// This is a third bucket rather than a smaller emission on the existing two,
/// because a write is a request *and* something else. It pays the pace bucket
/// and the daily bucket like every other request — it costs Instagram the same
/// — and then it pays this one on top, which is the constraint that has nothing
/// to do with volume.
///
/// Where the number comes from, since `pace.rs`'s numbers come from a reference
/// implementation and this one has none. The public field reports for 2026 put
/// an established account at 100 to 150 follow actions a day and a new account
/// at 10 to 30, and they agree on something more useful than either figure:
/// **what a service reacts to is the burst, not the daily total.** A hundred
/// unfollows inside half an hour is refused on an account whose day's count
/// would have passed without comment. So the design target is not a daily
/// ceiling at all — it is a floor under the gap between two writes, and ninety-
/// six a day is what falls out of it rather than what was aimed at.
///
/// Fifteen minutes is also below the rate a person clicking the button would
/// produce, which is the point: the ceiling that matters is not the one snob
/// enforces on itself but the one the account has already used up elsewhere.
/// This bucket knows nothing about the follows made in the app on the same
/// account today, so it has to leave room for them.
const WRITE_EMISSION_MS: i64 = 900_000;

/// Burst tolerance of the write bucket: **three** writes back to back, and then
/// the fifteen minutes apply.
///
/// Two emissions, not three. A GCRA tolerance of *n* intervals lets *n + 1*
/// through before it throttles — the *n* that fit ahead plus the one emitted on
/// pace, which is the arithmetic `FIT_IN_A_ROW` in the tests below spells out
/// for the pace bucket. Written as three it would have allowed four, and the
/// sentence above would have been wrong about the constant underneath it.
///
/// Deliberately tight, and the reason is the shape of the thing rather than the
/// size of it. Twenty was right for reads, where a burst is a walk going
/// through pages of one list, which is the cost of one question. Three writes in
/// a row is about as many decisions as a person makes at a sitting; past that it
/// is no longer somebody tidying their following list, which is the only use
/// this budget is sized for.
const WRITE_BURST_MS: i64 = WRITE_EMISSION_MS * 2;

/// Slack before deciding the system clock has gone backwards.
/// Slack before deciding the system clock has gone backwards.
const CLOCK_SKEW_TOLERANCE_MS: i64 = 5_000;

const MAX_COOLDOWN_MS: i64 = 24 * 3600 * 1000;

/// Escape hatch environment variable. Deliberately absent from the help: it
/// exists so a bug of ours cannot lock anyone out, not for skipping the limit
/// out of convenience.
const IGNORE_COOLDOWN_ENV: &str = "SNOB_IGNORE_COOLDOWN";

/// Whether the escape hatch is open.
fn ignoring_cooldowns() -> bool {
    std::env::var(IGNORE_COOLDOWN_ENV).is_ok_and(|value| is_affirmative(&value))
}

/// Whether a variable's value means yes.
///
/// The switch takes an answer rather than merely existing. This is the one
/// thing that turns off the protection the whole project is built around, and
/// `SNOB_IGNORE_COOLDOWN=0` meaning "yes, ignore it" is the kind of surprise
/// that only shows up later as an account in trouble.
///
/// Split from the read above so a test can drive it without setting a variable
/// the rest of the suite is reading at the same time.
fn is_affirmative(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// The core of the algorithm, isolated so it can be tested without a database.
///
/// `emission` is what a request costs in time and `burst` how far ahead one may
/// run. Returns the new theoretical instant and the wait.
///
/// Two of the four are moments and two are lengths of time, which is exactly
/// the pair a bare `i64` could not tell apart: `decide(now, tat, burst,
/// emission)` used to compile.
fn decide(tat: EpochMs, now: EpochMs, emission: i64, burst: i64) -> (EpochMs, i64) {
    let tat = tat.max(now);
    let wait = ((tat - Duration::from_millis(burst as u64)) - now).max(0);
    (tat + Duration::from_millis(emission as u64), wait)
}

pub struct SqliteRateBudget {
    /// Behind a `Mutex` because the trait exposes `&self` and opening a
    /// transaction needs `&mut Connection`. There is never real contention:
    /// reservations within a process are sequential, and coordination between
    /// processes is SQLite's job.
    conn: std::sync::Mutex<Connection>,
}

impl SqliteRateBudget {
    /// Opens its **own** connection to the same file, with the store's settings.
    ///
    /// Not sharing the store's connection is deliberate: this way the
    /// two-process case is the same as the two-connection case, so what runs is
    /// what gets tested, and the budget's borrow cannot clash with the
    /// transaction that inserts pages.
    ///
    /// But separate must not mean differently configured. `trusted_schema` and
    /// the defensive flag are about the file rather than about a handle, so one
    /// undefended connection leaves the file undefended and cancels what the
    /// store set. The same goes for `secure_delete` and the WAL size bound —
    /// and this is the connection that enforces the bound, because it commits
    /// an `IMMEDIATE` transaction before every single request and is therefore
    /// the one that checkpoints.
    pub fn open(paths: &AppPaths) -> Result<Self, StoreError> {
        // The store must have been opened first: it is what creates the schema.
        let conn = Connection::open(paths.db_file())?;
        super::configure(&conn)?;

        // The one setting this connection does not want from `configure`.
        // Under WAL, `synchronous = NORMAL` skips the fsync at commit, so a
        // power cut can lose the last transactions. Here those are the cooldown
        // writes, and losing one brings the account out of a block early — the
        // single direction `start_cooldown` must never be wrong in. One fsync
        // against a pace of one request every 2.4 seconds costs nothing.
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(Self::over(conn))
    }

    #[doc(hidden)]
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: std::sync::Mutex::new(conn),
        }
    }

    /// Tolerates poisoning: if a thread panicked holding the lock, the worst
    /// case here is a half-done reservation the transaction already rolled back.
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reserve_bucket(
        tx: &rusqlite::Transaction<'_>,
        bucket: &str,
        emission: i64,
        burst: i64,
        now: EpochMs,
    ) -> Result<i64, RateBudgetError> {
        // The two columns are `INTEGER` and become moments here, at the row
        // boundary, the way an account id does through `pk_from_sql`.
        let row: Option<(EpochMs, EpochMs)> = tx
            .query_row(
                "SELECT tat_ms, updated_at_ms FROM rate_budget WHERE bucket = ?1",
                params![bucket],
                |row| Ok((EpochMs::new(row.get(0)?), EpochMs::new(row.get(1)?))),
            )
            .optional()
            .map_err(budget_err)?;

        let stored_tat = match row {
            // If the clock went backwards the stored instant means nothing any
            // more: the wait would come out as hours. It is reset. That is not
            // a free pass, because it still grants no burst beyond tolerance.
            Some((_, updated))
                if now + Duration::from_millis(CLOCK_SKEW_TOLERANCE_MS as u64) < updated =>
            {
                tracing::warn!(bucket, "the system clock went backwards; budget reset");
                now
            }
            Some((tat, _)) => tat,
            None => now,
        };

        let (new_tat, wait) = decide(stored_tat, now, emission, burst);

        tx.execute(
            "INSERT INTO rate_budget (bucket, tat_ms, emission_ms, burst_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(bucket) DO UPDATE SET
                 tat_ms = excluded.tat_ms,
                 emission_ms = excluded.emission_ms,
                 burst_ms = excluded.burst_ms,
                 updated_at_ms = excluded.updated_at_ms",
            params![bucket, new_tat.get(), emission, burst, now.get()],
        )
        .map_err(budget_err)?;

        Ok(wait)
    }
}

impl SqliteRateBudget {
    /// The body of both reservations, so that the two cannot come apart.
    ///
    /// Every reservation charges the pace bucket and the daily one; a write
    /// charges the write bucket as well. The answer is the longest of the waits
    /// they hand back, and **all of the buckets are charged whichever wait
    /// wins** — a request held back by one budget still spends the others,
    /// because it is still going to be sent.
    ///
    /// One transaction for all of them, and it is `IMMEDIATE` for the reason
    /// spelled out below. Charging the write bucket in a second transaction
    /// would let two processes interleave between the two, which is exactly the
    /// case a shared budget exists for.
    fn reserve_buckets(&self, write: bool) -> Result<Duration, RateBudgetError> {
        let now = now_ms();

        // BEGIN IMMEDIATE rather than the default deferred one: with deferred,
        // two processes read, both try to write, and the second gets
        // SQLITE_BUSY when upgrading the transaction, at which point
        // busy_timeout can no longer help and it fails outright. Taking the
        // write lock up front makes the processes serialize.
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(budget_err)?;

        let pace_wait =
            Self::reserve_bucket(&tx, PACE_BUCKET, PACE_EMISSION_MS, PACE_BURST_MS, now)?;
        let daily_wait =
            Self::reserve_bucket(&tx, DAILY_BUCKET, DAILY_EMISSION_MS, DAILY_BURST_MS, now)?;
        let write_wait = if write {
            Self::reserve_bucket(&tx, WRITE_BUCKET, WRITE_EMISSION_MS, WRITE_BURST_MS, now)?
        } else {
            0
        };

        tx.commit().map_err(budget_err)?;

        Ok(Duration::from_millis(
            pace_wait.max(daily_wait).max(write_wait).max(0) as u64,
        ))
    }
}

impl RateBudget for SqliteRateBudget {
    fn reserve(&self) -> Result<Duration, RateBudgetError> {
        self.reserve_buckets(false)
    }

    fn reserve_write(&self) -> Result<Duration, RateBudgetError> {
        self.reserve_buckets(true)
    }

    fn cooldown(&self) -> Result<Option<EpochMs>, RateBudgetError> {
        if ignoring_cooldowns() {
            tracing::warn!("{IGNORE_COOLDOWN_ENV} is set: the cooldown is being ignored");
            return Ok(None);
        }

        let row: Option<(EpochMs, EpochMs)> = self
            .conn()
            .query_row(
                "SELECT until_ms, set_at_ms FROM cooldowns WHERE scope = ?1",
                params![SESSION_SCOPE],
                |row| Ok((EpochMs::new(row.get(0)?), EpochMs::new(row.get(1)?))),
            )
            .optional()
            .map_err(budget_err)?;

        let now = now_ms();
        Ok(match row {
            // Also checked against `set_at_ms`: if the clock went backwards the
            // cooldown still stands even though `until_ms` looks past.
            Some((until, set_at)) if now < until || now < set_at => Some(until),
            _ => None,
        })
    }

    /// Reads the previous cooldown and writes the next one in **one**
    /// transaction, and never lets the result end sooner than what was already
    /// there.
    ///
    /// Both halves matter for the same reason `reserve` takes the write lock up
    /// front: the CLI and the v2 service share this file. Two processes reading
    /// `strikes = 1` at the same instant both wrote `strikes = 2`, so one
    /// escalation was lost. And the write was unconditional, so a two-hour
    /// throttle recorded ten minutes into a twelve-hour action block replaced
    /// it — the account came out of the more serious block early, which is the
    /// one direction this table must never be wrong in.
    fn start_cooldown(&self, reason: &str, minimum: Duration) -> Result<EpochMs, RateBudgetError> {
        let now = now_ms();
        let base = minimum.as_millis() as i64;

        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(budget_err)?;

        let previous: Option<(EpochMs, i64, EpochMs)> = tx
            .query_row(
                "SELECT set_at_ms, strikes, until_ms FROM cooldowns WHERE scope = ?1",
                params![SESSION_SCOPE],
                |row| {
                    Ok((
                        EpochMs::new(row.get(0)?),
                        row.get(1)?,
                        EpochMs::new(row.get(2)?),
                    ))
                },
            )
            .optional()
            .map_err(budget_err)?;

        // Reoffending within the next day doubles the penalty.
        let (length, strikes) = match previous {
            Some((set_at, strikes, _)) if now - set_at < MAX_COOLDOWN_MS => {
                let next = strikes + 1;
                let escalated = base.saturating_mul(1 << (next - 1).min(5));
                (escalated.min(MAX_COOLDOWN_MS), next)
            }
            _ => (base.min(MAX_COOLDOWN_MS), 1),
        };

        let standing = previous.map_or(EpochMs::new(0), |(_, _, until)| until);
        let until = (now + Duration::from_millis(length as u64)).max(standing);

        tx.execute(
            "INSERT INTO cooldowns (scope, until_ms, set_at_ms, reason, strikes)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope) DO UPDATE SET
                 until_ms = excluded.until_ms,
                 set_at_ms = excluded.set_at_ms,
                 reason = excluded.reason,
                 strikes = excluded.strikes",
            params![SESSION_SCOPE, until.get(), now.get(), reason, strikes],
        )
        .map_err(budget_err)?;
        tx.commit().map_err(budget_err)?;

        tracing::warn!(reason, minutes = length / 60_000, "account in cooldown");
        Ok(until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The numbers that decide how fast this program talks to Instagram.**
    ///
    /// Written out rather than derived, because a test written in terms of the
    /// constant it is checking passes whatever that constant becomes — which is
    /// how these came to be unguarded in the first place. Five sibling families
    /// in this tree already have a test of this exact shape and these did not,
    /// though `AGENTS.md` calls them the reason for most of the rules in it.
    ///
    /// It was not a theoretical gap. Setting `PACE_EMISSION_MS` to 100 — ten
    /// requests a second, thirty-eight times the documented rate — left all 274
    /// tests in this crate green, and so did cutting `WRITE_EMISSION_MS` from
    /// fifteen minutes to seventy seconds. Every database test here is relative:
    /// it counts reservations against a burst expressed in emissions, and
    /// because `PACE_BURST_MS` is `PACE_EMISSION_MS * 20` the whole suite is
    /// scale-invariant. Nothing else in the workspace looks at these values.
    ///
    /// The arithmetic behind the first one is in the module header and is worth
    /// restating here, because it is the pair that drifts: 1250 + 1150 +
    /// 10000/7 = 3829, from `pace::Pace::default`. If that pace changes, this
    /// fails and says so — which is the point, since the two are one decision
    /// written in two files.
    #[test]
    fn the_budget_numbers_are_the_documented_ones() {
        assert_eq!(PACE_EMISSION_MS, 3_830, "one request every 3.83 s");
        assert_eq!(PACE_BURST_MS, 76_600, "twenty requests of tolerance");
        assert_eq!(DAILY_EMISSION_MS, 43_200, "2000 requests a day");
        assert_eq!(DAILY_BURST_MS, 86_400_000, "a day of them");
        assert_eq!(
            WRITE_EMISSION_MS, 900_000,
            "one write every fifteen minutes"
        );
        assert_eq!(
            WRITE_BURST_MS, 1_800_000,
            "two emissions of tolerance, which lets three writes through and not four"
        );
    }

    const T: i64 = 2_400;
    const TAU: i64 = 48_000; // twenty requests

    /// A tolerance of twenty intervals lets twenty-one requests through before
    /// throttling: the twenty that fit ahead plus the one emitted on pace.
    const FIT_IN_A_ROW: usize = (TAU / T) as usize + 1;

    #[test]
    fn the_first_request_from_cold_does_not_wait() {
        let (tat, wait) = decide(EpochMs::new(0), EpochMs::new(1_000_000), T, TAU);
        assert_eq!(wait, 0);
        assert_eq!(tat, EpochMs::new(1_000_000 + T));
    }

    #[test]
    fn nothing_waits_within_the_burst() {
        let now = EpochMs::new(1_000_000);
        let mut tat = now;
        for i in 0..FIT_IN_A_ROW {
            let (next, wait) = decide(tat, now, T, TAU);
            assert_eq!(wait, 0, "request {i} should not wait");
            tat = next;
        }
    }

    #[test]
    fn once_the_burst_is_spent_waiting_begins() {
        let now = EpochMs::new(1_000_000);
        let mut tat = now;
        for _ in 0..FIT_IN_A_ROW {
            tat = decide(tat, now, T, TAU).0;
        }
        let (_, wait) = decide(tat, now, T, TAU);
        assert_eq!(wait, T, "past the burst you pay the full interval");
    }

    #[test]
    fn the_budget_refills_over_time() {
        let now = EpochMs::new(1_000_000);
        let mut tat = now;
        for _ in 0..25 {
            tat = decide(tat, now, T, TAU).0;
        }
        // An hour later it is fully refilled.
        let (_, wait) = decide(tat, now + Duration::from_millis(3_600_000), T, TAU);
        assert_eq!(wait, 0);
    }

    #[test]
    fn an_instant_in_the_past_does_not_grant_unlimited_budget() {
        // The old tat is clamped to "now": idleness does not accrue credit.
        let (tat, wait) = decide(EpochMs::new(1), EpochMs::new(1_000_000), T, TAU);
        assert_eq!(wait, 0);
        assert_eq!(tat, EpochMs::new(1_000_000 + T));
    }

    /// Built through the real constructor, not through `over`. The claim above
    /// `open` is that what runs is what gets tested, and a helper that skipped
    /// the configuration would have made that false for every test in here.
    fn temp_budget() -> (tempfile::TempDir, SqliteRateBudget) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::AppPaths::rooted_at(tmp.path());
        // The store creates the schema; the budget hooks in afterwards.
        let _db = super::super::Store::open(&paths).unwrap();
        (tmp, SqliteRateBudget::open(&paths).unwrap())
    }

    #[test]
    fn the_first_reservations_do_not_throttle() {
        let (_tmp, b) = temp_budget();
        for _ in 0..10 {
            assert_eq!(b.reserve().unwrap(), Duration::ZERO);
        }
    }

    /// Far enough past the burst that the disk cannot decide the answer.
    ///
    /// The bucket refills in real time, so every millisecond these reservations
    /// take is a millisecond of throttling they undo. Twenty-one of them left a
    /// margin of one emission — 2.4 seconds for twenty-one committed
    /// transactions — and this connection runs with `synchronous = FULL`, so
    /// each one waits for the platform to flush. A Windows runner spent that
    /// margin and the test failed on a correct budget.
    ///
    /// Thirty is the same assertion with twenty-four seconds of slack: past the
    /// burst is past the burst, and a machine slow enough to break this one is
    /// slow enough that it was never going to outrun the pace anyway.
    #[test]
    fn the_budget_runs_out_and_starts_throttling() {
        let (_tmp, b) = temp_budget();
        for _ in 0..30 {
            b.reserve().unwrap();
        }
        assert!(
            b.reserve().unwrap() > Duration::ZERO,
            "past the burst it should throttle"
        );
    }

    /// Three writes fit, and the fourth waits. The tolerance is what decides
    /// that, and it is deliberately much tighter than the one reads get.
    #[test]
    fn three_writes_fit_in_a_row_and_the_fourth_waits() {
        let (_tmp, b) = temp_budget();
        for i in 0..3 {
            assert_eq!(
                b.reserve_write().unwrap(),
                Duration::ZERO,
                "write {i} should not have waited"
            );
        }
        assert!(
            b.reserve_write().unwrap() > Duration::from_secs(60),
            "the fourth write should be held back by minutes, not milliseconds"
        );
    }

    /// **The buckets do not leak into each other.** A write spends the read
    /// budgets too — it is a request — but an exhausted write bucket must not
    /// stop a walk, which is the failure this would have if the write cost were
    /// expressed as a smaller emission on the pace bucket instead of as a
    /// bucket of its own.
    #[test]
    fn a_spent_write_budget_does_not_hold_up_a_read() {
        let (_tmp, b) = temp_budget();
        for _ in 0..4 {
            b.reserve_write().unwrap();
        }
        assert!(
            b.reserve_write().unwrap() > Duration::ZERO,
            "the write bucket should be spent by now"
        );
        assert_eq!(
            b.reserve().unwrap(),
            Duration::ZERO,
            "a read must not pay for the writes"
        );
    }

    /// A write is a request, so the pace bucket sees it. Without this the write
    /// path would be a way of reaching Instagram that the request count does not
    /// know about, and `Pacer::spent` would stop meaning what it says.
    #[test]
    fn a_write_spends_the_read_budget_as_well() {
        let (_tmp, b) = temp_budget();
        // Three writes are all the write bucket allows in a row, so the rest of
        // the pace bucket has to be spent by reads for the assertion to be
        // about the writes having spent theirs.
        for _ in 0..3 {
            b.reserve_write().unwrap();
        }
        for _ in 0..27 {
            b.reserve().unwrap();
        }
        assert!(
            b.reserve().unwrap() > Duration::ZERO,
            "thirty requests, three of which were writes, should have spent the pace burst"
        );
    }

    /// The connection that writes most often gets everything the store sets on
    /// itself. A second connection to one file, configured differently, is the
    /// same as not configuring the file.
    #[test]
    fn the_budget_connection_is_protected_like_the_store() {
        let (_tmp, budget) = temp_budget();
        let conn = budget.conn();
        let pragma = |name: &str| -> i64 {
            conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
                .unwrap()
        };

        assert_eq!(
            pragma("trusted_schema"),
            0,
            "the schema is executable content"
        );
        assert_eq!(pragma("secure_delete"), 1);
        assert_eq!(pragma("journal_size_limit"), 4 * 1024 * 1024);

        // FULL, not the store's NORMAL: losing the last commit here would bring
        // an account out of a cooldown early.
        assert_eq!(pragma("synchronous"), 2, "cooldown writes must be fsynced");
    }

    #[test]
    fn with_no_cooldown_there_is_no_cooldown() {
        let (_tmp, b) = temp_budget();
        assert_eq!(b.cooldown().unwrap(), None);
    }

    #[test]
    fn a_cooldown_blocks_and_reoffending_lengthens_it() {
        let (_tmp, b) = temp_budget();

        let first = b.start_cooldown("429", Duration::from_secs(3600)).unwrap();
        let active = b.cooldown().unwrap().unwrap();
        assert_eq!(active, first);

        let second = b.start_cooldown("429", Duration::from_secs(3600)).unwrap();
        assert!(
            second - now_ms() > first - now_ms(),
            "reoffending should lengthen the cooldown"
        );
    }

    /// The one direction this table must never be wrong in. A twelve-hour
    /// action block, followed ten minutes later by a two-hour throttle, used to
    /// end at the two-hour mark: the account came out of the more serious block
    /// early.
    #[test]
    fn a_shorter_cause_never_cuts_a_standing_cooldown_short() {
        let (_tmp, b) = temp_budget();

        let long = b
            .start_cooldown("feedback_required", Duration::from_secs(12 * 3600))
            .unwrap();
        let after_short = b
            .start_cooldown("rate_limit", Duration::from_secs(2 * 3600))
            .unwrap();

        assert!(
            after_short >= long,
            "the cooldown was cut from {long} to {after_short}"
        );
        assert_eq!(b.cooldown().unwrap().unwrap(), after_short);
    }

    /// The switch that turns off the protection the whole project is built
    /// around takes a yes, not merely a value. Setting it to `0` and getting
    /// "cooldowns ignored" is a surprise that only surfaces later, as an
    /// account in trouble.
    #[test]
    fn the_escape_hatch_needs_an_affirmative_value() {
        for yes in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(is_affirmative(yes), "{yes:?}");
        }
        for no in ["0", "false", "no", "off", "", "  ", "maybe"] {
            assert!(!is_affirmative(no), "{no:?}");
        }
    }

    #[test]
    fn the_cooldown_has_a_ceiling() {
        let (_tmp, b) = temp_budget();
        for _ in 0..10 {
            b.start_cooldown("429", Duration::from_secs(12 * 3600))
                .unwrap();
        }
        let until = b.cooldown().unwrap().unwrap();
        assert!(until - now_ms() <= MAX_COOLDOWN_MS);
    }

    /// Proves the budget really is shared between connections, which is what
    /// will keep the CLI and the v2 service from spending at once.
    #[test]
    fn two_connections_share_the_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shared.db");
        let _db = super::super::Store::open_at(&path).unwrap();

        let one = SqliteRateBudget::over(Connection::open(&path).unwrap());
        let two = SqliteRateBudget::over(Connection::open(&path).unwrap());

        for _ in 0..15 {
            one.reserve().unwrap();
        }
        for _ in 0..15 {
            two.reserve().unwrap();
        }

        // Thirty reservations comfortably exceed the burst of twenty, so the
        // next one has to throttle even on the first connection.
        assert!(
            one.reserve().unwrap() > Duration::ZERO,
            "both connections should share one budget"
        );
    }
}
