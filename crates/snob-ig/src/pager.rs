//! Paginated walk over a followers or following list.
//!
//! The loop the project we took inspiration from is missing: theirs does
//! `catch { continue }`, which skips every wait and repeats the request at
//! once, so throttling turns it into a burst against Instagram. Here every stop
//! condition is explicit.

use std::error::Error;
use std::time::Duration;

use snob_core::Pk;
use snob_core::model::StopReason;

use crate::client::{Direction, IgClient};
use crate::error::{IgError, Reaction};
use crate::model::FriendshipsPage;
use crate::pace::{CancelToken, Pace};

/// Hard page ceiling. No failure should ever be able to produce an endless
/// stream of requests, whatever happens to the other stop conditions.
pub const HARD_PAGE_CAP: u32 = 2_000;

/// Past this **declared** size, falling short smells like Instagram truncating
/// the list rather than the counter lying.
///
/// It is a guess based on unverified reports. That is why it is a named
/// constant and why both numbers are logged: so real data can correct it.
///
/// It is compared against what Instagram declared, not against what was walked.
/// The other way round was a hole: a walk that stopped at six thousand of a
/// declared forty thousand fell under the threshold on the walked count and was
/// reported as a complete list, which is precisely the case the threshold
/// exists for.
pub const TRUNCATION_THRESHOLD: usize = 10_000;

/// How many pages in a row without a single new account are tolerated before
/// assuming the list is going round in circles.
const MAX_PAGES_WITHOUT_NEW: u32 = 3;

/// The waits a walk pays. The budget's own wait is not among them: it is
/// imposed inside the client, announced by the [`crate::pace::Pacer`], and the
/// walker never learns of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitKind {
    Micro,
    Cycle,
    Long,
}

/// What happens as the walk proceeds.
///
/// An enum and an `FnMut` rather than a trait: this crate depends on no
/// presentation library, and tests can spy on the exact sequence with a `Vec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Started {
        estimated: Option<u64>,
        resumed: bool,
    },
    Page {
        number: u32,
        received: usize,
        added: usize,
        running_total: usize,
    },
    Waiting {
        kind: WaitKind,
        duration: Duration,
    },
    Retrying {
        attempt: u32,
        after: Duration,
        error: String,
    },
    Warning(String),
    Finished {
        pages: u32,
        users: usize,
        reason: StopReason,
    },
}

/// What to walk.
#[derive(Debug, Clone)]
pub struct ListRequest<'a> {
    pub pk: Pk,
    /// Only used to name the page a browser would have called from. Empty is
    /// allowed and simply leaves the referer generic.
    pub username: &'a str,
    pub direction: Direction,
    /// Cursor to continue an interrupted walk from.
    pub from: Option<&'a str>,
    /// How many are expected, for progress. Never a stop condition.
    pub estimated: Option<u64>,
    /// Page cap requested by the caller.
    pub max_pages: Option<u32>,
    /// How many were already stored, when resuming.
    pub already_stored: usize,
}

/// How the walk ended.
#[derive(Debug)]
pub struct WalkSummary {
    pub pages: u32,
    pub users: usize,
    pub reason: StopReason,
    /// Where it left off, if anything is left.
    pub pending_cursor: Option<String>,
    /// The error that cut the walk short, if any.
    pub error: Option<IgError>,
}

impl WalkSummary {
    pub fn is_complete(&self) -> bool {
        self.reason.yields_complete_list()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    #[error("the account is in cooldown for another {}", minutes(.remaining_ms))]
    Cooldown { until_ms: i64, remaining_ms: i64 },
    #[error(transparent)]
    Budget(IgError),
    #[error("could not save the page: {0}")]
    Save(Box<dyn Error + Send + Sync>),
}

/// "1 minute" / "45 minutes"; a remainder under a minute still says 1.
fn minutes(remaining_ms: &i64) -> String {
    match (remaining_ms / 60_000).max(1) {
        1 => "1 minute".to_string(),
        n => format!("{n} minutes"),
    }
}

pub struct ListWalker<'a> {
    client: &'a IgClient,
    pace: Pace,
    cancel: CancelToken,
    sleeps: bool,
}

impl<'a> ListWalker<'a> {
    /// Rate control is not a parameter any more: it is inside the client, which
    /// cannot be built without it. Walking without it stopped being something
    /// review has to catch and became something that cannot be written.
    ///
    /// **The client also decides whether the waits are real**, by the server it
    /// is pointed at. This was `without_sleeping()`, a `#[doc(hidden)]` method
    /// any caller could reach for — so a walk against Instagram with no waits
    /// between pages was one line away, and the rule against it lived in a
    /// doc-comment. It is now unreachable: a mock server is not Instagram and
    /// Instagram is not a mock server.
    pub fn new(client: &'a IgClient) -> Self {
        Self {
            client,
            pace: Pace::default(),
            cancel: CancelToken::default(),
            sleeps: client.is_live(),
        }
    }

    pub fn with_pace(mut self, pace: Pace) -> Self {
        self.pace = pace;
        self
    }

    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Walks the whole list, page by page.
    ///
    /// `save` receives each page as soon as it arrives and **before any wait**,
    /// so the caller can persist it immediately; it returns how many users were
    /// genuinely new.
    pub async fn walk<S, O>(
        &self,
        request: ListRequest<'_>,
        mut save: S,
        mut observe: O,
    ) -> Result<WalkSummary, WalkError>
    where
        S: FnMut(&FriendshipsPage, u32) -> Result<usize, Box<dyn Error + Send + Sync>>,
        O: FnMut(Event),
    {
        self.check_cooldown()?;

        observe(Event::Started {
            estimated: request.estimated,
            resumed: request.from.is_some(),
        });

        let mut state = WalkState::new(&request);

        let reason = loop {
            if self.cancel.is_canceled() {
                break StopReason::Canceled;
            }
            if let Some(end) = state.cap_reached(&request) {
                break end;
            }

            // The budget's own wait is not paid here. It happens inside the
            // client, after this one, so an exhausted budget makes the walk
            // slower than the two numbers suggest — the safe direction, and
            // only once it is already rationing.
            if self
                .wait(WaitKind::Micro, self.pace.micro_pause(), &mut observe)
                .await
            {
                break StopReason::Canceled;
            }

            let page = match self.fetch_page(&request, &state, &mut observe).await {
                Ok(p) => p,
                Err(e) => break self.stop_reason_for(e, &mut state),
            };
            let received = page.users.len();
            let added = save(&page, state.pages + 1).map_err(WalkError::Save)?;

            state.pages += 1;
            state.users += added;
            observe(Event::Page {
                number: state.pages,
                received,
                added,
                running_total: state.users,
            });

            if let Some(end) = state.record_page(received, added, &page, &mut observe) {
                break end;
            }

            // The next two waits are skipped when the loop is about to end:
            // otherwise every walk would pay up to fifteen seconds of pointless
            // waiting right at the finish.
            if self
                .wait(WaitKind::Cycle, self.pace.cycle_wait(), &mut observe)
                .await
            {
                break StopReason::Canceled;
            }
            if state.pages.is_multiple_of(self.pace.pages_per_long_pause)
                && self
                    .wait(WaitKind::Long, self.pace.long_pause(), &mut observe)
                    .await
            {
                break StopReason::Canceled;
            }
        };

        let reason = state.verify_completion(reason, &request, &mut observe);

        observe(Event::Finished {
            pages: state.pages,
            users: state.users,
            reason,
        });

        Ok(WalkSummary {
            pages: state.pages,
            users: state.users,
            reason,
            pending_cursor: if reason.yields_complete_list() {
                None
            } else {
                state.cursor.clone()
            },
            error: state.error,
        })
    }

    fn check_cooldown(&self) -> Result<(), WalkError> {
        let until = self.client.pacer().cooldown().map_err(WalkError::Budget)?;
        if let Some(until_ms) = until {
            let remaining_ms = until_ms - snob_core::store::now_ms();
            return Err(WalkError::Cooldown {
                until_ms,
                remaining_ms,
            });
        }
        Ok(())
    }

    /// Fetches a page, retrying only what is worth retrying.
    async fn fetch_page<O: FnMut(Event)>(
        &self,
        request: &ListRequest<'_>,
        state: &WalkState,
        observe: &mut O,
    ) -> Result<FriendshipsPage, IgError> {
        let mut attempt = 0;
        loop {
            let result = self
                .client
                .friendships_page(
                    request.pk,
                    request.username,
                    request.direction,
                    self.pace.per_page,
                    state.cursor.as_deref(),
                )
                .await;

            let error = match result {
                Ok(p) => return Ok(p),
                Err(e) => e,
            };

            // `reaction()` rather than the individual predicates: it is the
            // single authority here, and it is what guarantees a 429 is never
            // retried.
            if error.reaction() != Reaction::Retry || attempt >= self.pace.network_retries {
                return Err(error);
            }

            let after = self.pace.backoff(attempt);
            observe(Event::Retrying {
                attempt: attempt + 1,
                after,
                error: error.to_string(),
            });
            // `Canceled`, not the server's error. Both outcomes of the wait
            // used to return what Instagram had said, so a 503 on page nine
            // plus Ctrl+C during the backoff was reported as a network failure:
            // an `error:` label, `stopped_by = 'network'` on the snapshot and
            // exit 1 — while the same interrupt half a second later exits 130
            // with nothing to explain. The user stopping is not the server
            // failing, whichever of the two happened first.
            if self.sleeps && self.cancel.sleep_or_cancel(after).await {
                return Err(IgError::Canceled);
            }
            attempt += 1;
        }
    }

    fn stop_reason_for(&self, error: IgError, state: &mut WalkState) -> StopReason {
        let reason = match error.reaction() {
            Reaction::Cooldown => StopReason::RateLimit,
            Reaction::Retry => StopReason::Network,
            Reaction::Abort => match error {
                IgError::SessionExpired
                | IgError::UserAgentMismatch
                | IgError::Challenge { .. }
                | IgError::Checkpoint { .. } => StopReason::SessionInvalid,
                // Ctrl+C during the budget's wait arrives as an error from the
                // client rather than through the token, and it is still the
                // user stopping rather than anything going wrong.
                IgError::Canceled => StopReason::Canceled,
                _ => StopReason::Network,
            },
        };
        state.error = Some(error);
        reason
    }

    /// Returns `true` if it was canceled while waiting.
    async fn wait<O: FnMut(Event)>(
        &self,
        kind: WaitKind,
        duration: Duration,
        observe: &mut O,
    ) -> bool {
        if duration.is_zero() {
            return false;
        }
        observe(Event::Waiting { kind, duration });
        if !self.sleeps {
            return self.cancel.is_canceled();
        }
        self.cancel.sleep_or_cancel(duration).await
    }
}

/// Mutable state of the walk, kept apart so the stop conditions are functions
/// rather than branches scattered through the loop.
struct WalkState {
    cursor: Option<String>,
    /// The cursor of the previous page, to catch one that does not advance.
    last_cursor: Option<String>,
    pages: u32,
    users: usize,
    empty_in_a_row: u32,
    barren_in_a_row: u32,
    error: Option<IgError>,
}

impl WalkState {
    fn new(request: &ListRequest<'_>) -> Self {
        Self {
            cursor: request.from.map(str::to_string),
            last_cursor: None,
            pages: 0,
            users: request.already_stored,
            empty_in_a_row: 0,
            barren_in_a_row: 0,
            error: None,
        }
    }

    fn cap_reached(&self, request: &ListRequest<'_>) -> Option<StopReason> {
        if let Some(max) = request.max_pages
            && self.pages >= max
        {
            return Some(StopReason::PageLimit);
        }
        if self.pages >= HARD_PAGE_CAP {
            return Some(StopReason::Truncated);
        }
        None
    }

    fn record_page<O: FnMut(Event)>(
        &mut self,
        received: usize,
        added: usize,
        page: &FriendshipsPage,
        observe: &mut O,
    ) -> Option<StopReason> {
        let next = page.next_cursor().map(str::to_string);

        // No cursor means the list is done.
        let Some(next) = next else {
            return Some(StopReason::Completed);
        };

        // A cursor that does not advance is a loop. This is the guard the
        // original project lacks.
        //
        // Compared against the one just **sent**, not only against the previous
        // page's: on the first page of a resumed walk there is no previous one,
        // so a cursor that comes straight back unchanged would have gone
        // undetected for one more request — and a walk that ends this way keeps
        // that cursor and stays resumable for fifteen minutes, so every run in
        // that window paid the same two requests to rediscover it.
        if self.last_cursor.as_deref() == Some(next.as_str())
            || self.cursor.as_deref() == Some(next.as_str())
        {
            observe(Event::Warning(
                "Instagram returned the same cursor twice; stopping so the request is not repeated"
                    .into(),
            ));
            return Some(StopReason::Truncated);
        }

        if received == 0 {
            self.empty_in_a_row += 1;
            if self.empty_in_a_row >= 2 {
                observe(Event::Warning(
                    "Instagram returned two empty pages in a row".into(),
                ));
                return Some(StopReason::Truncated);
            }
        } else {
            self.empty_in_a_row = 0;
        }

        if added == 0 {
            self.barren_in_a_row += 1;
            if self.barren_in_a_row >= MAX_PAGES_WITHOUT_NEW {
                observe(Event::Warning(
                    "several pages in a row with no new accounts; the list is going in circles"
                        .into(),
                ));
                return Some(StopReason::Truncated);
            }
        } else {
            self.barren_in_a_row = 0;
        }

        self.last_cursor = Some(next.clone());
        self.cursor = Some(next);
        None
    }

    /// Last filter: a clean but very short ending, measured against what was
    /// declared, is probably not clean at all.
    fn verify_completion<O: FnMut(Event)>(
        &self,
        reason: StopReason,
        request: &ListRequest<'_>,
        observe: &mut O,
    ) -> StopReason {
        if reason != StopReason::Completed {
            return reason;
        }

        // Nothing walked, and nothing to check that against.
        //
        // An account with no followers and an Instagram that served no
        // followers look identical from here: one page, no users, no cursor,
        // which `record_page` reads as the list being done. The counter is
        // what tells them apart, and this is the branch where the counter is
        // missing — the profile poll failed, which is the same bad afternoon
        // that produces the empty page.
        //
        // Getting it wrong here is not a partial result but a wrong one: an
        // empty followers list accepted as complete makes every account you
        // follow an unfollower. A real empty account loses nothing by being
        // asked again, so this refuses.
        if request.estimated.is_none() && self.users == 0 {
            observe(Event::Warning(
                "the list came back empty and the profile counter could not be read, so there is \
                 no way to tell an empty list from one Instagram did not serve; treating it as \
                 incomplete rather than risking the comparison"
                    .into(),
            ));
            return StopReason::Truncated;
        }

        let Some(estimated) = request.estimated else {
            return reason;
        };

        let estimated = estimated as usize;
        if self.users >= estimated {
            return reason;
        }

        // A shortfall of more than a tenth is more than deleted accounts
        // usually account for, and is what makes either explanation worth
        // weighing at all.
        let far_short = estimated > self.users * 11 / 10;
        if !far_short {
            return reason;
        }

        if truncated(self.users, estimated) {
            observe(Event::Warning(format!(
                "Instagram stopped serving pages at {} of the {estimated} accounts it declared; \
                 the list is incomplete and cannot be compared against",
                self.users
            )));
            return StopReason::Truncated;
        }

        observe(Event::Warning(format!(
            "walked {} accounts while Instagram declared {estimated}; \
             the difference is usually deleted accounts",
            self.users
        )));
        reason
    }
}

/// Which of the two explanations for a short list to believe.
///
/// A walk that ends cleanly with fewer accounts than declared is either the
/// counter lying — it includes deleted and deactivated accounts that no longer
/// appear — or Instagram having stopped serving pages. Getting it wrong in one
/// direction disables comparison for every account whose counter overstates;
/// getting it wrong in the other reports departures that never happened, which
/// is the failure this whole tool is built to avoid.
///
/// Two independent signals, either of which is enough:
///
/// - **The declared size is past the threshold.** That is where Instagram is
///   reported to stop paginating, so any real shortfall there is suspect. It is
///   measured against what was *declared*, not against what was walked: reading
///   six thousand of a declared forty thousand is exactly the case this is for,
///   and testing the walked count would have called it complete.
/// - **The shortfall is enormous whatever the size.** Deleted accounts are not
///   half of anybody's followers. A hundred out of three thousand is not a
///   counter that overstates, it is a list that stopped.
fn truncated(walked: usize, declared: usize) -> bool {
    declared >= TRUNCATION_THRESHOLD || walked * 2 < declared
}

#[cfg(test)]
mod tests {
    use snob_core::session::{Session, SessionOrigin};
    use url::Url;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::pace::Pacer;

    const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
    const SID: &str = "42%3AAbCdEfGh%3A20";

    #[test]
    fn the_cooldown_display_pluralizes_the_minutes() {
        let one = WalkError::Cooldown {
            until_ms: 0,
            remaining_ms: 30_000,
        };
        assert_eq!(
            one.to_string(),
            "the account is in cooldown for another 1 minute"
        );

        let four = WalkError::Cooldown {
            until_ms: 0,
            remaining_ms: 240_000,
        };
        assert_eq!(
            four.to_string(),
            "the account is in cooldown for another 4 minutes"
        );
    }

    fn client_with(server: &MockServer, pacer: Pacer) -> IgClient {
        let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        IgClient::new(session, pacer)
            .unwrap()
            .with_base_url(Url::parse(&server.uri()).unwrap())
    }

    fn client(server: &MockServer) -> IgClient {
        client_with(server, Pacer::unlimited())
    }

    /// A walk against Instagram pays every wait; one against a test server pays
    /// none, and neither is a choice a caller gets to make.
    ///
    /// This used to be `ListWalker::without_sleeping()`. It was `#[doc(hidden)]`
    /// and its doc said not to use it against Instagram, which is the weakest
    /// kind of guard there is: the whole of AGENTS.md's "never walk a real
    /// account's lists without the limiter" rested on nobody writing one line.
    /// Nothing asserted it either, so the rule could have been deleted and the
    /// suite would have stayed green.
    #[tokio::test]
    async fn only_a_walk_against_a_test_server_skips_the_waits() {
        let server = MockServer::start().await;
        assert!(
            !ListWalker::new(&client(&server)).sleeps,
            "a mock server is not Instagram, so there is nothing to be polite to"
        );

        let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        let live = IgClient::new(session, Pacer::unlimited()).unwrap();
        assert!(
            ListWalker::new(&live).sleeps,
            "a walk against Instagram has to pay its waits"
        );
    }

    /// A body with `n` users and, optionally, a cursor to the next page.
    fn body(from: u64, n: u64, cursor: Option<&str>) -> String {
        let users: Vec<String> = (from..from + n)
            .map(|i| format!(r#"{{"pk":{i},"username":"u{i}"}}"#))
            .collect();
        let cursor = match cursor {
            Some(c) => format!(r#","next_max_id":"{c}""#),
            None => String::new(),
        };
        format!(r#"{{"users":[{}]{cursor}}}"#, users.join(","))
    }

    fn request<'a>() -> ListRequest<'a> {
        ListRequest {
            pk: 42,
            username: "someone",
            direction: Direction::Followers,
            from: None,
            estimated: None,
            max_pages: None,
            already_stored: 0,
        }
    }

    /// Serves a scripted sequence of responses, one per request.
    async fn server(responses: Vec<ResponseTemplate>) -> MockServer {
        let server = MockServer::start().await;
        for (i, r) in responses.into_iter().enumerate() {
            Mock::given(method("GET"))
                .respond_with(r)
                .up_to_n_times(1)
                .with_priority(i as u8 + 1)
                .mount(&server)
                .await;
        }
        server
    }

    fn ok(body: String) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string(body)
    }

    /// Walks collecting the events, without sleeping and with a free budget.
    async fn walk(
        server: &MockServer,
        request: ListRequest<'_>,
    ) -> (WalkSummary, Vec<Event>, Vec<u64>) {
        let client = client(server);
        let walker = ListWalker::new(&client);

        let mut events = Vec::new();
        let mut seen: Vec<u64> = Vec::new();

        let summary = walker
            .walk(
                request,
                |page, _| {
                    let before = seen.len();
                    for u in &page.users {
                        if !seen.contains(&u.pk) {
                            seen.push(u.pk);
                        }
                    }
                    Ok(seen.len() - before)
                },
                |e| events.push(e),
            )
            .await
            .unwrap();

        (summary, events, seen)
    }

    /// The one case where a clean ending is not a clean ending.
    ///
    /// One page, no users, no cursor: `record_page` reads that as the list
    /// being done, and it is also what Instagram serving nothing looks like.
    /// The declared counter is what tells the two apart, and the walk that
    /// hits this is the one whose profile poll already failed.
    ///
    /// Accepted as complete, the empty followers list makes every account you
    /// follow an unfollower — not a partial answer but a wrong one.
    #[tokio::test]
    async fn an_empty_list_with_no_counter_to_check_it_against_is_refused() {
        let server = server(vec![ok(body(0, 0, None))]).await;
        let (summary, events, _) = walk(&server, request()).await;

        assert_eq!(summary.users, 0);
        assert_eq!(summary.reason, StopReason::Truncated);
        assert!(!summary.is_complete());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Warning(w) if w.contains("came back empty"))),
            "{events:?}"
        );
    }

    /// The same answer, with the counter agreeing that the account really has
    /// nobody. Nothing to be suspicious of, and refusing it would make an
    /// empty account permanently unusable.
    #[tokio::test]
    async fn an_empty_list_the_counter_confirms_is_complete() {
        let server = server(vec![ok(body(0, 0, None))]).await;
        let request = ListRequest {
            estimated: Some(0),
            ..request()
        };
        let (summary, _, _) = walk(&server, request).await;

        assert_eq!(summary.reason, StopReason::Completed);
        assert!(summary.is_complete());
    }

    #[tokio::test]
    async fn it_walks_until_the_cursor_runs_out() {
        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ok(body(100, 20, None)),
        ])
        .await;

        let (summary, _, seen) = walk(&server, request()).await;

        assert_eq!(summary.reason, StopReason::Completed);
        assert!(summary.is_complete());
        assert_eq!(summary.pages, 3);
        assert_eq!(summary.users, 120);
        assert_eq!(seen.len(), 120);
        assert_eq!(summary.pending_cursor, None);
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    /// Pins the pacing policy down: the long pause lands after the seventh and
    /// fourteenth page and nowhere else, every wait is inside its documented
    /// range, and none is paid after the last page.
    #[tokio::test]
    async fn the_cadence_is_the_documented_one() {
        let mut responses: Vec<ResponseTemplate> = (0..15)
            .map(|i| ok(body(i * 50, 50, Some(&format!("c{i}")))))
            .collect();
        responses.push(ok(body(750, 10, None)));
        let server = server(responses).await;

        let client = client(&server);
        // PRODUCTION pace: the real policy is what is under test.
        let walker = ListWalker::new(&client).with_pace(Pace::default());

        let mut waits = Vec::new();
        walker
            .walk(
                request(),
                |p, _| Ok(p.users.len()),
                |e| {
                    if let Event::Waiting { kind, duration } = e {
                        waits.push((kind, duration.as_millis() as u64));
                    }
                },
            )
            .await
            .unwrap();

        let long: Vec<usize> = waits
            .iter()
            .enumerate()
            .filter(|(_, (k, _))| *k == WaitKind::Long)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(long.len(), 2, "the long pause must land twice in 16 pages");

        for (kind, ms) in &waits {
            let range = match kind {
                WaitKind::Micro => (500, 2_000),
                WaitKind::Cycle => (1_000, 1_300),
                WaitKind::Long => (5_000, 15_000),
            };
            assert!(
                (range.0..=range.1).contains(ms),
                "{kind:?} of {ms} ms is outside {range:?}"
            );
        }

        // Nothing is waited after the last page: the final entry is the micro
        // pause of request number sixteen.
        assert_eq!(
            waits.last().map(|(k, _)| *k),
            Some(WaitKind::Micro),
            "there should be no waits after the last page"
        );
    }

    #[tokio::test]
    async fn a_repeating_cursor_stops_the_loop() {
        // Instagram always returning the same cursor: without the guard this is
        // an endless burst of requests.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ok(body(0, 50, Some("always_the_same"))))
            .mount(&server)
            .await;

        let (summary, events, _) = walk(&server, request()).await;

        assert_eq!(summary.reason, StopReason::Truncated);
        assert_eq!(summary.pages, 2, "it should stop as soon as it repeats");
        assert!(events.iter().any(|e| matches!(e, Event::Warning(_))));
    }

    #[tokio::test]
    async fn two_empty_pages_in_a_row_cut_it_short() {
        let server = server(vec![
            ok(body(0, 10, Some("c1"))),
            ok(body(0, 0, Some("c2"))),
            ok(body(0, 0, Some("c3"))),
        ])
        .await;

        let (summary, _, _) = walk(&server, request()).await;
        assert_eq!(summary.reason, StopReason::Truncated);
        assert!(!summary.is_complete());
    }

    #[tokio::test]
    async fn throttling_stops_without_retrying() {
        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ResponseTemplate::new(429).set_body_string(r#"{"message":"","spam":true}"#),
        ])
        .await;

        let (summary, _, _) = walk(&server, request()).await;

        assert_eq!(summary.reason, StopReason::RateLimit);
        assert!(!summary.is_complete());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            3,
            "a 429 must not cause a fourth request"
        );
        assert_eq!(summary.pending_cursor.as_deref(), Some("c2"));
    }

    #[tokio::test]
    async fn an_expired_session_stops_and_says_so() {
        let server =
            server(vec![ResponseTemplate::new(403).set_body_string(
                r#"{"message":"login_required","status":"fail"}"#,
            )])
            .await;

        let (summary, _, _) = walk(&server, request()).await;
        assert_eq!(summary.reason, StopReason::SessionInvalid);
        assert!(matches!(summary.error, Some(IgError::SessionExpired)));
    }

    #[tokio::test]
    async fn a_server_error_is_retried_and_then_given_up_on() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string("oops"))
            .mount(&server)
            .await;

        let (summary, events, _) = walk(&server, request()).await;

        let retries = events
            .iter()
            .filter(|e| matches!(e, Event::Retrying { .. }))
            .count();
        assert_eq!(retries, 3, "it should retry three times");
        assert_eq!(summary.reason, StopReason::Network);
    }

    /// Ctrl+C during a retry backoff is the user stopping, not the server
    /// failing.
    ///
    /// Both outcomes of the wait used to return the server's error, so a 503 on
    /// page nine plus an interrupt during the two-second backoff was reported as
    /// a network failure — an `error:` label, `stopped_by = 'network'` and exit
    /// 1 — while the same interrupt half a second later exits 130 with nothing
    /// to explain.
    ///
    /// The waits have to be on for this: the backoff is where the token is
    /// read, and a walk against a test server does not wait at all. Everything
    /// else in the pace is set to zero so the only real wait is the one under
    /// test, and the cancellation is fired from the `Retrying` event, which is
    /// emitted immediately before it.
    #[tokio::test]
    async fn canceling_during_a_backoff_is_not_a_network_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string("oops"))
            .mount(&server)
            .await;

        let client = client(&server);
        let cancel = CancelToken::default();
        let pace = Pace {
            micro_pause_ms: (0, 0),
            cycle_wait_ms: (0, 0),
            long_pause_ms: (0, 0),
            backoff_base_ms: 30_000,
            ..Pace::default()
        };
        let mut walker = ListWalker::new(&client)
            .with_pace(pace)
            .with_cancel(cancel.clone());
        walker.sleeps = true;

        let summary = walker
            .walk(
                request(),
                |p, _| Ok(p.users.len()),
                |event| {
                    if matches!(event, Event::Retrying { .. }) {
                        cancel.cancel();
                    }
                },
            )
            .await
            .unwrap();

        assert_eq!(summary.reason, StopReason::Canceled);
        assert!(
            matches!(summary.error, Some(IgError::Canceled)),
            "the server's error must not survive the user's interrupt: {:?}",
            summary.error
        );
    }

    #[tokio::test]
    async fn the_page_cap_stops_it_and_keeps_the_cursor() {
        // Distinct cursors per page: otherwise the repeating-cursor guard would
        // fire first.
        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ok(body(100, 50, Some("c3"))),
        ])
        .await;

        let mut r = request();
        r.max_pages = Some(2);
        let (summary, _, _) = walk(&server, r).await;

        assert_eq!(summary.reason, StopReason::PageLimit);
        assert_eq!(summary.pages, 2);
        assert_eq!(summary.pending_cursor.as_deref(), Some("c2"));
        assert!(
            !summary.is_complete(),
            "a walk cut short does not describe the whole list"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "the cap has to save real requests"
        );
    }

    #[tokio::test]
    async fn canceling_stops_at_the_page_boundary() {
        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ok(body(100, 50, Some("c3"))),
        ])
        .await;

        let client = client(&server);
        let cancel = CancelToken::default();
        let walker = ListWalker::new(&client).with_cancel(cancel.clone());

        let summary = walker
            .walk(
                request(),
                |p, number| {
                    if number == 2 {
                        cancel.cancel();
                    }
                    Ok(p.users.len())
                },
                |_| {},
            )
            .await
            .unwrap();

        assert_eq!(summary.reason, StopReason::Canceled);
        assert_eq!(summary.pages, 2);
        assert!(summary.pending_cursor.is_some());
    }

    #[tokio::test]
    async fn resuming_starts_from_the_stored_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ok(body(0, 10, None)))
            .mount(&server)
            .await;

        let mut r = request();
        r.from = Some("from_here");
        r.already_stored = 100;
        let (summary, events, _) = walk(&server, r).await;

        assert!(matches!(
            events.first(),
            Some(Event::Started { resumed: true, .. })
        ));
        // What was already stored counts towards the total.
        assert_eq!(summary.users, 110);

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests[0]
                .url
                .query()
                .unwrap()
                .contains("max_id=from_here"),
            "the first request should continue from the cursor"
        );
    }

    #[tokio::test]
    async fn falling_short_on_a_small_list_is_not_truncation() {
        // Instagram's counter includes deleted accounts that no longer appear.
        let server = server(vec![ok(body(0, 80, None))]).await;

        let mut r = request();
        r.estimated = Some(100);
        let (summary, events, _) = walk(&server, r).await;

        assert_eq!(summary.reason, StopReason::Completed);
        assert!(
            summary.is_complete(),
            "80 out of 100 on a small list is normal"
        );
        assert!(events.iter().any(|e| matches!(e, Event::Warning(_))));
    }

    /// The rule that decides between "the counter overstates" and "Instagram
    /// stopped serving", pinned down without walking anything.
    #[test]
    fn the_two_explanations_for_a_short_list_are_told_apart() {
        // A tenth missing off a small list is the counter counting the dead.
        assert!(!truncated(80, 100));
        assert!(!truncated(270, 300));

        // Half missing is not, whatever the size. This is the case that used
        // to be reported as a complete list.
        assert!(truncated(100, 3_000));
        assert!(truncated(1_000, 2_001));

        // Past the declared threshold, any real shortfall is suspect — and it
        // is the declared count that is measured, not the walked one.
        assert!(truncated(6_000, 40_000));
        assert!(truncated(9_000, 10_000));

        // Just under the threshold with a believable difference stays clean.
        assert!(!truncated(8_000, 9_999));
    }

    /// The regression the rule above exists for: a walk that ends cleanly far
    /// short of a large declared count must not become a basis for comparison.
    #[tokio::test]
    async fn stopping_far_short_of_a_large_counter_is_truncation() {
        let server = server(vec![ok(body(0, 50, None))]).await;

        let mut r = request();
        r.estimated = Some(40_000);
        let (summary, _, _) = walk(&server, r).await;

        assert_eq!(summary.reason, StopReason::Truncated);
        assert!(
            !summary.is_complete(),
            "50 accounts out of a declared 40,000 is a list that stopped, not a counter that lied"
        );
    }

    #[tokio::test]
    async fn falling_short_on_a_large_list_is_truncation() {
        let mut responses: Vec<ResponseTemplate> = (0..219)
            .map(|i| ok(body(i * 50, 50, Some(&format!("c{i}")))))
            .collect();
        responses.push(ok(body(10_950, 50, None)));
        let server = server(responses).await;

        let mut r = request();
        r.estimated = Some(30_000);
        let (summary, _, _) = walk(&server, r).await;

        assert_eq!(summary.reason, StopReason::Truncated);
        assert!(
            !summary.is_complete(),
            "a large list that gets cut short cannot be compared against"
        );
    }

    #[tokio::test]
    async fn a_cooldown_prevents_starting() {
        use snob_core::store::rate_budget::{RateBudget, RateBudgetError};

        struct InCooldown;
        impl RateBudget for InCooldown {
            fn reserve(&self) -> Result<Duration, RateBudgetError> {
                Ok(Duration::ZERO)
            }
            fn cooldown(&self) -> Result<Option<i64>, RateBudgetError> {
                Ok(Some(snob_core::store::now_ms() + 3_600_000))
            }
            fn start_cooldown(&self, _: &str, _: Duration) -> Result<i64, RateBudgetError> {
                Ok(0)
            }
        }

        let server = MockServer::start().await;
        let client = client_with(&server, Pacer::new(std::sync::Arc::new(InCooldown)));
        let walker = ListWalker::new(&client);

        let error = walker
            .walk(request(), |p, _| Ok(p.users.len()), |_| {})
            .await
            .unwrap_err();

        assert!(matches!(error, WalkError::Cooldown { .. }));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            0,
            "in cooldown not a single request is made"
        );
    }

    /// The point of moving the budget into the client: a walk pays for every
    /// page without the walker having to remember to.
    #[tokio::test]
    async fn every_page_is_charged_to_the_budget() {
        use snob_core::store::rate_budget::{RateBudget, RateBudgetError};

        #[derive(Default)]
        struct Counting(std::sync::atomic::AtomicUsize);
        impl RateBudget for Counting {
            fn reserve(&self) -> Result<Duration, RateBudgetError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(Duration::ZERO)
            }
            fn cooldown(&self) -> Result<Option<i64>, RateBudgetError> {
                Ok(None)
            }
            fn start_cooldown(&self, _: &str, _: Duration) -> Result<i64, RateBudgetError> {
                Ok(0)
            }
        }

        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ok(body(100, 20, None)),
        ])
        .await;

        let budget = std::sync::Arc::new(Counting::default());
        let client = client_with(&server, Pacer::new(budget.clone()));
        let walker = ListWalker::new(&client);
        let summary = walker
            .walk(request(), |p, _| Ok(p.users.len()), |_| {})
            .await
            .unwrap();

        assert_eq!(summary.pages, 3);
        assert_eq!(
            budget.0.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "one reservation per request, taken by the client itself"
        );
    }

    #[tokio::test]
    async fn repeats_do_not_inflate_the_count() {
        // The same account on two pages: the running total follows what the
        // saver reports as new, not what arrived.
        let server =
            server(vec![
            ok(r#"{"users":[{"pk":1,"username":"a"},{"pk":2,"username":"b"}],"next_max_id":"c1"}"#
                .to_string()),
            ok(r#"{"users":[{"pk":2,"username":"b"},{"pk":3,"username":"c"}]}"#.to_string()),
        ])
            .await;

        let (summary, _, seen) = walk(&server, request()).await;
        assert_eq!(summary.users, 3);
        assert_eq!(seen, vec![1, 2, 3]);
    }
}
