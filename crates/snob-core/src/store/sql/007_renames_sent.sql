-- Which renames have actually been announced, rather than how far a number got.
--
-- `watch_renames.cursor` was asked two questions it cannot both answer: *what
-- have I scanned* and *what have I announced*. They come apart in both
-- directions, and neither direction is exotic.
--
-- **Announced twice.** The cursor moves only when every list of the account is
-- accounted for, which is right: a refused list must not let the window close
-- over people only it can see. But the *announcing* is gated on there being any
-- verified list at all. So a tick with one list refused and the other unchanged
-- announces a rename and then files nothing saying it did, and the next tick
-- re-reads the identical window against the identical capture and announces it
-- again -- under a fresh `run_id`, which is the value receivers are told to
-- deduplicate on. On an account whose second list meets the truncation wall
-- every time, that is every run for ever.
--
-- **Never announced.** `renames_since` joins the members of a *verified*
-- capture, so a rename of somebody only in the refused list sits inside the
-- window and is simply not found. It is not skipped by arithmetic; it is
-- invisible to the query that run.
--
-- A second watermark cannot fix both, and it is worth writing down why, because
-- it is the obvious design and it is wrong. A high-water mark of "announced",
-- with rows at or below it excluded, closes the first: the re-read finds the
-- rename below the mark and drops it. It closes the second by the same stroke,
-- in the wrong sense -- the rename that was never found is also below the mark,
-- so it is dropped for ever, and the run that could finally see it is the run
-- that is forbidden to.
--
-- The question is per (account, rename), so that is what is stored. The set is
-- bounded by the open window rather than by history: an entry is only wanted
-- between `watch_renames.cursor` and the head, and `prune` drops the rest --
-- so an account whose lists are all readable carries none of these at all, and
-- an account stuck behind a wall carries exactly the rows that must not be
-- lost.
CREATE TABLE watch_renames_sent (
  account_pk INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
  -- The `username_history` row that was announced.
  history_id INTEGER NOT NULL,
  sent_at    INTEGER NOT NULL,
  PRIMARY KEY (account_pk, history_id)
) STRICT, WITHOUT ROWID;

-- Nothing is carried over. Every existing row of `watch_renames` holds a cursor
-- that was already treated as "scanned and announced up to here", and the
-- reader keeps that meaning: a rename at or below the cursor is outside the
-- window and is never read at all. So an installation upgrading has an empty
-- set and the same behaviour it had, and the set only starts filling for
-- windows that stay open from here.
