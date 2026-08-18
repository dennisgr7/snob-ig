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
use crate::Pk;

/// How long to wait before the first retry. Each attempt doubles it.
const FIRST_BACKOFF_SECS: i64 = 60;

/// The longest wait between attempts, so a queue that has been failing for a
/// day still tries roughly hourly rather than settling into a week.
const MAX_BACKOFF_SECS: i64 = 3_600;

/// How many attempts before a report is given up on.
///
/// With the backoff above, eight attempts span a little over five hours. Past
/// that the far end is not restarting, it is gone or it is refusing, and the
/// row is more use as a line in the log than as work that never finishes.
const MAX_ATTEMPTS: i64 = 8;

/// How old a report may get before it is given up on whatever its attempt
/// count says.
///
/// Both bounds exist because they catch different failures. The attempt count
/// catches an endpoint that answers quickly and wrongly; the age catches a
/// process that was stopped for a week, where nothing has been attempted at all
/// and a report about who unfollowed you last Tuesday is no longer news.
pub const MAX_AGE_SECS: i64 = 24 * 3_600;

/// A report waiting to be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub id: i64,
    pub run_id: String,
    /// The exact bytes to send, and the ones the signature covers.
    pub body: String,
    pub attempts: i64,
    pub created_at: i64,
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
        "SELECT id, run_id, body, attempts, created_at
         FROM watch_deliveries
         WHERE state = 'pending' AND next_try_at IS NOT NULL AND next_try_at <= ?1
           AND created_at >= ?3
           AND (destination IS NULL OR destination = ?4)
         ORDER BY created_at, id
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(
        params![now, limit as i64, now - MAX_AGE_SECS, destination],
        |row| {
            Ok(Delivery {
                id: row.get(0)?,
                run_id: row.get(1)?,
                body: row.get(2)?,
                attempts: row.get(3)?,
                created_at: row.get(4)?,
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
    } else if now - created_at >= MAX_AGE_SECS {
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

/// How many reports are waiting, for `snob watch status` to report.
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
            failed(db.conn(), id, Some(0), "not a header", true, 1_000).unwrap(),
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

    /// A queue that never drains is a queue that grows forever. This is the
    /// half of "retry" that the reasoning in the module header pays for.
    #[test]
    fn a_report_runs_out_of_attempts_rather_than_being_retried_forever() {
        let db = store();
        let id = queued(&db, "run-1", 1_000);

        let mut now = 1_000;
        for _ in 1..MAX_ATTEMPTS {
            let Outcome::Retrying(next) =
                failed(db.conn(), id, Some(500), "boom", false, now).unwrap()
            else {
                panic!("not out of attempts yet");
            };
            now = next;
        }

        assert_eq!(
            failed(db.conn(), id, Some(500), "boom", false, now).unwrap(),
            Outcome::GaveUp(GaveUp::OutOfAttempts)
        );
        assert!(due(db.conn(), now + 999_999, 10, HERE).unwrap().is_empty());
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
