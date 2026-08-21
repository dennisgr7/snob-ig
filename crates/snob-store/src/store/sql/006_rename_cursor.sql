-- The rename window, per account rather than per list.
--
-- `watch_marks` carried a `history_cursor` per (account, list), and a cursor is
-- not a per-list thing. `username_history` is one sequence covering every account
-- this tool has ever seen; "how far along it has this account been reported"
-- has one answer, and storing it twice let the two copies disagree.
--
-- Both directions of that disagreement were live defects. Reading the **oldest**
-- of the two -- which is what the code did, so that nothing between them was
-- skipped -- meant that a list refused for a week left its copy behind, and the
-- next run that could compare re-read a window already sent: somebody in both
-- lists, which is the ordinary case for a friend, was announced twice under two
-- different `run_id`s, and the `run_id` is the value receivers are told to
-- deduplicate on. Reading the newest instead would have skipped the window
-- between them for anybody in only one list. There is no right way to read two
-- numbers that should have been one.
--
-- The other half of the same defect was in when the cursor moved. It advanced
-- for every list a run did not refuse, including on a run that read no window at
-- all: two unmoved counters is the documented common case -- "a run with nothing
-- to report costs one request" -- and such a run compared nothing, looked for no
-- renames, and still jumped the cursor to the global head of
-- `username_history`. Anything filed in between by another walk was stepped over
-- and could never be reported by anything, `snob watch diff` included, because it
-- reads the same cursor. It moves only when a window was actually read now, which
-- is a rule this table can hold and two per-list copies could not.
CREATE TABLE watch_renames (
  account_pk INTEGER NOT NULL PRIMARY KEY REFERENCES accounts(pk) ON DELETE CASCADE,
  -- The last `username_history` row that has been reported for this account.
  cursor     INTEGER NOT NULL,
  marked_at  INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- Carried over so an existing installation does not re-announce every rename it
-- has already sent. The largest of the account's per-list copies is the one that
-- was certainly reported: every rename below it went out with the list that had
-- got that far.
INSERT INTO watch_renames (account_pk, cursor, marked_at)
SELECT account_pk, max(history_cursor), max(compared_at) FROM watch_marks
GROUP BY account_pk;

-- And the column goes, rather than being left written by nothing: a value two
-- places can hold is a value two places can disagree about, which is the whole
-- reason for this migration. `DROP COLUMN` rather than the create-copy-drop-
-- rename recipe -- nothing indexes or constrains this column, and the recipe is
-- the one the header of `migrations.rs` warns about for a `WITHOUT ROWID` child.
ALTER TABLE watch_marks DROP COLUMN history_cursor;
