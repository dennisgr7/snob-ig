//! The outbox: reports owed to the address the user chose.
//!
//! The rules live here rather than in whatever does the sending, because they
//! are the ones a caller would otherwise have to remember: a report is queued
//! before the mark that makes it old news moves, a retry sends the bytes that
//! were signed, the wait between attempts grows, and an attempt that will never
//! succeed is given up on rather than retried until the end of time.
//!
//! Retrying at all is not a contradiction of the project's hard-stop rule. That
//! rule is about **Instagram** — when a service that did not ask to be talked to
//! says no, the answer is to stop asking. This is the user's own server, which
//! they set up to be talked to, and a webhook that drops a report because n8n
//! was restarting is a webhook nobody can rely on.

use rusqlite::{Connection, OptionalExtension, params};

use super::{StoreError, pk_to_sql};
use snob_core::Pk;

/// How long to wait before the first retry. Each attempt doubles it.
const FIRST_BACKOFF_SECS: i64 = 60;

/// The longest wait between attempts, so a queue that has been failing for a
/// day still tries roughly hourly rather than settling into a week.
const MAX_BACKOFF_SECS: i64 = 3_600;

/// How many attempts before a report is given up on.
///
/// **The age below is the bound that decides; this one is the backstop.** It was
/// the other way round by arithmetic rather than by intent: this said eight
/// attempts span a little over five hours, and 60 + 120 + 240 + 480 + 960 +
/// 1920 + 3600 is 7 380 seconds — two hours and three minutes. So a receiver
/// that was down for an afternoon lost every report queued in it, and
/// `MAX_AGE_SECS`, whose whole doc is about catching what the attempt count
/// cannot, was unreachable: nothing survived long enough to be judged by it.
///
/// That matters more here than the number suggests. `commit_report` moves the
/// mark in the same transaction that queues the report, so a report given up on
/// is a set of arrivals and departures that no later run will find — the
/// standing rule is that a report is never lost because its delivery failed.
///
/// Thirty-two is what it takes for the age to win: seven doubling waits and
/// then hourly, which passes a day before this is reached. Every attempt waits,
/// so this cannot be burned through quickly by an endpoint that answers fast.
const MAX_ATTEMPTS: i64 = 32;

/// How old a report may get before it is given up on whatever its attempt
/// count says.
///
/// Both bounds exist because they catch different failures. The age catches a
/// process that was stopped for a week, where nothing has been attempted at all
/// and a report about who unfollowed you last Tuesday is no longer news; the
/// attempt count catches a queue that somehow keeps being tried without the
/// clock moving. In ordinary running it is the age that decides.
pub const MAX_AGE_SECS: i64 = 24 * 3_600;

/// A report waiting to be sent.
///
/// **`created_at` is a column and not a field.** It was read by nothing, and
/// that is not an omission: a caller holding it would compute a report's age and
/// decide something from it, which is [`MAX_AGE_SECS`]'s job and is decided in
/// one place. The mapper below reads by position, so a field nothing needs is
/// also one more position to keep in step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub id: i64,
    pub run_id: String,
    /// The exact bytes to send, and the ones the signature covers.
    pub body: String,
    pub attempts: i64,
}

/// Queues a report.
///
/// Takes a `&Connection` rather than opening its own transaction on purpose:
/// the caller has to be able to put this and the mark it makes stale in **one**
/// transaction, and a function that begins its own makes that impossible.
/// `destination` is where it is addressed, and it is what [`due`] filters on.
/// `None` when this run has no webhook at all, where the report is only ever
/// printed.
pub fn enqueue(
    conn: &Connection,
    run_id: &str,
    account_pk: Pk,
    body: &str,
    at: i64,
    destination: Option<&str>,
) -> Result<i64, StoreError> {
    conn.execute(
        "INSERT INTO watch_deliveries
            (run_id, account_pk, created_at, body, next_try_at, destination)
         VALUES (?1, ?2, ?3, ?4, ?3, ?5)",
        params![run_id, pk_to_sql(account_pk), at, body, destination],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The moment a report has to be newer than to still be news.
///
/// One function because the bound was three hand-written comparisons and they
/// disagreed at exactly a day: [`due`] handed the report out as news, [`failed`]
/// gave up on it, and `store::watch::prune` left it `pending` for ever. A report
/// either is news or is not, and one of the three readings had to be it.
///
/// `failed`'s is the one kept, because
/// `a_report_too_old_to_be_news_is_given_up_on` pins it: a report is too old the
/// moment it *reaches* [`MAX_AGE_SECS`], and everything strictly newer is still
/// news.
fn still_news_after(now: i64) -> i64 {
    now - MAX_AGE_SECS
}

/// Gives up on reports that have grown too old to be news.
///
/// **[`MAX_AGE_SECS`] cannot be left to [`failed`] alone**, which is only
/// reached by a run that *tries* the report — so a report nothing ever retried
/// aged without limit. Three ways that showed: a year-old row was still `due`
/// and went out as news on the next run; removing `[webhook]` from the
/// configuration left rows owed forever with `status` promising the next run
/// would try them; and a walk that failed before the delivery step stopped the
/// queue draining even when the webhook was fine.
///
/// Marked rather than deleted, so `status` can still say what became of it.
///
/// Here rather than written out in `store::watch::prune`, which is where it was.
/// The outbox's rules live in this file — the header says so — and that
/// statement out there is how the age bound came to be spelled a third time, in
/// a third direction, with nothing to compare the three against.
pub(super) fn expire_stale(conn: &Connection, now: i64) -> Result<usize, StoreError> {
    let expired = conn.execute(
        "UPDATE watch_deliveries
         SET state = 'expired', settled_at = ?1,
             next_try_at = NULL,
             last_error = coalesce(last_error, 'it grew too old to be news')
         WHERE state = 'pending' AND created_at <= ?2",
        params![now, still_news_after(now)],
    )?;
    Ok(expired)
}

/// How long a settled delivery is kept, for `status` and for anybody wondering
/// where a report went. **Seven days**; older ones are only a record that
/// something arrived.
pub const KEEP_SETTLED_FOR_SECS: i64 = 7 * 24 * 3_600;

/// Forgets deliveries settled long enough ago to be history rather than work.
///
/// **A pending row is never deleted here.** [`failed`] and [`expire_stale`] are
/// what decide when a report stops being owed, and removing one from under them
/// would lose a report that was still going to be tried.
pub(super) fn forget_settled(conn: &Connection, now: i64) -> Result<usize, StoreError> {
    let removed = conn.execute(
        "DELETE FROM watch_deliveries
         WHERE state != 'pending' AND settled_at IS NOT NULL AND settled_at < ?1",
        params![now - KEEP_SETTLED_FOR_SECS],
    )?;
    Ok(removed)
}

/// Reports that may be tried now, oldest first.
///
/// Oldest first because they are a sequence of events about one account, and a
/// receiver that gets Tuesday's arrivals after Wednesday's has to sort them out
/// itself.
/// The age bound is applied here as well as in [`failed`], and that is not
/// belt-and-braces: `failed` is only reached by a run that *tries* the report,
/// so a report nothing retried — because the process was stopped, or the
/// webhook was removed from the configuration — aged without limit and then
/// went out as news. A row past the bound is simply not due; `store::watch::prune`
/// is what settles it.
///
/// `destination` is the address the caller can post to, and only reports
/// addressed there come back. There was no such predicate and no such column, so
/// the queue was drained through whichever client this invocation happened to
/// build: `snob watch once --webhook https://webhook.site/<id>`, run to see what
/// the payload looks like, sent the reports queued for the team's receiver to the
/// request bin with the team's token on them, and marked them delivered. Leaked,
/// and lost for the address they were made for.
///
/// **The whole address, not its origin.** This is a URL and it is compared as
/// one. An origin cannot tell two workflows on one host apart, and that is the
/// shape the tool is most often pointed at: n8n publishes a workflow at
/// `/webhook/name` and the same workflow's test run at `/webhook-test/name`, so
/// the two differ in the path alone. Filtering by origin sent the production
/// backlog to the test workflow, with the production token on it, and marked it
/// delivered — the very failure described above, one URL level up.
///
/// A row with no destination was queued before the column existed. It matches
/// whatever is asked — the old behavior, kept for those rows rather than a guess
/// about where they belong.
pub fn due(
    conn: &Connection,
    now: i64,
    limit: usize,
    destination: &str,
) -> Result<Vec<Delivery>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, run_id, body, attempts
         FROM watch_deliveries
         WHERE state = 'pending' AND next_try_at IS NOT NULL AND next_try_at <= ?1
           AND created_at > ?3
           AND (destination IS NULL OR destination = ?4)
         ORDER BY created_at, id
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(
        params![now, limit as i64, still_news_after(now), destination],
        |row| {
            Ok(Delivery {
                id: row.get(0)?,
                run_id: row.get(1)?,
                body: row.get(2)?,
                attempts: row.get(3)?,
            })
        },
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Records that a report arrived.
pub fn delivered(conn: &Connection, id: i64, status: u16, at: i64) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE watch_deliveries
         SET state = 'delivered', settled_at = ?2, next_try_at = NULL,
             attempts = attempts + 1, last_status = ?3, last_error = NULL
         WHERE id = ?1",
        params![id, at, status as i64],
    )?;
    Ok(())
}

/// What happened to a report that did not arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It will be tried again, at this moment.
    Retrying(i64),
    /// It will not. Either it was refused in a way waiting cannot fix, or it
    /// ran out of attempts, or it got too old to be news.
    GaveUp(GaveUp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GaveUp {
    /// There was no request to send, so there is none to send again: a header
    /// the builder cannot construct from what was configured.
    ///
    /// **Not "the far end said no."** It used to mean that, and it cost changes:
    /// the mark has already moved by the time a delivery fails, so a 4xx here
    /// expired the report after zero attempts and what it described was never
    /// reported by anything. A 401 or a 404 from a webhook is very often
    /// transient, and the far end is the user's own server.
    Refused,
    OutOfAttempts,
    TooOld,
}

/// Records a failed attempt and decides what happens next.
///
/// `permanent` is the caller's reading of the attempt — the store does not know
/// what an HTTP status means — and it separates "there was no request to send"
/// from everything else. It is deliberately narrow: it used to mean "the far end
/// answered 4xx", and because the mark has already moved by the time this runs,
/// that expired the report after zero retries and the window it described was
/// never reported by anything. What bounds the retrying is [`MAX_ATTEMPTS`] and
/// [`MAX_AGE_SECS`], not a guess about a status code.
pub fn failed(
    conn: &Connection,
    id: i64,
    status: Option<u16>,
    error: &str,
    permanent: bool,
    now: i64,
) -> Result<Outcome, StoreError> {
    let (attempts, created_at): (i64, i64) = conn.query_row(
        "SELECT attempts, created_at FROM watch_deliveries WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let attempts = attempts + 1;

    let gave_up = if permanent {
        Some(GaveUp::Refused)
    } else if attempts >= MAX_ATTEMPTS {
        Some(GaveUp::OutOfAttempts)
    } else if created_at <= still_news_after(now) {
        Some(GaveUp::TooOld)
    } else {
        None
    };

    match gave_up {
        Some(reason) => {
            conn.execute(
                "UPDATE watch_deliveries
                 SET state = 'expired', settled_at = ?2, next_try_at = NULL,
                     attempts = ?3, last_status = ?4, last_error = ?5
                 WHERE id = ?1",
                params![id, now, attempts, status.map(i64::from), error],
            )?;
            Ok(Outcome::GaveUp(reason))
        }
        None => {
            let next = now + backoff(attempts);
            conn.execute(
                "UPDATE watch_deliveries
                 SET attempts = ?2, next_try_at = ?3, last_status = ?4, last_error = ?5
                 WHERE id = ?1",
                params![id, attempts, next, status.map(i64::from), error],
            )?;
            Ok(Outcome::Retrying(next))
        }
    }
}

/// How long to wait before attempt number `attempts + 1`.
///
/// Doubling, capped. `checked_shl` rather than `<<`: eight attempts cannot
/// overflow, but a shift by a number this function does not control is the kind
/// of thing that stops being true when somebody raises the cap.
fn backoff(attempts: i64) -> i64 {
    let shift = (attempts.max(1) - 1).min(16) as u32;
    FIRST_BACKOFF_SECS
        .checked_shl(shift)
        .unwrap_or(MAX_BACKOFF_SECS)
        .min(MAX_BACKOFF_SECS)
}

/// A count of the queue, split by whether this configuration could post it.
///
/// One integer could not answer both questions, and two readers asked it the
/// wrong one. [`pending`] counts every row in the state — no destination, no age
/// bound — while [`due`] hands back only what is addressed here: so after
/// `[webhook] url` moved from one receiver to another, `status` printed "2
/// reports are waiting to be delivered; the next run tries them" about rows
/// `due` can never return, and the health verdict failed the monitor over them.
/// Nothing is lost by refusing an old-destination row — that is the settled
/// rule, and `store::watch::prune` expires it at a day — but a probe told the
/// wrong number in the alarming direction is one people switch off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Owed {
    /// Reports this destination could post: addressed here, or carrying no
    /// address at all because they were queued before the column existed.
    pub waiting: usize,
    /// Reports addressed elsewhere. Not owed to this run, and unless something
    /// else posts to that address they expire where they are — which is worth
    /// saying separately rather than counting in with the rest.
    pub elsewhere: usize,
    /// Reports nothing will ever deliver, because they grew too old to be news.
    ///
    /// Not owed — the opposite: owing has ended. It is here because `status` is
    /// where somebody goes to ask what became of a report, and this is the one
    /// answer it could not give. Kept for `KEEP_SETTLED_FOR_SECS`, which is how
    /// long the row survives `forget_settled`.
    pub given_up: usize,
}

/// What is queued, for `snob watch status` to report.
///
/// `destination` is the address the caller could post to, in the spelling
/// [`enqueue`] recorded — `commands::watch::destination_of`, for a caller that
/// builds no delivery of its own. `None` is a configuration that names no
/// address: it can post nothing, so nothing is owed to *it*, whatever the rows
/// say. That is deliberately not the same as "orphaned". A run given `--webhook`
/// on the command line — which is how the README's own systemd example runs —
/// queues rows this file knows nothing about and drains them again on its next
/// tick, and nothing here can tell that apart from a `[webhook]` somebody
/// deleted. Whoever reads this decides what a guess is worth; this only counts.
///
/// The age bound `due` also applies is deliberately not repeated. A row past it
/// is expired by `prune` on the next settle rather than left `pending`, so
/// filtering it out here would print 0 while rows sit in the table — the same
/// failure this exists to fix, in the other direction.
pub fn owed(conn: &Connection, destination: Option<&str>) -> Result<Owed, StoreError> {
    // The third count reads a **settled** row, which is why the `state` test
    // moved out of the `WHERE` and into the filters. Counting only `pending`
    // meant the one outcome worth telling somebody about was the one nothing
    // could see: a report given up on left `pending` and became invisible in
    // the same statement, so `status` went quiet and the health verdict went
    // from `warning` to `ok` at exactly the moment the change was thrown away.
    // `forget_settled` takes the row after `KEEP_SETTLED_FOR_SECS`, which is
    // the window this has to say it in.
    let (waiting, elsewhere, given_up): (i64, i64, i64) = conn.query_row(
        "SELECT
           count(*) FILTER (WHERE state = 'pending'
             AND ?1 IS NOT NULL AND (destination IS NULL OR destination = ?1)),
           count(*) FILTER (WHERE state = 'pending'
             AND (?1 IS NULL OR (destination IS NOT NULL AND destination <> ?1))),
           count(*) FILTER (WHERE state = 'expired')
         FROM watch_deliveries",
        params![destination],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(Owed {
        waiting: waiting as usize,
        elsewhere: elsewhere as usize,
        given_up: given_up as usize,
    })
}

/// The whole queue, whoever it belongs to.
///
/// Kept for the tests that assert a row was settled. Anything reported to a
/// person goes through [`owed`], which asks `due`'s question rather than this
/// one.
pub fn pending(conn: &Connection) -> Result<usize, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM watch_deliveries WHERE state = 'pending'",
        [],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}

/// The state of one report, for tests and for `status`.
pub fn state(conn: &Connection, id: i64) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT state FROM watch_deliveries WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, accounts, users};

    const ME: Pk = 42;
    /// The address these reports are addressed to.
    const HERE: &str = "https://receiver.example";

    fn store() -> Store {
        let db = Store::in_memory().unwrap();
        users::ensure(db.conn(), ME).unwrap();
        accounts::upsert(db.conn(), ME, true).unwrap();
        db
    }

    /// How old a report may get before it stops being news, written out.
    ///
    /// Every reference to it in the tree is relative — a fixture ages a row by
    /// `MAX_AGE_SECS + 1` and asserts it is no longer due — so the constant can
    /// be changed to anything at all and the suite still passes, while reports
    /// are either given up on within the hour or kept for a year.
    #[test]
    fn the_age_a_report_stops_being_news_at_is_the_documented_one() {
        assert_eq!(MAX_AGE_SECS, 24 * 3_600);
    }

    fn queued(db: &Store, run_id: &str, at: i64) -> i64 {
        enqueue(db.conn(), run_id, ME, r#"{"a":1}"#, at, Some(HERE)).unwrap()
    }

    /// The property the signature depends on. A retry sends the string that was
    /// signed, not a fresh rendering of the events.
    #[test]
    fn the_body_that_comes_back_is_the_body_that_went_in() {
        let db = store();
        let body = r#"{"schema":1,"events":{"followers_gained":[]}}"#;
        enqueue(db.conn(), "run-1", ME, body, 1_000, Some(HERE)).unwrap();

        let waiting = due(db.conn(), 1_000, 10, HERE).unwrap();
        assert_eq!(waiting[0].body, body);
    }

    #[test]
    fn a_queued_report_is_due_at_once() {
        let db = store();
        queued(&db, "run-1", 1_000);
        assert_eq!(due(db.conn(), 1_000, 10, HERE).unwrap().len(), 1);
        assert_eq!(pending(db.conn()).unwrap(), 1);
    }

    #[test]
    fn a_delivered_report_is_not_due_again() {
        let db = store();
        let id = queued(&db, "run-1", 1_000);

        delivered(db.conn(), id, 200, 1_010).unwrap();
        assert!(due(db.conn(), 2_000, 10, HERE).unwrap().is_empty());
        assert_eq!(state(db.conn(), id).unwrap().unwrap(), "delivered");
        assert_eq!(pending(db.conn()).unwrap(), 0);
    }

    /// A failure that waiting can fix is waited on, and it is not tried again
    /// in the meantime.
    #[test]
    fn a_temporary_failure_is_retried_later_and_not_sooner() {
        let db = store();
        let id = queued(&db, "run-1", 1_000);

        let Outcome::Retrying(next) =
            failed(db.conn(), id, Some(503), "busy", false, 1_000).unwrap()
        else {
            panic!("a 503 is worth another try");
        };
        assert_eq!(next, 1_000 + FIRST_BACKOFF_SECS);
        assert!(due(db.conn(), next - 1, 10, HERE).unwrap().is_empty());
        assert_eq!(due(db.conn(), next, 10, HERE).unwrap().len(), 1);
    }

    /// A request that could not be sent at all is not sent again.
    ///
    /// `permanent` used to mean "the far end answered 4xx", and that lost
    /// changes: the mark has moved by the time this is called, so a single 404
    /// from an n8n workflow that was not registered expired the row after zero
    /// retries and the arrivals it described were gone. It means "there is no
    /// request to retry" now — a header the builder cannot construct — which is
    /// the only failure retrying genuinely cannot change.
    #[test]
    fn a_request_that_cannot_be_sent_is_not_retried_at_all() {
        let db = store();
        let id = queued(&db, "run-1", 1_000);

        assert_eq!(
            // `None`, not `Some(0)`: `Attempt::Refused` carries no status
            // because no server answered, and 0 is not a code one can send.
            failed(db.conn(), id, None, "not a header", true, 1_000).unwrap(),
            Outcome::GaveUp(GaveUp::Refused)
        );
        assert!(due(db.conn(), 999_999, 10, HERE).unwrap().is_empty());
        assert_eq!(state(db.conn(), id).unwrap().unwrap(), "expired");
    }

    /// The wait grows, and it stops growing. Without the cap, the eighth
    /// attempt would be hours out and the queue would look stuck.
    #[test]
    fn the_wait_grows_and_then_stops_growing() {
        let waits: Vec<i64> = (1..=6).map(backoff).collect();
        assert_eq!(waits, vec![60, 120, 240, 480, 960, 1_920]);
        assert_eq!(backoff(20), MAX_BACKOFF_SECS, "and it is capped");
        assert!(backoff(i64::MAX) > 0, "a shift that big must not wrap");
    }

    /// A receiver that is down for an afternoon does not cost the reports queued
    /// in it.
    ///
    /// This counted iterations, so it passed at any ladder length — and the
    /// ladder was 60 + 120 + 240 + 480 + 960 + 1920 + 3600 = 7 380 seconds, two
    /// hours and three minutes, under a doc claiming five and under an age bound
    /// of a day that nothing could ever live long enough to reach.
    ///
    /// It matters more than the number suggests: `commit_report` moves the mark
    /// in the transaction that queues the report, so one given up on is a set of
    /// arrivals and departures no later run will find.
    #[test]
    fn a_receiver_down_for_hours_still_has_its_report_retried() {
        let db = store();
        let queued_at = 1_000;
        let id = queued(&db, "run-1", queued_at);

        let mut now = queued_at;
        loop {
            match failed(db.conn(), id, Some(503), "restarting", false, now).unwrap() {
                Outcome::Retrying(next) => now = next,
                Outcome::GaveUp(reason) => {
                    assert_eq!(
                        reason,
                        GaveUp::TooOld,
                        "the age is what decides in ordinary running"
                    );
                    break;
                }
            }
        }

        assert!(
            now - queued_at >= MAX_AGE_SECS,
            "it gave up after {}s, and a day is what the age bound names",
            now - queued_at
        );
        assert!(due(db.conn(), now + 999_999, 10, HERE).unwrap().is_empty());
    }

    /// And the attempt count is still there, for a queue that is somehow tried
    /// without the clock moving. A queue that never drains is a queue that grows
    /// forever.
    #[test]
    fn the_attempt_count_is_the_backstop_when_the_clock_does_not_move() {
        let db = store();
        let id = queued(&db, "run-1", 1_000);

        for _ in 1..MAX_ATTEMPTS {
            assert!(
                matches!(
                    failed(db.conn(), id, Some(500), "boom", false, 1_000).unwrap(),
                    Outcome::Retrying(_)
                ),
                "not out of attempts yet"
            );
        }

        assert_eq!(
            failed(db.conn(), id, Some(500), "boom", false, 1_000).unwrap(),
            Outcome::GaveUp(GaveUp::OutOfAttempts)
        );
    }

    /// The other bound, and it catches a different failure: a process stopped
    /// for a week has attempted nothing, and news about last Tuesday is not
    /// news.
    #[test]
    fn a_report_too_old_to_be_news_is_given_up_on() {
        let db = store();
        let id = queued(&db, "run-1", 1_000);

        assert_eq!(
            failed(
                db.conn(),
                id,
                None,
                "connection refused",
                false,
                1_000 + MAX_AGE_SECS
            )
            .unwrap(),
            Outcome::GaveUp(GaveUp::TooOld)
        );
    }

    /// They are a sequence of events about one account, so they go out in the
    /// order they happened.
    #[test]
    fn reports_come_out_oldest_first() {
        let db = store();
        queued(&db, "later", 2_000);
        queued(&db, "earlier", 1_000);

        let order: Vec<String> = due(db.conn(), 3_000, 10, HERE)
            .unwrap()
            .into_iter()
            .map(|d| d.run_id)
            .collect();
        assert_eq!(order, vec!["earlier", "later"]);
    }

    /// A report belongs to the address it was addressed to.
    ///
    /// The table recorded no destination and `due` had no predicate, so the queue
    /// was drained through whichever client the current invocation built. Running
    /// `snob watch once --webhook https://webhook.site/<id>` to see what the
    /// payload looks like therefore sent the reports queued for the team's
    /// receiver to the request bin, with the team's token on them, and marked them
    /// delivered.
    #[test]
    fn a_report_is_only_due_at_the_address_it_was_addressed_to() {
        let db = store();
        let mine = enqueue(db.conn(), "run-1", ME, "{}", 1_000, Some(HERE)).unwrap();
        let elsewhere = enqueue(
            db.conn(),
            "run-2",
            ME,
            "{}",
            1_000,
            Some("https://bin.example"),
        )
        .unwrap();
        // Queued before the column existed, so nothing can say where it belongs.
        let legacy = enqueue(db.conn(), "run-3", ME, "{}", 1_000, None).unwrap();

        let here: Vec<i64> = due(db.conn(), 1_000, 10, HERE)
            .unwrap()
            .iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(here, vec![mine, legacy]);

        let there: Vec<i64> = due(db.conn(), 1_000, 10, "https://bin.example")
            .unwrap()
            .iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(there, vec![elsewhere, legacy]);
    }

    /// Two workflows on one host are two addresses.
    ///
    /// The test above uses two different *hosts*, which is what let the filter
    /// pass while it compared origins: an origin tells `bin.example` from
    /// `receiver.example` and cannot tell `/webhook/snob` from
    /// `/webhook-test/snob`. That pair is not a corner case, it is how n8n
    /// publishes a workflow and its test run — so `snob watch once --webhook
    /// https://n8n.local/webhook-test/snob`, typed to see what the payload
    /// looks like, drained the production backlog into the test workflow with
    /// the production token on it and marked it delivered.
    #[test]
    fn a_report_is_not_drained_to_a_different_path_on_the_same_host() {
        const PUBLISHED: &str = "https://n8n.local/webhook/snob";
        const TEST_RUN: &str = "https://n8n.local/webhook-test/snob";

        let db = store();
        let owed = enqueue(db.conn(), "run-1", ME, "{}", 1_000, Some(PUBLISHED)).unwrap();

        let to_the_test_run: Vec<i64> = due(db.conn(), 1_000, 10, TEST_RUN)
            .unwrap()
            .iter()
            .map(|d| d.id)
            .collect();
        assert!(
            to_the_test_run.is_empty(),
            "a report addressed to {PUBLISHED} was drained to {TEST_RUN}"
        );

        let to_where_it_belongs: Vec<i64> = due(db.conn(), 1_000, 10, PUBLISHED)
            .unwrap()
            .iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(
            to_where_it_belongs,
            vec![owed],
            "and it is still owed at the address it was addressed to"
        );
    }

    /// What a person is told is owed is what a run could actually post.
    ///
    /// `pending` counts every pending row and two readers took it for `due`'s
    /// answer. Move `[webhook] url` from one receiver to another with reports
    /// queued and `snob watch status` says "2 reports are waiting to be
    /// delivered; the next run tries them" about a row `due` can never return,
    /// while the health verdict fails the monitor over it. Nothing is lost by
    /// refusing the old row -- that is the settled rule, and `prune` expires it
    /// at a day -- but a probe told the wrong number in the alarming direction
    /// is one people switch off, and then the right number never reaches anybody
    /// either.
    #[test]
    fn what_is_owed_is_split_from_what_is_addressed_elsewhere() {
        let db = store();
        enqueue(db.conn(), "here", ME, "{}", 1_000, Some(HERE)).unwrap();
        enqueue(
            db.conn(),
            "there",
            ME,
            "{}",
            1_000,
            Some("https://bin.example"),
        )
        .unwrap();
        // Queued before the column existed, so it belongs to whoever asks.
        enqueue(db.conn(), "legacy", ME, "{}", 1_000, None).unwrap();

        let here = owed(db.conn(), Some(HERE)).unwrap();
        assert_eq!(
            here,
            Owed {
                waiting: 2,
                elsewhere: 1,
                given_up: 0
            },
            "the row for {HERE} and the one with no address are owed here; the other is not"
        );
        assert_eq!(
            due(db.conn(), 1_000, 10, HERE).unwrap().len(),
            here.waiting,
            "what is reported as owed has to be what a run would be handed"
        );

        // A configuration naming no address can post nothing at all, so nothing
        // is owed to it -- and it cannot tell a row queued by a `--webhook` run
        // from one whose `[webhook]` was deleted, which is why that is a
        // separate number rather than a verdict.
        assert_eq!(
            owed(db.conn(), None).unwrap(),
            Owed {
                waiting: 0,
                elsewhere: 3,
                given_up: 0
            }
        );

        // However it is split, nothing falls out of the total.
        assert_eq!(here.waiting + here.elsewhere, pending(db.conn()).unwrap());
    }

    /// A report nothing will ever deliver has to be visible for as long as the
    /// row survives.
    ///
    /// `owed` counted `state = 'pending'` alone, so a row stopped being counted
    /// at the instant it stopped being deliverable: `status` went quiet and the
    /// health verdict went from `warning` to `ok` at exactly the moment the
    /// change was thrown away. `expire_stale` marks rather than deletes for
    /// this, and until now nothing read the mark.
    #[test]
    fn a_report_given_up_on_is_still_counted_while_the_row_is_kept() {
        let db = store();
        enqueue(db.conn(), "old", ME, "{}", 1_000, Some(HERE)).unwrap();

        let before = owed(db.conn(), Some(HERE)).unwrap();
        assert_eq!(before.waiting, 1);
        assert_eq!(before.given_up, 0);

        // Far enough past the age bound that it is no longer news.
        let much_later = 1_000 + MAX_AGE_SECS + 1;
        assert_eq!(expire_stale(db.conn(), much_later).unwrap(), 1);

        let after = owed(db.conn(), Some(HERE)).unwrap();
        assert_eq!(after.waiting, 0, "it is not owed any more");
        assert_eq!(
            after.given_up, 1,
            "and that is exactly when somebody has to be told"
        );
    }

    /// A day old is one answer, not three.
    ///
    /// The bound was written out three times -- here, in `failed`, and in
    /// `store::watch::prune` -- and at exactly `MAX_AGE_SECS` the three
    /// disagreed: `due` handed the report out as news, `failed` gave up on it,
    /// and `prune` left it pending for ever, where nothing could reach it again.
    /// Whichever reading is right, one of them has to be it.
    #[test]
    fn a_report_exactly_a_day_old_is_the_same_answer_to_everyone() {
        let queued_at = 1_000;
        let now = queued_at + MAX_AGE_SECS;

        let db = store();
        let id = queued(&db, "run-1", queued_at);
        assert!(
            due(db.conn(), now, 10, HERE).unwrap().is_empty(),
            "a report the age bound has caught must not go out as news"
        );
        assert_eq!(
            failed(db.conn(), id, None, "connection refused", false, now).unwrap(),
            Outcome::GaveUp(GaveUp::TooOld),
            "and the run that tries it gives up on it"
        );

        // And the sweep that runs whether or not anything tried it agrees, which
        // is the reading that was the odd one out.
        let db = store();
        let swept = queued(&db, "run-2", queued_at);
        expire_stale(db.conn(), now).unwrap();
        assert_eq!(
            state(db.conn(), swept).unwrap().as_deref(),
            Some("expired"),
            "a report nothing ever retried is expired at the same age"
        );
    }

    /// And how long the record of a delivered one is kept, for the same reason
    /// the age bound is written down: `prune`'s fixture ages a row by a multiple
    /// of this, so it agreed with anything.
    #[test]
    fn how_long_a_settled_report_is_kept_is_the_documented_one() {
        assert_eq!(KEEP_SETTLED_FOR_SECS, 7 * 24 * 3_600);
    }

    /// The id the receiver deduplicates on has to be unique, or two runs could
    /// hand it the same one and it would drop a real report.
    #[test]
    fn two_reports_cannot_share_a_run_id() {
        let db = store();
        queued(&db, "run-1", 1_000);
        assert!(
            enqueue(db.conn(), "run-1", ME, "{}", 2_000, None).is_err(),
            "the id a receiver deduplicates on must be unique"
        );
    }
}
