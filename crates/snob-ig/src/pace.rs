//! Request pacing and cancellation.
//!
//! The numbers in [`Pace`] are copied from InstagramUnfollowers, which has
//! years of real use without incident, and have only been changed to make
//! *fewer* requests. **They are not changed without a documented reason** —
//! each one carries below what it is for, and that is what stops a number being
//! quietly tuned down until the tool is asking far more of Instagram's service
//! than answering the question needs.
//!
//! Two limits on what that provenance covers, both worth knowing before
//! leaning on it:
//!
//! - **It covers the cadence, not the error handling.** The reference project
//!   has none: its whole failure path is `catch { continue; }`, which re-enters
//!   the loop without advancing the cursor and without sleeping — an unbounded
//!   retry against Instagram on any failure, and precisely the pattern this
//!   project forbids. The hard stop here is a deliberate improvement on it, not
//!   a copy of it, and nobody should "restore fidelity" by removing it.
//! - **It covers the cadence, not the total volume.** The reference walks one
//!   list: it reads who you follow and derives the rest from a per-account flag
//!   in the same response. This walks both lists, so for a symmetric account it
//!   spends roughly twice the requests for the same answer. What offsets that is
//!   a budget that persists across runs, which the reference also does not have.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use snob_core::store::rate_budget::RateBudget;

/// Request cadence during a walk.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    /// Users per page. The reference project asks for 24 over GraphQL; this
    /// asks for 50 over REST.
    ///
    /// **It does not halve the requests.** Independent measurement in 2026 puts
    /// the followers endpoint at about 25 accounts per response whatever is
    /// asked for — a thousand followers costs around forty requests either way
    /// — which is what the settled note in AGENTS.md already says and what this
    /// comment used to contradict. It is kept at 50 because it costs nothing,
    /// it is honoured on the following list, and asking for less would only
    /// ever mean more requests.
    pub per_page: u32,
    /// Short pause before each request.
    pub micro_pause_ms: (u64, u64),
    /// Wait after processing each page.
    pub cycle_wait_ms: (u64, u64),
    /// Periodic long pause.
    pub long_pause_ms: (u64, u64),
    /// How many pages between long pauses.
    pub pages_per_long_pause: u32,
    /// How many times a network failure is retried before giving up.
    pub network_retries: u32,
    /// Base of the exponential delay between retries.
    pub backoff_base_ms: u64,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            per_page: 50,
            micro_pause_ms: (500, 2_000),
            cycle_wait_ms: (1_000, 1_300),
            long_pause_ms: (5_000, 15_000),
            // The original project calls its constant "after five", but its
            // condition is `scrollCycle > 6`, which is every seven. The
            // behavior is what gets copied, not the name.
            pages_per_long_pause: 7,
            network_retries: 3,
            backoff_base_ms: 2_000,
        }
    }
}

impl Pace {
    /// Cadence for walking somebody else's lists.
    ///
    /// Reading an account that is not yours is a heavier thing to ask for than
    /// reading your own, and Instagram is correspondingly readier to refuse it,
    /// so the walk is stretched out: every wait is roughly two to three times
    /// the default and the long pause comes round almost twice as often. Going
    /// slower is the courtesy owed to whoever's service and whoever's account
    /// this is, neither of them ours.
    ///
    /// `per_page` deliberately stays at 50. Asking for smaller pages could only
    /// mean more requests for the same users, and requests are the thing being
    /// counted — slowing down must not turn into knocking more often.
    ///
    /// The cost is that a walk takes about three times as long, and the resume
    /// window is measured from when it started. Somewhere past four thousand
    /// accounts an interrupted walk stops being resumable and begins again from
    /// the top. That is the right trade anyway: a walk that long no longer
    /// describes a single moment, which is what the window is there to protect.
    pub fn third_party() -> Self {
        Self {
            micro_pause_ms: (1_500, 4_000),
            cycle_wait_ms: (2_500, 4_000),
            long_pause_ms: (10_000, 30_000),
            pages_per_long_pause: 4,
            // One fewer than the default: on somebody else's account, a network
            // failure is a reason to stop rather than to insist.
            network_retries: 2,
            backoff_base_ms: 3_000,
            ..Self::default()
        }
    }

    pub(crate) fn micro_pause(&self) -> Duration {
        jitter(self.micro_pause_ms)
    }

    pub(crate) fn cycle_wait(&self) -> Duration {
        jitter(self.cycle_wait_ms)
    }

    pub(crate) fn long_pause(&self) -> Duration {
        jitter(self.long_pause_ms)
    }

    pub(crate) fn backoff(&self, attempt: u32) -> Duration {
        let base = self.backoff_base_ms.saturating_mul(1u64 << attempt.min(6));
        Duration::from_millis(base)
    }
}

/// Random wait inside the range, both ends included.
fn jitter((min, max): (u64, u64)) -> Duration {
    if max <= min {
        return Duration::from_millis(min);
    }
    Duration::from_millis(fastrand::u64(min..=max))
}

/// Who pays for a request, and who gets told when paying means waiting.
///
/// It exists so that making a request and paying for it cannot come apart.
/// Before this, every caller reserved budget on its own and three of them went
/// uncounted for three phases; now the only way to reach Instagram is through
/// [`crate::client::IgClient`], and the only way through it is past here.
///
/// The wait it imposes **adds to** the walker's own micro pause rather than
/// replacing it, so a walk running on an exhausted budget goes slightly slower
/// than the arithmetic suggests. That is the safe direction to be wrong in, and
/// it only happens once the budget is already rationing.
pub struct Pacer {
    budget: Arc<dyn RateBudget>,
    cancel: CancelToken,
    /// Told before a wait, so whoever is driving can say why nothing is
    /// happening. `None` stays silent, which is what tests and machine-readable
    /// runs want.
    announce: Option<Arc<dyn Fn(Duration) + Send + Sync>>,
    /// How many requests have been paid for. See [`Pacer::spent`].
    spent: AtomicU32,
}

impl Pacer {
    pub fn new(budget: Arc<dyn RateBudget>) -> Self {
        Self {
            budget,
            cancel: CancelToken::default(),
            announce: None,
            spent: AtomicU32::new(0),
        }
    }

    /// How many requests have been paid for since the client was built.
    ///
    /// This is the honest number, and the only one: it counts what the budget
    /// was charged, so retries, the profile lookup and the counter poll are all
    /// in it. Counting successful pages instead — which is what the walker used
    /// to report — understated a walk that hit a 503 and recovered, telling the
    /// user three requests while the budget had been charged five.
    pub fn spent(&self) -> u32 {
        self.spent.load(Ordering::Relaxed)
    }

    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Sets who gets told about a wait the budget imposed.
    pub fn announcing(mut self, announce: Arc<dyn Fn(Duration) + Send + Sync>) -> Self {
        self.announce = Some(announce);
        self
    }

    /// Grants everything and counts nothing. **Tests only**: using it against
    /// Instagram skips rate control entirely.
    #[doc(hidden)]
    pub fn unlimited() -> Self {
        Self::new(Arc::new(snob_core::store::rate_budget::UnlimitedRateBudget))
    }

    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    /// Until when the account is in cooldown, if it is.
    pub fn cooldown(&self) -> Result<Option<i64>, crate::error::IgError> {
        self.budget
            .cooldown()
            .map_err(|e| crate::error::IgError::Budget(e.to_string()))
    }

    /// Records that Instagram pushed back, so the next run does not walk
    /// straight into it again.
    pub fn start_cooldown(
        &self,
        reason: &str,
        minimum: Duration,
    ) -> Result<i64, crate::error::IgError> {
        self.budget
            .start_cooldown(reason, minimum)
            .map_err(|e| crate::error::IgError::Budget(e.to_string()))
    }

    /// Charges the budget for one request, off the async worker.
    ///
    /// `reserve` opens an immediate transaction against a database this process
    /// does not have to itself — the v2 service is meant to share it — so under
    /// contention it sits on the five-second busy timeout. That is a long time
    /// to hold a runtime worker, and this runs before every single request.
    async fn reserve(&self) -> Result<Duration, crate::error::IgError> {
        let budget = Arc::clone(&self.budget);
        // `spawn_blocking` rather than `block_in_place`, which would be simpler
        // and needs no clone: `block_in_place` panics on a current-thread
        // runtime, and that is what `#[tokio::test]` builds by default. A tool
        // whose tests cannot run it is not a tool this code can use.
        tokio::task::spawn_blocking(move || budget.reserve())
            .await
            .map_err(|e| crate::error::IgError::Budget(format!("the budget task failed: {e}")))?
            .map_err(|e| crate::error::IgError::Budget(e.to_string()))
    }

    /// Takes a slot and waits for it. Every request goes through here.
    ///
    /// Which is why the cancellation is read here rather than left to each
    /// caller: the token was only honored *inside* the wait, so with nothing
    /// owed a canceled run kept sending. Every loop that had to remember to
    /// check between requests was a loop that could forget, and two of them
    /// had — the monitor kept walking its second list and posting its queued
    /// reports after the user had asked it to stop. "No request is sent after
    /// cancellation" now lives in the same place as "every request is paid
    /// for", and is as hard to get around.
    ///
    /// Before the reservation, not after: refusing to send and charging for it
    /// anyway is the one combination that helps nobody.
    pub(crate) async fn clear_to_send(&self) -> Result<(), crate::error::IgError> {
        if self.cancel.is_canceled() {
            return Err(crate::error::IgError::Canceled);
        }

        let owed = self.reserve().await?;
        // Counted at the reservation rather than at the answer: the budget has
        // been charged by now whatever the server goes on to say.
        self.spent.fetch_add(1, Ordering::Relaxed);

        if owed.is_zero() {
            return Ok(());
        }
        if let Some(announce) = &self.announce {
            announce(owed);
        }
        if self.cancel.sleep_or_cancel(owed).await {
            return Err(crate::error::IgError::Canceled);
        }
        Ok(())
    }
}

/// Shared cancellation token.
///
/// Used instead of awaiting the signal directly on every iteration because a
/// `select!` inside a loop rebuilds its branches each time round, which would
/// throw away the signal future over and over.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl CancelToken {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn is_canceled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    async fn wait_for_cancel(&self) {
        loop {
            // Register BEFORE checking the flag. The other way round loses a
            // notification arriving between the check and the registration.
            let pending = self.notify.notified();
            if self.is_canceled() {
                return;
            }
            pending.await;
        }
    }

    /// Sleeps, or returns early on cancellation. Returns `true` if canceled.
    pub async fn sleep_or_cancel(&self, duration: Duration) -> bool {
        if self.is_canceled() {
            return true;
        }
        tokio::select! {
            biased;
            _ = self.wait_for_cancel() => true,
            _ = tokio::time::sleep(duration) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_lands_inside_the_range() {
        for _ in 0..200 {
            let d = jitter((500, 2_000)).as_millis() as u64;
            assert!((500..=2_000).contains(&d), "{d} is out of range");
        }
    }

    #[test]
    fn a_degenerate_range_does_not_panic() {
        // fastrand panics on an empty range, and the test pace has zeroes.
        assert_eq!(jitter((0, 0)), Duration::ZERO);
        assert_eq!(jitter((100, 100)), Duration::from_millis(100));
        assert_eq!(jitter((100, 50)), Duration::from_millis(100));
    }

    #[test]
    fn the_default_pace_is_the_documented_one() {
        let p = Pace::default();
        assert_eq!(p.per_page, 50);
        assert_eq!(p.pages_per_long_pause, 7);
        assert_eq!(p.micro_pause_ms, (500, 2_000));
        assert_eq!(p.cycle_wait_ms, (1_000, 1_300));
        assert_eq!(p.long_pause_ms, (5_000, 15_000));
    }

    #[test]
    fn the_third_party_pace_is_slower_everywhere_but_asks_no_more_often() {
        let own = Pace::default();
        let other = Pace::third_party();

        // The same users in the same number of requests.
        assert_eq!(other.per_page, own.per_page);

        assert!(other.micro_pause_ms.0 > own.micro_pause_ms.0);
        assert!(other.cycle_wait_ms.0 > own.cycle_wait_ms.0);
        assert!(other.long_pause_ms.0 > own.long_pause_ms.0);
        assert!(other.pages_per_long_pause < own.pages_per_long_pause);
        assert!(other.network_retries < own.network_retries);

        assert_eq!(other.micro_pause_ms, (1_500, 4_000));
        assert_eq!(other.cycle_wait_ms, (2_500, 4_000));
        assert_eq!(other.long_pause_ms, (10_000, 30_000));
        assert_eq!(other.pages_per_long_pause, 4);
    }

    #[test]
    fn the_backoff_grows_and_does_not_overflow() {
        let p = Pace::default();
        assert_eq!(p.backoff(0), Duration::from_millis(2_000));
        assert_eq!(p.backoff(1), Duration::from_millis(4_000));
        assert_eq!(p.backoff(2), Duration::from_millis(8_000));
        // Even given an absurd number.
        assert!(p.backoff(99) <= Duration::from_millis(2_000 * 64));
    }

    #[tokio::test]
    async fn canceling_interrupts_a_long_wait() {
        let c = CancelToken::default();
        let copy = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            copy.cancel();
        });

        let start = std::time::Instant::now();
        let canceled = c.sleep_or_cancel(Duration::from_secs(30)).await;
        assert!(canceled);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the wait was not interrupted"
        );
    }

    #[tokio::test]
    async fn without_cancellation_the_wait_finishes_on_its_own() {
        let c = CancelToken::default();
        assert!(!c.sleep_or_cancel(Duration::from_millis(1)).await);
    }

    #[tokio::test]
    async fn canceling_before_waiting_is_still_noticed() {
        let c = CancelToken::default();
        c.cancel();
        assert!(c.sleep_or_cancel(Duration::from_secs(30)).await);
    }

    /// A canceled run is never cleared to send, even with nothing owed.
    ///
    /// The token used to be read only inside the wait, so in the ordinary case
    /// — a budget that owes nothing — `clear_to_send` answered `Ok` and the
    /// request went out. That left "stop when asked" as something every loop
    /// between requests had to remember, and two of them did not: the monitor
    /// walked its second list and drained its webhook queue after the user had
    /// pressed Ctrl+C.
    #[tokio::test]
    async fn a_canceled_run_is_not_cleared_to_send() {
        let pacer = Pacer::unlimited();
        pacer
            .clear_to_send()
            .await
            .expect("nothing is cancelled yet");
        assert_eq!(pacer.spent(), 1);

        pacer.cancel_token().cancel();

        let error = pacer.clear_to_send().await.unwrap_err();
        assert!(matches!(error, crate::error::IgError::Canceled));
        assert_eq!(
            pacer.spent(),
            1,
            "a request that is refused is not charged for"
        );
    }
}
