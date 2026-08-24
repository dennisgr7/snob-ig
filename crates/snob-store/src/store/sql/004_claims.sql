-- Who is walking a snapshot right now.
--
-- Two columns, added rather than a table rebuilt: `ALTER TABLE ADD COLUMN` does
-- not touch the existing rows' storage, so this avoids the create-copy-drop-
-- rename recipe entirely and with it the foreign-key trap the header of
-- `migrations.rs` warns about.
--
-- Why this exists. `resumable` looks for any half-finished walk of an account
-- and list and continues it, and until now it had no way to tell one that was
-- abandoned from one that another process is in the middle of. One `snob watch`
-- running while somebody types `snob followers` was enough for both to adopt
-- the same row, and then:
--
--   * whichever finished first closed it `complete = 1` while the other was
--     still paging into it, so a reader saw a list that claimed to be whole and
--     was not -- the one thing `usable_snapshots` exists to make impossible;
--   * or the slower one was throttled and closed the same row `complete = 0`,
--     and the account's only finished capture vanished;
--   * or one process's `delete_partials` removed a walk the other was actively
--     writing to.
--
-- A claim is a soft lease, not a lock. Nothing here can stop a process being
-- killed, so the question a reader has to answer is "has anybody touched this
-- recently", and `claimed_at` is what answers it. `save_page` refreshes it, so
-- a walk that is making progress holds its claim however long the list is, and
-- one whose process died stops holding it a few minutes later.
ALTER TABLE snapshots ADD COLUMN claimed_by TEXT;
ALTER TABLE snapshots ADD COLUMN claimed_at INTEGER;

-- The lookup `resumable` and `delete_partials` both make.
CREATE INDEX snapshots_claims ON snapshots(account_pk, kind, claimed_at)
  WHERE complete = 0;
