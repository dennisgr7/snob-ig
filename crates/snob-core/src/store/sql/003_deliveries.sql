-- Reports waiting to reach the address the user chose.
--
-- Same conventions as 001: UTC epoch in seconds, STRICT.
--
-- Why there is a queue at all. A change that is detected and not delivered is
-- never detected again: the next run compares against the mark this one moved,
-- and the arrival it found is by then old news. So the report is made durable
-- here, in the same transaction that moves the mark. If the process dies
-- between writing this row and sending it, the receiver gets the report twice
-- -- which is why every one carries an id it can deduplicate on. Losing a
-- change is the failure that matters; sending one twice is not.
CREATE TABLE watch_deliveries (
  id           INTEGER PRIMARY KEY,
  -- Also inside the body, so the receiver can match the two and drop a repeat.
  run_id       TEXT    NOT NULL UNIQUE,
  account_pk   INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
  created_at   INTEGER NOT NULL,
  -- The exact bytes that were signed, stored rather than the events they were
  -- built from.
  --
  -- A retry must send the same string. Rendering the events again could space
  -- or order the JSON differently, and the signature covers bytes -- so the
  -- second attempt would be rejected after the first was accepted, which reads
  -- as an intermittent network fault and is not one.
  body         TEXT    NOT NULL,
  attempts     INTEGER NOT NULL DEFAULT 0,
  -- When it may next be tried. NULL once it is settled either way.
  next_try_at  INTEGER,
  settled_at   INTEGER,
  last_status  INTEGER,          -- the HTTP code, when there was one
  last_error   TEXT,
  state        TEXT    NOT NULL DEFAULT 'pending'
                 CHECK (state IN ('pending', 'delivered', 'expired'))
) STRICT;

-- Partial, because the only question ever asked of this table in the hot path
-- is "what is due now", and settled rows are the ones that accumulate.
CREATE INDEX watch_deliveries_due
  ON watch_deliveries(next_try_at) WHERE next_try_at IS NOT NULL;
