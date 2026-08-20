//! Snapshots: a follower or following list captured at one moment.
//!
//! A snapshot is opened when the walk begins, filled in page by page, and
//! closed at the end. Each page commits in its own transaction, and that is
//! what makes "save partial progress" not an action to be executed in time but
//! simply a matter of stopping: whatever committed is durable even if the
//! process dies outright.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{Store, StoreError, now, now_ms, pk_from_sql, pk_to_sql};
use crate::Pk;
use crate::model::{ListKind, StopReason, User};

/// How long an interrupted walk may still be resumed.
///
/// Past the window it is not resumed: stitching two separate moments together
/// produces a list that reflects no single instant, and comparing it against
/// another invents arrivals and departures that never happened.
pub const RESUME_WINDOW_SECS: i64 = 15 * 60;

/// How long a claim on a walk outlives the last page it saved.
///
/// A different question from [`RESUME_WINDOW_SECS`], and deliberately its own
/// number: that one asks whether a partial still describes one moment, this one
/// asks whether anybody is still working on it. A walk that keeps saving pages
/// keeps its claim however long the list is, because `save_page` refreshes it;
/// what this bounds is how long a walk whose process was killed goes on looking
/// busy.
///
/// **It has to be strictly shorter than [`RESUME_WINDOW_SECS`], and by enough
/// to leave a gap somebody can use.** `save_page` refreshes `claimed_at`, so it
/// is never earlier than `started_at` — which means that with the two constants
/// equal, "the claim has gone stale" (`claimed_at + CLAIM_TTL_SECS < now`) and
/// "the partial is still worth resuming" (`started_at + RESUME_WINDOW_SECS >=
/// now`) cannot both hold. They were equal, and so the window in which another
/// process could adopt a walk whose own process was killed was empty: every
/// interrupted walk of every list started again from page one, at the full cost
/// of the requests it had already paid for.
///
/// Five minutes rather than something tighter because the pacer can legitimately
/// be quiet for a while: `Pace::third_party` waits up to thirty seconds between
/// pages, and the request budget can ration on top of that. And rather than
/// something looser because losing a claim is no longer a way to corrupt a
/// capture: `save_page` refuses to write into a snapshot this process does not
/// hold, so a walk whose claim was taken stops instead of interleaving with the
/// walk that took it.
pub const CLAIM_TTL_SECS: i64 = 5 * 60;

/// The ordering above, enforced where it cannot be argued with: a build in which
/// a killed walk can never be adopted does not compile.
const _: () = assert!(CLAIM_TTL_SECS < RESUME_WINDOW_SECS);

/// Who this process is, for the length of this process.
///
/// The pid and the moment it was first asked. Neither alone is enough — pids are
/// reused, and two processes can start in the same second — but a process that
/// has the same pid as an earlier one necessarily started later, so the pair is
/// unique among the processes that can be running at once. That is all a claim
/// needs: it is not an identity, it is "not me".
pub fn this_process() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| format!("{}-{}", std::process::id(), now_ms()))
}

/// A capture, as much of it as anything reads.
///
/// **`complete`, `requests` and `resumes` are columns and not fields.** They
/// were hydrated on every lookup and read by nobody, and `complete` was the
/// dangerous one: it is constant per construction path — `latest_complete` and
/// `find_usable` both select from `usable_snapshots`, and `resumable` selects
/// `complete = 0` — so `if snapshot.complete` read as the store's central
/// safety check while deciding nothing at all. The check is the view, and it is
/// held there rather than by whoever remembers to ask.
///
/// The columns stay: `pages` and `requests` are what a walk cost, and the next
/// thing that wants to say so should find them written down rather than lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: i64,
    pub account_pk: Pk,
    pub kind: ListKind,
    pub started_at: i64,
    pub taken_at: Option<i64>,
    pub member_count: u64,
    pub declared_count: Option<u64>,
    pub pages: u32,
    pub next_cursor: Option<String>,
}

/// The columns [`row_to_snapshot`] reads, in the order it reads them.
///
/// It reads **by position**, and three statements spelled the list out
/// separately with nothing tying them to it or to each other. `source` and
/// `stopped_by` are already columns the projection leaves out, so the next
/// field to be added is one somebody adds here and forgets there — and a miss
/// is not a compile error but an `InvalidColumnIndex` at runtime, on whichever
/// of the three paths was missed. Two of the three are ordinary lookups and the
/// third is the resume path, which only a walk that was interrupted ever takes.
const SNAPSHOT_COLUMNS: &str = "id, account_pk, kind, started_at, taken_at, member_count,                                 declared_count, pages, next_cursor";

/// What changed when a page was saved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedPage {
    /// How many users were not already in the snapshot. Instagram's pagination
    /// repeats accounts across pages, so this rarely matches what arrived.
    pub added: usize,
    /// Accounts that have renamed themselves since we last saw them.
    pub renamed: Vec<(Pk, String)>,
}

/// The snapshot a walk has just opened: its id, and the moment it opened.
///
/// A struct rather than a pair because the caller needs both and they are not
/// interchangeable. The timestamp is handed back rather than left in the row
/// because it is one end of the interval the finished list describes, and the
/// caller has to have the number the row actually holds — not one it read a
/// moment before or after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened {
    pub id: i64,
    pub started_at: i64,
}

/// Starts a new snapshot, claimed by this process.
pub fn begin(
    conn: &Connection,
    account_pk: Pk,
    kind: ListKind,
    declared_count: Option<u64>,
) -> Result<Opened, StoreError> {
    let started_at = now();
    conn.execute(
        "INSERT INTO snapshots
            (account_pk, kind, source, started_at, declared_count, claimed_by, claimed_at)
         VALUES (?1, ?2, 'live', ?3, ?4, ?5, ?3)",
        params![
            pk_to_sql(account_pk),
            kind.as_str(),
            started_at,
            declared_count.map(|v| v as i64),
            this_process(),
        ],
    )?;
    Ok(Opened {
        id: conn.last_insert_rowid(),
        started_at,
    })
}

/// Looks for an interrupted walk this process may continue, and takes it.
///
/// **The claim is taken in the same statement that finds the row**, which is
/// what makes it safe between processes: `UPDATE … WHERE` is atomic under
/// SQLite's write lock, so two processes racing for one partial cannot both
/// win. Reading first and claiming afterwards would leave exactly the window
/// this exists to close.
///
/// A partial is available when nobody holds it, when this process already does
/// — resuming its own work after a restart within the window — or when whoever
/// held it has not saved a page for [`CLAIM_TTL_SECS`] and is taken to be gone.
pub fn resumable(
    conn: &Connection,
    account_pk: Pk,
    kind: ListKind,
) -> Result<Option<Snapshot>, StoreError> {
    let now = now();

    // `RETURNING`, so what comes back is **the row that was claimed** rather
    // than the answer to a second, looser question.
    //
    // The read-back used to be its own statement, keyed on
    // `account_pk`/`kind`/`complete = 0`/`claimed_by` and ordered `started_at
    // DESC` — dropping both of the predicates that make a partial resumable at
    // all: `next_cursor IS NOT NULL` and the resume window. So a claimed
    // cursor-less row left behind by this process — `walk::fetch` returns on a
    // save or budget failure without calling `close` — was newer than the
    // partial actually claimed, and came back instead of it. What followed was
    // `mark_resumed` against the wrong id, a walk restarted from page one under
    // the wrong `started_at`, and the real partial abandoned still holding this
    // process's claim.
    let snapshot = conn
        .query_row(
            &format!(
                "UPDATE snapshots
                 SET claimed_by = ?4, claimed_at = ?5
                 WHERE id = (
                   SELECT id FROM snapshots
                   WHERE account_pk = ?1 AND kind = ?2 AND complete = 0
                     AND next_cursor IS NOT NULL AND started_at >= ?3
                     AND (claimed_by IS NULL OR claimed_by = ?4
                          OR claimed_at IS NULL OR claimed_at < ?6)
                   ORDER BY started_at DESC LIMIT 1
                 )
                 RETURNING {SNAPSHOT_COLUMNS}"
            ),
            params![
                pk_to_sql(account_pk),
                kind.as_str(),
                now - RESUME_WINDOW_SECS,
                this_process(),
                now,
                now - CLAIM_TTL_SECS,
            ],
            row_to_snapshot,
        )
        .optional()?;
    Ok(snapshot)
}

/// Whether an interrupted walk could be continued — asked, not taken.
///
/// The same predicate as [`resumable`] and deliberately not the same statement.
/// `engine::walk` asks this after closing its own snapshot, to choose between
/// "run it again to continue where it left off" and "run it again to start
/// over"; asking it with [`resumable`] meant the answer claimed the row on the
/// way past, so the process that had just released the claim took it back as it
/// exited. The next invocation is a different process, and it found a partial
/// with a claim that could not go stale before the resume window closed:
/// [`resumable`] refused it and [`delete_partials`] spared it. Every interrupted
/// walk of `followers`, `following`, `scan`, `unfollowers`, `fans` and `friends`
/// began again at page one while the advice on screen promised the opposite.
///
/// Availability is judged the way the **next** process will judge it, so this
/// does not count "claimed by me" as available: the run that continues the walk
/// is never this one.
pub fn is_resumable(conn: &Connection, account_pk: Pk, kind: ListKind) -> Result<bool, StoreError> {
    let now = now();
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM snapshots
             WHERE account_pk = ?1 AND kind = ?2 AND complete = 0
               AND next_cursor IS NOT NULL AND started_at >= ?3
               AND (claimed_by IS NULL OR claimed_at IS NULL OR claimed_at < ?4)
             LIMIT 1",
            params![
                pk_to_sql(account_pk),
                kind.as_str(),
                now - RESUME_WINDOW_SECS,
                now - CLAIM_TTL_SECS,
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Records that an existing snapshot is being continued.
pub fn mark_resumed(conn: &Connection, id: i64) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE snapshots SET resumes = resumes + 1 WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Saves a whole page in a single transaction: the users, their membership in
/// the snapshot, and the cursor advance.
///
/// Committing here rather than at the end is what makes partial progress
/// durable.
pub fn save_page(
    store: &mut Store,
    id: i64,
    users: &[User],
    cursor: Option<&str>,
) -> Result<SavedPage, StoreError> {
    // `BEGIN IMMEDIATE` rather than the default deferred one, for the reason
    // `SqliteRateBudget::reserve` gives at length: this reads before it writes,
    // and a deferred transaction pins a WAL read snapshot at the `SELECT` below
    // that the first `INSERT` then has to upgrade. `SQLITE_BUSY_SNAPSHOT` on
    // that upgrade does **not** invoke the busy handler, so the five-second
    // `busy_timeout` this connection sets does not cover it and the page fails
    // outright. What follows is a `WalkError::Save`, an early return that never
    // calls `close`, the page that was already paid for rolled back, and the
    // snapshot left claimed until the lease goes stale — over a page that would
    // have succeeded a moment later.
    //
    // The database is shared between processes on purpose, which is the whole
    // reason the claim exists; taking the write lock up front is what makes them
    // queue instead of collide.
    let tx = store
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;

    let first_ordinal: i64 = tx.query_row(
        "SELECT member_count FROM snapshots WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;

    let mut result = SavedPage::default();

    {
        // Compiled once for the whole page rather than once per account. This is
        // the only per-account loop in the program, and `Connection::execute`
        // prepares its statement every call.
        let mut member = tx.prepare_cached(
            "INSERT OR IGNORE INTO snapshot_members (snapshot_id, user_pk, ordinal)
             VALUES (?1, ?2, ?3)",
        )?;

        for u in users {
            if let Some(old) = super::users::upsert(&tx, u)? {
                result.renamed.push((u.pk, old));
            }

            // The ordinal counts only the rows that go in, so it advances with
            // `added` alone. Adding the position within the page as well used to
            // inflate it on every repeat — and Instagram repeats accounts across
            // pages, as the OR IGNORE above exists to absorb — which left later
            // pages starting at a lower ordinal than earlier ones had reached, so
            // `members` returned them out of the order Instagram served them in.
            //
            // The insert's own answer rather than `tx.changes()`: it is the count
            // for this statement, where the connection-wide one is whatever ran
            // last and would follow `upsert` if a statement were ever added
            // between the two.
            let inserted = member.execute(params![
                id,
                pk_to_sql(u.pk),
                first_ordinal + result.added as i64
            ])?;
            if inserted > 0 {
                result.added += 1;
            }
        }
    }

    // `claimed_at` moves with every page, which is what lets a claim outlive a
    // long walk without outliving a dead process: progress is the evidence that
    // somebody is still here.
    //
    // `claimed_by = ?4` in the WHERE is what makes the claim a rule rather than
    // a hint. Because `CLAIM_TTL_SECS` is now shorter than the resume window, a
    // walk that goes quiet for long enough really can have its row adopted by
    // another process — and the page it saves next would otherwise interleave
    // with the adopter's, inflating `member_count`, scrambling the ordinals and
    // clobbering the cursor, until whichever finished first closed a capture
    // that was missing the other's pages. Refusing the write leaves exactly one
    // writer per snapshot, which is the invariant the claim was added for.
    let held = tx.execute(
        "UPDATE snapshots
         SET member_count = member_count + ?2,
             pages        = pages + 1,
             requests     = requests + 1,
             next_cursor  = ?3,
             claimed_at   = ?5
         WHERE id = ?1 AND claimed_by = ?4",
        params![id, result.added as i64, cursor, this_process(), now()],
    )?;
    if held == 0 {
        // Dropping the transaction rolls the page back, so the capture is left
        // exactly as the process that holds it last saw it.
        return Err(StoreError::ClaimTaken);
    }

    tx.commit()?;
    Ok(result)
}

/// Closes the snapshot. Only a full walk leaves it usable for comparison.
///
/// **A finished capture is never unfinished again.** The `complete = 0` guard
/// is what says so, and it is not defensive coding: two processes that had
/// adopted one row could have the faster one close it `Completed` and the
/// slower one, throttled seconds later, close the same row `RateLimit` — and
/// the account's only usable capture disappeared, while the monitor's mark had
/// already moved past it. Claims make that pair unreachable now; this makes the
/// outcome unreachable regardless of how the pair arises.
///
/// The claim is released either way. The walk is over, so anything else may
/// have the row.
///
/// **And only this process's walk may end it.** `save_page` refuses a snapshot
/// this process does not hold, and AGENTS.md rests "a walk in progress has
/// exactly one writer" on that guard alone — but `close` was a second writer
/// with no guard at all, so a process whose claim had gone stale and been
/// adopted still closed the row and set `claimed_by` to NULL on the way out.
/// The adopter's next `save_page` then answered `ClaimTaken`, rolled back and
/// gave up: both walks died over one that should simply have stopped. The
/// window is narrow — an exhausted retry sequence is about two minutes against
/// a `CLAIM_TTL_SECS` of five — but a rationing budget or a suspend reaches it,
/// and it is the exact case the claim exists for.
pub fn close(conn: &Connection, id: i64, reason: StopReason) -> Result<(), StoreError> {
    let complete = reason.yields_complete_list();
    conn.execute(
        "UPDATE snapshots
         SET complete = ?2, taken_at = ?3, stopped_by = ?4,
             next_cursor = CASE WHEN ?2 = 1 THEN NULL ELSE next_cursor END,
             claimed_by = NULL, claimed_at = NULL
         WHERE id = ?1 AND complete = 0 AND claimed_by = ?5",
        params![id, complete, now(), reason.as_str(), this_process()],
    )?;
    Ok(())
}

/// The most recent usable snapshot.
///
/// Reads from the view, not the table, so it is impossible to return an
/// incomplete one even if someone gets the condition wrong.
///
/// `id DESC` breaks the tie, and it is not decoration. `taken_at` is in whole
/// seconds, so two walks that close inside the same second are equally recent
/// as far as the sort is concerned and SQLite may hand back either — which
/// makes "the newest capture" a question with two answers. Nothing noticed
/// while the only readers were the cache and the crossings, where either answer
/// is the same list; the monitor compares this id against the one it last
/// reported, so an arbitrary answer means a run that silently finds no changes.
/// `id` is the rowid, so it increases with every insert and orders the two the
/// way they actually happened.
pub fn latest_complete(
    conn: &Connection,
    account_pk: Pk,
    kind: ListKind,
) -> Result<Option<Snapshot>, StoreError> {
    let snapshot = conn
        .query_row(
            &format!(
                "SELECT {SNAPSHOT_COLUMNS}
                 FROM usable_snapshots
                 WHERE account_pk = ?1 AND kind = ?2
                 ORDER BY taken_at DESC, id DESC LIMIT 1"
            ),
            params![pk_to_sql(account_pk), kind.as_str()],
            row_to_snapshot,
        )
        .optional()?;
    Ok(snapshot)
}

/// One usable snapshot by id.
///
/// From the view, like [`latest_complete`], so a caller holding the id of a
/// walk that stopped short gets `None` rather than a capture with accounts
/// missing from it. That is the difference between a monitor saying nothing and
/// a monitor announcing two hundred departures that never happened, and it is
/// held here rather than by whoever remembers to check `complete`.
/// Whether anything has ever been captured of this list, finished or not.
///
/// A deliberately weaker question than [`latest_complete`], and the difference
/// is the whole point. A walk that stopped short is not a capture anything may
/// be compared against — but it ran `save_page`, and `save_page` runs
/// `users::upsert`, so it filed `username_history` rows for the people it did
/// see. Those people are members of that list, and a rename among them is a
/// rename this list could have reported.
///
/// So "has this list ever had a *complete* capture" is the wrong test for
/// whether it may let the rename window close over it: on an account whose
/// second list meets the truncation wall every time, the answer is `None`
/// forever while the list goes on filing history rows every run.
pub fn any_capture(conn: &Connection, account_pk: Pk, kind: ListKind) -> Result<bool, StoreError> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM snapshots WHERE account_pk = ?1 AND kind = ?2 LIMIT 1",
            params![pk_to_sql(account_pk), kind.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

pub fn find_usable(conn: &Connection, id: i64) -> Result<Option<Snapshot>, StoreError> {
    let snapshot = conn
        .query_row(
            &format!("SELECT {SNAPSHOT_COLUMNS} FROM usable_snapshots WHERE id = ?1"),
            params![id],
            row_to_snapshot,
        )
        .optional()?;
    Ok(snapshot)
}

/// The users in a snapshot, in the order Instagram served them.
pub fn members(conn: &Connection, id: i64) -> Result<Vec<User>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT u.pk, u.username, u.full_name, u.is_private, u.is_verified, u.pfp_url
         FROM snapshot_members m JOIN users u ON u.pk = m.user_pk
         WHERE m.snapshot_id = ?1
         ORDER BY m.ordinal",
    )?;
    let rows = stmt.query_map(params![id], super::users::row_to_user)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Discards half-finished walks for this account and list. Called when starting
/// a new one, so partials that no longer serve do not pile up.
pub fn delete_partials(
    conn: &Connection,
    account_pk: Pk,
    kind: ListKind,
) -> Result<usize, StoreError> {
    // Anything another process is actively writing to is left alone. This used
    // to delete every incomplete row, so one process starting a walk removed a
    // walk another was in the middle of — the victim's next `save_page` then
    // failed against a row that no longer existed, having spent its requests
    // for nothing.
    //
    // A claim that has gone stale is not protection: whoever held it is gone,
    // and the row is exactly the abandoned partial this is here to clear.
    let deleted = conn.execute(
        "DELETE FROM snapshots
         WHERE account_pk = ?1 AND kind = ?2 AND complete = 0
           AND (claimed_by IS NULL OR claimed_by = ?3
                OR claimed_at IS NULL OR claimed_at < ?4)",
        params![
            pk_to_sql(account_pk),
            kind.as_str(),
            this_process(),
            now() - CLAIM_TTL_SECS,
        ],
    )?;
    Ok(deleted)
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<Snapshot> {
    let kind: String = row.get(2)?;
    Ok(Snapshot {
        id: row.get(0)?,
        account_pk: pk_from_sql(row.get(1)?),
        kind: if kind == "followers" {
            ListKind::Followers
        } else {
            ListKind::Following
        },
        started_at: row.get(3)?,
        taken_at: row.get(4)?,
        member_count: row.get::<_, i64>(5)? as u64,
        declared_count: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        pages: row.get::<_, i64>(7)? as u32,
        next_cursor: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{accounts, users};

    fn user(pk: Pk) -> User {
        User {
            pk,
            username: format!("user{pk}"),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    /// The two windows are numbers somebody chose, and every other test in the
    /// tree refers to them by name -- so `RESUME_WINDOW_SECS = 6 * 60` passes
    /// the whole suite while every walk paused for longer than six minutes
    /// silently begins again at page one, and `CLAIM_TTL_SECS = 60` makes a
    /// walk that pauses for a minute look abandoned to any other process.
    ///
    /// Written out, the way `the_default_pace_is_the_documented_one` writes out
    /// the pacing: changing one has to be deliberate and has to say so here.
    #[test]
    fn the_two_windows_are_the_documented_ones() {
        assert_eq!(RESUME_WINDOW_SECS, 15 * 60);
        assert_eq!(CLAIM_TTL_SECS, 5 * 60);
    }

    fn base() -> Store {
        let db = Store::in_memory().unwrap();
        users::upsert(db.conn(), &user(1)).unwrap();
        accounts::upsert(db.conn(), 1, true).unwrap();
        db
    }

    /// Pretends to be another process by writing its claim directly.
    ///
    /// `this_process` is a per-process constant, so a test cannot be a second
    /// process — but every read of a claim compares against that constant, and
    /// a row claimed by anything else is exactly what the other process leaves
    /// behind. `at` is when that process last saved a page.
    fn claimed_by_somebody_else(db: &Store, id: i64, at: i64) {
        db.conn()
            .execute(
                "UPDATE snapshots SET claimed_by = 'another-process', claimed_at = ?2
                 WHERE id = ?1",
                params![id, at],
            )
            .unwrap();
    }

    /// A walk somebody else is in the middle of is not adopted.
    ///
    /// This is the whole point of the claim. One `snob watch` running while
    /// somebody types `snob followers` used to have both processes continue the
    /// same partial and write into one capture.
    #[test]
    fn a_walk_another_process_is_working_on_is_left_alone() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, id, &[user(10)], Some("cursor")).unwrap();
        claimed_by_somebody_else(&db, id, now());

        assert!(
            resumable(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_none(),
            "somebody else is walking this one"
        );
    }

    /// And one they abandoned is. Nothing can tell us a process died, so the
    /// evidence is that it has stopped saving pages.
    #[test]
    fn a_walk_abandoned_long_enough_is_adopted() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, id, &[user(10)], Some("cursor")).unwrap();
        claimed_by_somebody_else(&db, id, now() - CLAIM_TTL_SECS - 1);

        let adopted = resumable(db.conn(), 1, ListKind::Followers)
            .unwrap()
            .expect("whoever held it is gone");
        assert_eq!(adopted.id, id);
    }

    /// Taking it is what `resumable` does, not something the caller remembers
    /// to do afterwards — so asking twice from two places cannot hand it out
    /// twice.
    #[test]
    fn resuming_takes_the_claim_in_the_same_breath() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, id, &[user(10)], Some("cursor")).unwrap();
        // Free for anybody.
        db.conn()
            .execute(
                "UPDATE snapshots SET claimed_by = NULL, claimed_at = NULL WHERE id = ?1",
                params![id],
            )
            .unwrap();

        assert!(
            resumable(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_some()
        );

        let holder: Option<String> = db
            .conn()
            .query_row(
                "SELECT claimed_by FROM snapshots WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(holder.as_deref(), Some(this_process()));
    }

    /// Asking whether a walk could be continued does not continue it.
    ///
    /// `engine::walk` asks right after closing its own snapshot, to pick the
    /// sentence it prints. It asked with `resumable`, which claims the row it
    /// finds — so the exiting process took back the claim `close` had just
    /// released, and the next invocation found a partial it could not adopt and
    /// could not clear. Six commands walked every interrupted list again from
    /// page one while telling the user it would continue.
    #[test]
    fn asking_whether_a_walk_is_resumable_does_not_claim_it() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, id, &[user(10)], Some("cursor")).unwrap();
        close(db.conn(), id, StopReason::Canceled).unwrap();

        assert!(
            is_resumable(db.conn(), 1, ListKind::Followers).unwrap(),
            "the walk stopped with a cursor, inside the window"
        );

        let holder: Option<String> = db
            .conn()
            .query_row(
                "SELECT claimed_by FROM snapshots WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(holder, None, "asking is not taking");
    }

    /// There is a window in which another process can adopt a killed walk.
    ///
    /// A hard kill never reaches `close`, so the claim `save_page` last wrote
    /// stands. With the two constants equal that claim could not go stale before
    /// the partial stopped being worth resuming — `claimed_at` is never earlier
    /// than `started_at` — so the two conditions were mutually exclusive and
    /// resume was dead for every list command. The ordering of the two constants
    /// is held by the `const` assertion beside them; this is the state it makes
    /// reachable.
    #[test]
    fn a_claim_goes_stale_while_the_partial_is_still_worth_resuming() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, id, &[user(10)], Some("cursor")).unwrap();

        // A walk that started well inside the resume window and whose process
        // was killed a moment after its last page: no `close`, so the claim is
        // still there.
        let started = now() - CLAIM_TTL_SECS - 60;
        db.conn()
            .execute(
                "UPDATE snapshots SET started_at = ?2 WHERE id = ?1",
                params![id, started],
            )
            .unwrap();
        claimed_by_somebody_else(&db, id, started + 30);

        let adopted = resumable(db.conn(), 1, ListKind::Followers)
            .unwrap()
            .expect("the claim is stale and the partial is young");
        assert_eq!(adopted.id, id);
    }

    /// A page for a walk this process no longer holds is refused, not written.
    ///
    /// The claim only bounds how long an abandoned walk looks busy; it cannot
    /// stop the walk that was abandoned from waking up. Without this guard that
    /// walk's next page interleaves with the adopter's — inflating
    /// `member_count`, scrambling the ordinals and clobbering the cursor — until
    /// whichever finished first closed a capture missing the other's pages.
    #[test]
    fn a_page_for_a_walk_somebody_else_took_over_is_refused() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, id, &[user(10)], Some("cursor")).unwrap();
        claimed_by_somebody_else(&db, id, now());

        let refused = save_page(&mut db, id, &[user(11)], Some("further"));
        assert!(matches!(refused, Err(StoreError::ClaimTaken)));

        let (count, cursor): (i64, Option<String>) = db
            .conn()
            .query_row(
                "SELECT member_count, next_cursor FROM snapshots WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "the refused page rolled back");
        assert_eq!(cursor.as_deref(), Some("cursor"), "the cursor is untouched");
    }

    /// Closing a walk this process no longer holds is refused, like saving one.
    ///
    /// `save_page` carried the only claim guard, and AGENTS.md rests "a walk in
    /// progress has exactly one writer" on it — but `close` was a second writer
    /// with none, and it sets `claimed_by` to NULL. So a process whose claim had
    /// gone stale and been adopted still ended the row on its way out, and the
    /// adopter's next page answered `ClaimTaken`, rolled back and gave up: two
    /// walks died where one should merely have stopped.
    #[test]
    fn closing_a_walk_somebody_else_took_over_is_refused() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, id, &[user(10)], Some("cursor")).unwrap();
        claimed_by_somebody_else(&db, id, now());

        close(db.conn(), id, StopReason::Completed).unwrap();

        let (complete, claimed): (i64, Option<String>) = db
            .conn()
            .query_row(
                "SELECT complete, claimed_by FROM snapshots WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(complete, 0, "the adopter's walk is still in progress");
        assert_eq!(
            claimed.as_deref(),
            Some("another-process"),
            "and it still holds the claim it took"
        );
    }

    /// The row that comes back is the row that was claimed.
    ///
    /// The read-back used to be a second statement that dropped both predicates
    /// making a partial resumable — `next_cursor IS NOT NULL` and the resume
    /// window — and simply took the newest row this process held. A claimed,
    /// cursor-less row left behind by this process is newer than the partial
    /// actually claimed, and `walk::fetch` leaves exactly that behind when a
    /// save or the budget fails: it returns without calling `close`.
    ///
    /// What followed was `mark_resumed` against the wrong id, a walk restarted
    /// from page one under the wrong `started_at`, and the real partial
    /// abandoned still holding this process's claim.
    #[test]
    fn resuming_returns_the_partial_it_claimed_and_not_a_newer_dead_one() {
        let mut db = base();

        // The partial worth resuming: it has a cursor.
        let wanted = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, wanted, &[user(10)], Some("cursor")).unwrap();

        // And a newer row this process opened and walked away from without
        // closing, so it is claimed, incomplete and has no cursor at all.
        let abandoned = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;

        // Forced apart in the fixture rather than left to the clock. `begin`
        // stamps `started_at` in whole seconds, so two rows opened in one test
        // share it and `ORDER BY started_at DESC` is a tie SQLite may break
        // either way — which would make this pass or fail on timing rather than
        // on the thing it is about.
        db.conn()
            .execute(
                "UPDATE snapshots SET started_at = started_at - 10 WHERE id = ?1",
                params![wanted],
            )
            .unwrap();
        assert!(abandoned > wanted, "the dead one has to be the newer row");

        let adopted = resumable(db.conn(), 1, ListKind::Followers)
            .unwrap()
            .expect("there is a partial with a cursor to continue");
        assert_eq!(
            adopted.id, wanted,
            "the row handed back must be the row the claim was taken on"
        );
        assert_eq!(adopted.next_cursor.as_deref(), Some("cursor"));
    }

    /// A capture that stopped short is not found by id either.
    ///
    /// `find_usable` reads the view rather than the table, and AGENTS.md names
    /// that view first under "structural guards over discipline": the monitor
    /// holds an id and asks for it back, and a half-walked list handed over
    /// there is two hundred people reported as having left.
    ///
    /// Nothing asserted it. Every existing call is `is_some()` on a genuinely
    /// complete capture or `is_none()` on a row that had been deleted, and both
    /// of those pass with `FROM snapshots` just as well.
    #[test]
    fn a_capture_that_stopped_short_is_never_found_by_id() {
        let mut db = base();

        let cut_short = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, cut_short, &[user(10)], Some("cursor")).unwrap();
        close(db.conn(), cut_short, StopReason::RateLimit).unwrap();
        assert!(
            find_usable(db.conn(), cut_short).unwrap().is_none(),
            "a walk that stopped short is not something to compare against"
        );

        // The other half of the view: begun, never closed, so no `taken_at`.
        let still_open = begin(db.conn(), 1, ListKind::Following, Some(10))
            .unwrap()
            .id;
        assert!(find_usable(db.conn(), still_open).unwrap().is_none());

        // And one it does answer for, so this cannot pass by refusing
        // everything.
        let whole = begin(db.conn(), 1, ListKind::Following, Some(1))
            .unwrap()
            .id;
        save_page(&mut db, whole, &[user(11)], None).unwrap();
        close(db.conn(), whole, StopReason::Completed).unwrap();
        assert!(find_usable(db.conn(), whole).unwrap().is_some());
    }

    /// A finished capture is never unfinished again.
    ///
    /// Two processes on one row could have the faster close it `Completed` and
    /// the slower, throttled seconds later, close the same row `RateLimit` —
    /// and the account's only usable capture vanished while the monitor's mark
    /// had already moved past it.
    #[test]
    fn closing_a_finished_capture_again_cannot_unfinish_it() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, id, &[user(10)], None).unwrap();
        close(db.conn(), id, StopReason::Completed).unwrap();

        close(db.conn(), id, StopReason::RateLimit).unwrap();

        assert!(
            find_usable(db.conn(), id).unwrap().is_some(),
            "a capture that was finished stopped being usable"
        );
    }

    /// Starting a walk clears abandoned partials and leaves live ones alone.
    /// It used to delete every incomplete row, so one process starting a walk
    /// removed one another was actively writing to.
    #[test]
    fn clearing_partials_spares_the_one_somebody_is_writing_to() {
        let mut db = base();

        let live = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, live, &[user(10)], Some("cursor")).unwrap();
        claimed_by_somebody_else(&db, live, now());

        let abandoned = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        save_page(&mut db, abandoned, &[user(11)], Some("cursor")).unwrap();
        claimed_by_somebody_else(&db, abandoned, now() - CLAIM_TTL_SECS - 1);

        assert_eq!(
            delete_partials(db.conn(), 1, ListKind::Followers).unwrap(),
            1
        );

        let left: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM snapshots WHERE id = ?1",
                params![live],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left, 1, "the walk somebody is in the middle of was deleted");
    }

    #[test]
    fn a_fresh_snapshot_is_not_complete() {
        let db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(10))
            .unwrap()
            .id;
        assert!(
            latest_complete(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_none()
        );
        assert_eq!(members(db.conn(), id).unwrap().len(), 0);
    }

    #[test]
    fn saving_pages_accumulates_members_in_order() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, Some(4))
            .unwrap()
            .id;

        save_page(&mut db, id, &[user(10), user(11)], Some("c1")).unwrap();
        save_page(&mut db, id, &[user(12), user(13)], None).unwrap();

        let pks: Vec<_> = members(db.conn(), id)
            .unwrap()
            .into_iter()
            .map(|u| u.pk)
            .collect();
        assert_eq!(pks, vec![10, 11, 12, 13]);
    }

    #[test]
    fn repeats_across_pages_are_not_counted_twice() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;

        let first = save_page(&mut db, id, &[user(10), user(11)], Some("c")).unwrap();
        // Instagram repeats accounts across pages when the list shifts.
        let second = save_page(&mut db, id, &[user(11), user(12)], None).unwrap();

        assert_eq!(first.added, 2);
        assert_eq!(second.added, 1);
        assert_eq!(members(db.conn(), id).unwrap().len(), 3);
    }

    /// Instagram repeats accounts across pages when the list shifts under it.
    /// A repeat must not push the ordinal forward, or the accounts after it
    /// come back in a different order than they were served in — and that
    /// order is the chronological one the user reads.
    #[test]
    fn repeats_do_not_disturb_the_order_of_what_follows() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;

        save_page(&mut db, id, &[user(10), user(11)], Some("c1")).unwrap();
        // The first two come round again, with one genuinely new behind them.
        save_page(&mut db, id, &[user(10), user(11), user(12)], Some("c2")).unwrap();
        save_page(&mut db, id, &[user(13)], None).unwrap();

        let pks: Vec<_> = members(db.conn(), id)
            .unwrap()
            .into_iter()
            .map(|u| u.pk)
            .collect();
        assert_eq!(pks, vec![10, 11, 12, 13]);
    }

    #[test]
    fn saving_a_page_advances_the_cursor() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10)], Some("next")).unwrap();

        let pending = resumable(db.conn(), 1, ListKind::Followers)
            .unwrap()
            .unwrap();
        assert_eq!(pending.id, id);
        assert_eq!(pending.next_cursor.as_deref(), Some("next"));
        assert_eq!(pending.pages, 1);
    }

    #[test]
    fn renames_are_reported_when_saving() {
        let mut db = base();
        users::upsert(db.conn(), &user(10)).unwrap();

        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        let mut renamed = user(10);
        renamed.username = "brand_new_name".into();

        let saved = save_page(&mut db, id, &[renamed], None).unwrap();
        assert_eq!(saved.renamed, vec![(10, "user10".to_string())]);
    }

    /// The most important rule of the store: a half-finished snapshot can never
    /// be the basis of a comparison, because everything missing from it would
    /// show up as a departure.
    #[test]
    fn latest_complete_never_returns_a_partial_one() {
        let mut db = base();

        for reason in [
            StopReason::Canceled,
            StopReason::Truncated,
            StopReason::RateLimit,
            StopReason::Network,
            StopReason::SessionInvalid,
            StopReason::PageLimit,
        ] {
            let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
            save_page(&mut db, id, &[user(10)], None).unwrap();
            close(db.conn(), id, reason).unwrap();

            assert!(
                latest_complete(db.conn(), 1, ListKind::Followers)
                    .unwrap()
                    .is_none(),
                "a snapshot cut short by {reason:?} should not be usable"
            );
        }

        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10)], None).unwrap();
        close(db.conn(), id, StopReason::Completed).unwrap();
        assert!(
            latest_complete(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_some()
        );
    }

    /// "The newest capture" has to have one answer, not either of two.
    ///
    /// `taken_at` is in whole seconds, so two walks closing inside the same
    /// second are equally recent as far as the sort can tell, and without the
    /// tie-break SQLite may hand back the older one. Nothing noticed while the
    /// only readers were the cache and the crossings, where either answer is
    /// the same list. The monitor compares this id against the one it last
    /// reported, so an arbitrary answer is a run that finds no changes and says
    /// nothing about why.
    #[test]
    fn the_newest_of_two_captures_taken_in_one_second_is_the_later_one() {
        let mut db = base();

        let mut ids = Vec::new();
        for _ in 0..2 {
            let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
            save_page(&mut db, id, &[user(10)], None).unwrap();
            close(db.conn(), id, StopReason::Completed).unwrap();
            ids.push(id);
        }

        // Forced, not hoped for. `close` stamps `taken_at` from the clock and
        // nothing here freezes it, so this used to *assert* that the two rows
        // had landed in one second — which is true almost always and false when
        // the runner is loaded enough to be preempted between them. The test
        // then failed on its own precondition rather than on the tie-break, in
        // the one shape nobody can reproduce.
        db.conn()
            .execute(
                "UPDATE snapshots SET taken_at = 1000 WHERE complete = 1",
                [],
            )
            .unwrap();

        assert_eq!(
            latest_complete(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .unwrap()
                .id,
            ids[1]
        );
    }

    /// Guards against the enum and the schema drifting apart: every variant has
    /// to be accepted by the CHECK constraint.
    #[test]
    fn every_stop_reason_is_accepted_by_the_schema() {
        let mut db = base();
        for reason in StopReason::ALL {
            let id = begin(db.conn(), 1, ListKind::Following, None).unwrap().id;
            save_page(&mut db, id, &[user(10)], None).unwrap();
            close(db.conn(), id, reason)
                .unwrap_or_else(|e| panic!("the schema rejected {reason:?}: {e}"));
        }
    }

    #[test]
    fn closing_cleanly_clears_the_pending_cursor() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10)], Some("leftover")).unwrap();
        close(db.conn(), id, StopReason::Completed).unwrap();

        let snapshot = latest_complete(db.conn(), 1, ListKind::Followers)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.next_cursor, None);
        assert!(
            resumable(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn an_interrupted_one_keeps_its_cursor_to_resume() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10)], Some("from_here")).unwrap();
        close(db.conn(), id, StopReason::Canceled).unwrap();

        let pending = resumable(db.conn(), 1, ListKind::Followers)
            .unwrap()
            .unwrap();
        assert_eq!(pending.next_cursor.as_deref(), Some("from_here"));
    }

    #[test]
    fn something_too_old_is_not_resumed() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10)], Some("stale")).unwrap();

        let long_ago = now() - RESUME_WINDOW_SECS - 1;
        db.conn()
            .execute(
                "UPDATE snapshots SET started_at = ?1 WHERE id = ?2",
                params![long_ago, id],
            )
            .unwrap();

        assert!(
            resumable(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_two_lists_do_not_mix() {
        let mut db = base();
        let followers = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, followers, &[user(10)], None).unwrap();
        close(db.conn(), followers, StopReason::Completed).unwrap();

        assert!(
            latest_complete(db.conn(), 1, ListKind::Following)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn deleting_a_snapshot_deletes_its_members() {
        let mut db = base();
        let id = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, id, &[user(10), user(11)], None).unwrap();

        db.conn()
            .execute("DELETE FROM snapshots WHERE id = ?1", params![id])
            .unwrap();

        let left: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM snapshot_members", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(left, 0, "the foreign key cascade did not fire");
    }

    #[test]
    fn deleting_partials_spares_the_complete_ones() {
        let mut db = base();
        let good = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, good, &[user(10)], None).unwrap();
        close(db.conn(), good, StopReason::Completed).unwrap();

        let bad = begin(db.conn(), 1, ListKind::Followers, None).unwrap().id;
        save_page(&mut db, bad, &[user(11)], Some("c")).unwrap();

        assert_eq!(
            delete_partials(db.conn(), 1, ListKind::Followers).unwrap(),
            1
        );
        assert!(
            latest_complete(db.conn(), 1, ListKind::Followers)
                .unwrap()
                .is_some()
        );
    }
}
