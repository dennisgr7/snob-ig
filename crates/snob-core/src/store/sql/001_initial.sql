-- Initial snob-ig schema.
--
-- Every timestamp is UTC epoch. Columns without a suffix are in seconds, to
-- match Session::created_at. Those ending in _ms are milliseconds and only the
-- rate control uses them, because it needs sub-second precision. The unit goes
-- in the name so that mixing them is not a silent mistake.
--
-- Every table is STRICT. Without it SQLite would accept text in an INTEGER
-- column thanks to its loose typing, and a list comparison against a counter
-- turned into a string produces phantom differences.

CREATE TABLE users (
  pk          INTEGER PRIMARY KEY,   -- Instagram id, never the username
  username    TEXT    NOT NULL,
  full_name   TEXT,
  is_verified INTEGER,               -- may be NULL: the API does not always send it
  is_private  INTEGER,
  pfp_url     TEXT,
  first_seen  INTEGER NOT NULL,
  last_seen   INTEGER NOT NULL
) STRICT;

CREATE INDEX users_username ON users(username);

-- Usernames change. Recording it gives us, for free, an event no tool of this
-- kind offers: "so-and-so now goes by something else".
CREATE TABLE username_history (
  id         INTEGER PRIMARY KEY,
  pk         INTEGER NOT NULL REFERENCES users(pk) ON DELETE CASCADE,
  username   TEXT    NOT NULL,
  changed_at INTEGER NOT NULL
) STRICT;

CREATE INDEX username_history_pk ON username_history(pk, changed_at DESC);

-- A tracked account. It does not store the name: that lives in users, and
-- having two places where the name lives contradicts the rule that identity is
-- the numeric id.
CREATE TABLE accounts (
  pk              INTEGER PRIMARY KEY REFERENCES users(pk) ON DELETE CASCADE,
  is_self         INTEGER NOT NULL DEFAULT 0 CHECK (is_self IN (0, 1)),
  added_at        INTEGER NOT NULL,
  -- Last counter poll. This is what the cache policy rests on: if the number
  -- has not moved, there is no need to walk the whole list.
  polled_at       INTEGER,
  follower_count  INTEGER,
  following_count INTEGER
) STRICT;

CREATE TABLE snapshots (
  id             INTEGER PRIMARY KEY,
  account_pk     INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
  kind           TEXT    NOT NULL CHECK (kind IN ('followers', 'following')),
  source         TEXT    NOT NULL CHECK (source IN ('live', 'dyi')),
  started_at     INTEGER NOT NULL,
  taken_at       INTEGER,            -- NULL while the walk is still open
  complete       INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0, 1)),
  member_count   INTEGER NOT NULL DEFAULT 0,  -- what was actually stored
  declared_count INTEGER,            -- what Instagram claimed; it can lie
  pages          INTEGER NOT NULL DEFAULT 0,
  requests       INTEGER NOT NULL DEFAULT 0,
  next_cursor    TEXT,               -- pending; NULL once finished
  resumes        INTEGER NOT NULL DEFAULT 0,
  -- Mirrors StopReason::as_str(). A test walks every variant and closes a
  -- snapshot with it, so enum and schema cannot drift apart.
  stopped_by     TEXT CHECK (stopped_by IS NULL OR stopped_by IN
                   ('completed', 'canceled', 'page_limit', 'truncated',
                    'rate_limit', 'network', 'session_invalid'))
) STRICT;

CREATE INDEX snapshots_lookup
  ON snapshots(account_pk, kind, complete, taken_at DESC);

-- WITHOUT ROWID because the row is essentially its own key. The composite key
-- also deduplicates on its own, which is needed: Instagram's cursor pagination
-- repeats accounts across pages.
CREATE TABLE snapshot_members (
  snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
  user_pk     INTEGER NOT NULL REFERENCES users(pk)     ON DELETE CASCADE,
  ordinal     INTEGER NOT NULL,   -- walk order: Instagram serves recent first
  PRIMARY KEY (snapshot_id, user_pk)
) STRICT, WITHOUT ROWID;

CREATE INDEX snapshot_members_user ON snapshot_members(user_pk);

-- The rule "an incomplete snapshot is never a basis for comparison" stops being
-- discipline and becomes structure. Comparison code reads from here and never
-- from the table, so it cannot get it wrong even if it tries.
CREATE VIEW usable_snapshots AS
  SELECT * FROM snapshots WHERE complete = 1 AND taken_at IS NOT NULL;

-- Rate control by GCRA: an exact token bucket expressed as a single integer,
-- with integer arithmetic and no floating-point drift. Shared between the CLI
-- and the v2 service so two invocations do not spend without noticing each
-- other.
CREATE TABLE rate_budget (
  bucket        TEXT PRIMARY KEY,   -- 'pace' | 'daily' | 'writes'
  tat_ms        INTEGER NOT NULL,   -- theoretical arrival time allowed
  emission_ms   INTEGER NOT NULL,   -- what one request costs in time
  burst_ms      INTEGER NOT NULL,   -- burst tolerance
  updated_at_ms INTEGER NOT NULL    -- to detect the clock jumping backwards
) STRICT;

-- A cooldown covers the whole session, not one bucket: a 429 is caused by the
-- account, not by whichever bucket we happened to be spending from.
CREATE TABLE cooldowns (
  scope     TEXT PRIMARY KEY,       -- 'session'
  until_ms  INTEGER NOT NULL,
  set_at_ms INTEGER NOT NULL,
  reason    TEXT    NOT NULL,
  strikes   INTEGER NOT NULL DEFAULT 1
) STRICT;

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
