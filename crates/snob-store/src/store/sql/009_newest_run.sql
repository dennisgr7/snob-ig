-- "The newest run of each account", once, so retention and the reader cannot
-- come to mean different rows.
--
-- It was spelled twice, forty lines apart in the same file: `prune` exempts it
-- from the thirty-day sweep so `status` never says "it has not run yet" about a
-- monitor that has been failing for weeks, and `last_runs` reads it to say what
-- happened. Both carried the same window function, the same partition and the
-- same `started_at DESC, id DESC`.
--
-- That last term is the one that makes the pair dangerous rather than merely
-- repetitive. `started_at` is whole seconds, so two runs of one account inside
-- the same second are a tie -- reachable through `snob watch once` beside a
-- scheduled tick, which consults no schedule and no gap -- and `id DESC` is
-- what breaks it. Added to one site and not the other, prune keeps one of the
-- two rows and `status` reads the other: the row a person is shown is deleted
-- on the next settle, and the row that survives is the one nothing displays.
-- Nothing fails, the suite stays green, and `snob watch status` starts
-- describing a run nobody can see any more.
--
-- A view rather than a shared string, because `usable_snapshots` is already
-- exactly this and for exactly this reason -- a rule the database holds rather
-- than one every reader remembers.
--
-- `SELECT *` inside, so a column added to `watch_runs` arrives here by being a
-- column. The rank is projected out by name at each reader, which is what keeps
-- `SELECT *` from leaking it.
CREATE VIEW newest_run AS
SELECT *
FROM (
  SELECT *, row_number() OVER (
    PARTITION BY account_pk ORDER BY started_at DESC, id DESC
  ) AS rank
  FROM watch_runs
)
WHERE rank = 1;
