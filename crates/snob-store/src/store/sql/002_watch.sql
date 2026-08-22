-- The monitor's state.
--
-- Units follow 001: every timestamp is UTC epoch, in seconds unless the column
-- name ends in _ms. Every table is STRICT, for the reason 001 gives.
--
-- What this adds is the one thing a temporal diff needs and a static crossing
-- does not: a memory of what has already been *reported*. Everything else the
-- monitor reads -- snapshots, their members, the rename history -- was already
-- being written before anything consumed it.

-- The snapshot each account and list was last reported against.
--
-- The obvious alternative is "the second most recent snapshot", and it is wrong
-- twice over. A `snob followers` typed by hand between two ticks would become a
-- baseline nobody ever reported, so the next tick would compare against it and
-- silently swallow the changes the run before had found. And a delivery that
-- was given up on would be re-derived from a list that had already moved on.
-- A snapshot is a capture; this is a receipt, and only a receipt knows what has
-- been counted.
CREATE TABLE watch_marks (
  account_pk  INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
  kind        TEXT    NOT NULL CHECK (kind IN ('followers', 'following')),
  -- The capture the next diff measures against.
  --
  -- Nullable, and ON DELETE SET NULL rather than CASCADE: if the capture it
  -- names is ever pruned the baseline is gone, and what has to happen then is
  -- that the next run lays down a new one and reports nothing. CASCADE would
  -- take the whole row, and with it `compared_at` -- so the next run would not
  -- only lose the baseline but also the left edge of the rename window, and
  -- would either re-announce old renames or announce none.
  snapshot_id INTEGER REFERENCES snapshots(id) ON DELETE SET NULL,
  -- When the last report was made. What `snob watch status` prints, and the
  -- interval a report names to a person.
  --
  -- The run's own clock, not the capture's `taken_at`. Those differ whenever
  -- somebody types `snob followers` between two ticks, and what the user is
  -- being told is when they were last told something.
  compared_at INTEGER NOT NULL,
  -- The last `username_history` row that has been reported, as an id.
  --
  -- An id and not a timestamp. `changed_at` is in whole seconds, so a rename
  -- filed in the same second a report is written is neither clearly before it
  -- nor clearly after: an inclusive bound announces it twice and an exclusive
  -- one loses it for good. The history's `id` is the rowid, so it increases
  -- with every insert and puts every rename unambiguously on one side or the
  -- other. Zero means nothing has been reported yet, which is what a row
  -- written before this column existed would also mean.
  history_cursor INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (account_pk, kind)
) STRICT, WITHOUT ROWID;

-- One row per tick, including the ticks that found nothing.
--
-- "It ran and there was nothing" and "it never ran" are different answers, and
-- distinguishing them is most of what `snob watch status` is for: a monitor
-- that quietly stopped looks exactly like a quiet account.
CREATE TABLE watch_runs (
  id          INTEGER PRIMARY KEY,
  account_pk  INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
  started_at  INTEGER NOT NULL,
  finished_at INTEGER,            -- NULL while the tick is still running
  requests    INTEGER NOT NULL DEFAULT 0,
  -- Mirrors snob_core::watch::RunOutcome::as_str(), which is also what
  -- ExitCode::as_str() writes. The same vocabulary as the README's table, so a
  -- caller reading this column and a caller reading $? are told the same thing
  -- by the same name. A row spelled outside this list can only have been
  -- written by a newer build, and is read back as RecordedOutcome::Unknown.
  outcome     TEXT CHECK (outcome IS NULL OR outcome IN
                ('ok', 'error', 'no_session', 'challenge', 'rate_limited',
                 'interrupted')),
  changes     INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX watch_runs_lookup ON watch_runs(account_pk, started_at DESC);
