-- The instant an interval schedule is measured from, when nothing has run yet.
--
-- `seed_last_run` answers `Some(now)` for an interval with an empty run log, so
-- that a fresh install waits one interval rather than walking the second it is
-- set up. That is right, and it was never written down: the value lived in a
-- local variable, and `last_started` reads `watch_runs`, whose only writer is a
-- tick that succeeded. So while the log stayed empty, *every process start
-- measured the interval from that start*.
--
-- Which means a configured monitor that never looks at anything. `snob watch`
-- in a login item on a laptop up eight hours a day, with `--every 1d` -- the
-- wizard's own suggestion -- is due at hour twenty-four and shut down at hour
-- eight, every day, for ever. Sharper on a server: `--every 2w`, another of the
-- wizard's three examples, needs a fortnight of unbroken uptime, and
-- `Restart=always` after one crash puts it back to zero. `status` says "it has
-- not run yet" the whole time, which is exactly true and reads as the tool
-- being new rather than stuck.
--
-- A table rather than a column on something: this is a fact about the monitor
-- on this machine, not about an account, and `watch_runs` cannot hold it
-- because its whole point is to be empty until a run finishes.
CREATE TABLE watch_state (
  key   TEXT NOT NULL PRIMARY KEY,
  value INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
