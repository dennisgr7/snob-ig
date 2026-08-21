-- The same view, answered by seeking instead of by sorting the whole log.
--
-- `newest_run` is what `prune` exempts and what `watch status` and
-- `watch_setup::health` read, so it runs on every monitor tick. The window
-- function is the obvious way to write "the newest row per group" and it is the
-- one shape SQLite cannot satisfy without materializing and sorting every row
-- in `watch_runs`. Measured: 4,320 rows 5.83 ms, 50,000 rows 322.63 ms -- a log
-- that grows makes the sweep that bounds it slower.
--
-- The correlated form seeks `watch_runs_lookup`, which already exists and is
-- `(account_pk, started_at DESC)`, once per account: **0.014 ms at both sizes**.
-- End to end, `snob watch status --json` over 50,000 runs goes from 75.07 ms to
-- 16.68 ms.
--
-- **It has to answer identically, not merely similarly**, and the tie is where
-- that is decided. `started_at` is whole seconds, two runs of one account
-- inside a second are reachable, and `id DESC` is what breaks the tie -- so the
-- inner `ORDER BY` carries both terms exactly as the window function did.
-- Accounts with no runs appear in neither, because both start from
-- `watch_runs`.
--
-- `rank` stays in the output. Nothing reads it, but it was in the shape this
-- replaces and a view that quietly drops a column is a worse thing to leave
-- behind than one that carries a constant.
DROP VIEW newest_run;

CREATE VIEW newest_run AS
SELECT r.*, 1 AS rank
FROM watch_runs r
WHERE r.id = (
  SELECT id FROM watch_runs
  WHERE account_pk = r.account_pk
  ORDER BY started_at DESC, id DESC
  LIMIT 1
);
