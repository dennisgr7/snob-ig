# Changelog

## Unreleased

**snob follows, unfollows, and shows you stories.** The first release that
changes anything on Instagram at all, and the rule it replaces is worth reading
before the features: `snob` used to write nothing, deliberately, because a read
tool asks a service for what it already shows you while a write tool acts on
your behalf. That rule was lifted for two verbs and no others, and what took its
place is a regime rather than permission.

- **`snob follow` and `snob unfollow`**, one account per command. Both ask
  before they send; `-y` answers in advance. Both come out of a request budget
  of their own — one action every fifteen minutes, at most three in a row —
  which is separate from the one reads come out of, so an exhausted write budget
  never holds up a walk and a walk never spends a write. **There is no bulk
  mode and no flag that makes one.** What strains a service is the burst rather
  than the daily total, and the follow-then-unfollow churn that automating a
  list makes easy is a growth-hacking trick rather than housekeeping, and
  outside what this tool is for.
- A write needs a session with a CSRF token. `snob login --browser` already
  captured one; a session created by pasting a sessionid does not have one, and
  the two commands now **refuse before spending anything** rather than finding
  out from a 403 that would read as an expired session.
  `snob login --paste --csrftoken <token>` is the way to add one on a machine
  with no browser to launch.
- **`snob stories`** lists what an account has up: what each story is, when it
  went up and how long it has left. `--download 2` saves one by the number the
  listing printed, `--all` saves all of them, and `--interactive` is a list you
  move through with the arrow keys — Enter hands the story to the system viewer,
  `D` keeps a copy. It is built on what was already in the binary rather than on
  a terminal-UI framework, so it costs nothing to carry.
- **Nothing tells anybody you looked at their story.** Instagram registers a
  view through a separate request, snob does not make it, and a test reads the
  source of all three crates on every build so that adding one is a failing
  build rather than a code review somebody has to catch.
- A write follows no redirect. Instagram redirecting a POST would mean doing the
  thing twice, and the HTTP client replays the method and the body on a 307.

## 0.2.0 — 2026-08-21

**`snob watch` says what has changed since the last time it looked.** The
monitor, in two commands. Who has started following you and who has stopped,
who you have followed and unfollowed, and who now goes by a different name —
that last one out of a history the tool has been keeping on every walk since
the first release and had never shown anybody.

- **`snob watch`** stays up and runs on a schedule you choose. `--every 6h`, or
  `--on mon,thu --at 09:00`, or a cron expression if you already have one
  written: `--cron "0 9 * * 1,4"`. The two combine, so `--every 2w --on mon` is
  one Monday in every two — the interval cron cannot express. Times are your
  local ones. Each run is pushed a little later than its due moment so the
  walks do not start on the same second every day; `--jitter 0` turns that off.
  `snob watch --json >> events.ndjson` is a complete way to use it with no
  webhook at all.
- **`snob watch once`** does one run and exits, for a cron entry or a systemd
  timer. It reads your counters and only walks a list if its counter moved, so
  a run with nothing to report costs a single request. Whatever it reports, it
  does not report again.
- **`snob watch diff`** answers the same question out of what is already
  stored, without touching the network and without moving anything on. Ask
  twice, get the same answer.

All three take `--json`, and down a pipe `snob watch diff --json | jq` works
without being told to.

**And it can POST each report somewhere.** `--webhook https://n8n.local/webhook/snob`,
with `--header "Authorization: Bearer ..."` for an endpoint that wants one and
`--sign-with` to have the body signed — HMAC-SHA256, sent as
`X-Snob-Signature: sha256=...`, the shape you already have a snippet for. The
JSON is built for an automation to branch on: `schema` is the version of the
shape, at the top where a receiver can check it before reading anything else —
it moves when a field is removed or changes meaning, never when one is added, so
a workflow written against schema 1 keeps working. Every message carries it: the
report, the heartbeat, the preflight and each line of the `--json` stream.
`counts.followers_lost` is there next to the arrays so a condition does not have
to reach into one, `event` tells a report from a heartbeat without looking
inside, and `run.looked` says whether this run could see at all — which empty
arrays cannot. `run.lists` says it per
list, so a list a cooldown refused is told apart from one that was read and had
not moved: both leave zeros in `counts`, and only one of them means "nothing
happened". Every message carries the same `run` object — including the
`watch.preflight` one `snob watch check` posts, so a receiver branches on it
exactly as it branches on the rest — with the `run.id` to deduplicate on and
`run.at` for when it happened. `--json` writes that same object on every line,
one per tick, and a tick that failed leaves a line too, with `error.code` from
the same vocabulary the exit table uses.

Nothing is sent when nothing changed, unless `--heartbeat` asks for it, so every
message that arrives means something. A report that cannot be delivered is
queued and tried again with a growing wait, and it is queued **before** the
monitor moves on, so a receiver that was restarting does not cost you the
change. It carries `X-Snob-Delivery` for exactly that reason: delivery is
at-least-once, so a repeat is possible and the receiver can drop it. Every
answer your endpoint gives is worth another try — a 404 from an n8n workflow
that was not registered, a 403 from a proxy reloading, a 401 from a token that
expired — because the alternative is losing the only copy of that change. One
that keeps failing is given up on after a day rather than retried forever, and
the line you get then says the change was not reported rather than implying it
is still queued. A day, because that is how long a receiver can plausibly be
down over a weekend and still be worth waiting for.

A queued report remembers the address it was made for, so pointing a run
somewhere else with `--webhook` to see what the payload looks like does not
flush your backlog, or your stored token, to that address.

`http://` is refused unless the address is on your own network, because the
report carries account names and any token you configured travels with it. That
is checked when you give the address, not six hours later.

**`snob watch setup` writes it all down once**, so a systemd unit or a Task
Scheduler entry needs nothing but `snob watch`. It asks the questions, writes a
`watch.toml` you can edit afterwards, and puts any token or signing key in the
system keyring rather than in that file — which is what lets the unit file hold
nothing sensitive. It asks how far a run may be pushed past its moment as well,
and writes it: that setting was readable everywhere and written nowhere, so the
only way to set it was to edit the file by hand. It is refused if it is larger
than the gap between two runs can spare, rather than being quietly clamped at
every start, and on a schedule with no room to spare it says so instead of
asking. When it records an answer for somebody else's account it names the
address too, because that one answer authorizes two things: reading their lists,
and sending their names to whatever is on the other end.

`snob watch status` reads back what is configured, when each list was last
reported on, and what is still owed — split in two, because a report addressed
to somewhere this configuration no longer names is not one the next run will
try. It says so for each, and `--json` carries both counts under `deliveries`
beside the total it always had. `snob purge` takes the new
keyring entries with it, like everything else.

**`snob watch check` says whether a scheduled run would work**, before one runs
unattended at three in the morning. The schedule through the evaluator that
actually decides it, with the next three moments in your local time; the
session, and which store the credential landed in; each watched account —
that it resolves, that an unattended run may read it, and its counters, so an
account past the size this tool can walk is found before six hours of walking
rather than after; and the webhook, by posting one `watch.preflight` message to
it with your headers and your signature. `--no-webhook` leaves the receiver
alone and checks everything else — the address and every configured header still
go through the same validation, and the report still names the address, so
"nothing was posted" is told apart from "there is nowhere to post". It writes
nothing and walks no list,
and it exits non-zero when something would stop a run — which makes it usable as
a probe rather than only as something to read. Poll it hourly rather than by the
minute: it costs one request for the session and one per account, charged to the
same daily budget the walks draw on, and a probe that drains that budget causes
the condition it is watching for. `snob watch status` gained the same verdict, over what it
already knew.

`snob watch setup` now finishes by running that check, and then offers to take
the first capture, saying what it costs in requests and minutes. The first
scheduled run otherwise lays the baseline down and reports nothing, which reads
as broken when you have just set the thing up.

The monitor expires captures older than a month, so a database does not grow
without end on a six-hour schedule. What it never takes: the capture the next
comparison measures against, the newest of each list, and any interrupted walk
that could still be resumed.

Four things it will not do. The first run on an account has nothing to compare
against, so it reports nothing and says so rather than announcing your whole
follower list as new arrivals. A list served from storage during a cooldown, or
when the check failed, is not compared against anything — nothing established
that it is still true — and the monitor stays where it was, so what happened is
reported by the next run that can see. A walk that came back short is never a
basis either: the accounts missing from it would read as people who left. And a
scheduled run that was down for a day does not fire the runs it missed when it
comes back: it runs once and says how many it is standing in for, because there
is only one present state and nothing to catch up on.

`--every` will not go below fifteen minutes, and says why — and neither will a
calendar or a cron expression that amounts to the same thing, nor two runs that
jitter happened to push together.

Seven things here change what a script sees, so they come first:

- **`snob purge` with no terminal to ask at now exits 130 instead of 1**, and
  says so on standard error rather than standard output. 130 is what the
  exit-code table has always documented for a confirmation that was not given;
  `purge` was the one command answering with the generic failure instead.
- **`snob logout` and `snob purge` can now exit 1 where they exited 0.** A
  keyring that refuses to delete the credential used to be reported as a
  session that went. It is now reported as one that did not, because a tool
  whose promise is to leave no live cookie behind must not claim to have kept
  it when it has not.
- **`scan --format csv` and `--format xlsx` have four more columns.** Where each
  list came from and when it was taken were in the JSON and missing from the
  other two. They are appended, after the existing ones, and the header row
  names them — but anything appending rows to a sheet written by an older
  version will find them wider.
- **A run with standard error redirected now receives the warnings, pauses and
  countdowns it used to lose.** They were suppressed along with the progress
  bar, which meant `snob followers 2>log` recorded nothing about why a walk
  stopped.
- **`snob unfollowers`, `fans` and `friends` now exit 0 when `--max-pages`
  stopped the list**, which is what the exit-code table has always promised and
  what `snob following --max-pages 2` already did for the identical stop
  reason. The result was written and the shortfall warned about either way; only
  the code disagreed.
- **`--refresh` now walks even when the counter poll is what failed.** It used
  to answer with whatever capture was on disk, from any month, with exit 0 and
  nothing on screen saying which. Without the flag a failed poll is still a
  reason to reuse rather than to walk.
- **`snob scan --json` reports when the people-you-both-know line was
  captured.** `followed_by` gains `taken_at`, in the epoch seconds the rest of
  the object uses. The line itself now says the date as well, and is left out
  when the stored list behind it is older than `--max-age` — it is the one
  figure in that answer no flag refreshes.

And the corrections worth knowing about:

- A watched account named with a leading at sign is the same account as one
  named without, whether it was typed at `snob watch` or written into
  `watch.toml`. The two spellings used to be two accounts: a name typed with the
  sign matched no recorded answer, so a monitor that had been set up correctly
  refused to start over a consent it had just read out of the file, and one
  written into the file with the sign was asked about at Instagram with the sign
  still on it and reported as an account that does not exist.
- Jitter is measured against the day the run is really due on, in your zone, and
  not against a nominal 86400 seconds. On the one day a year a zone springs
  forward, a large configured jitter could push a run past the next moment on
  its own grid — which then read as one run for two days and none missed — or
  onto a day the calendar does not name at all.
- Old captures, settled deliveries and the run log are expired by a run that
  cannot start as well as by one that runs. A session that had been logged out,
  a webhook address the checks refuse, or a hand-edited schedule the scheduler
  will not build each end a run before anything is opened — and while that
  lasted nothing was ever expired: the queue went on counting reports no run
  would ever hand back, with `snob watch status` promising the next one would
  try them.

- A crossing of two stored lists compares the gap between the two walks rather
  than between the moments they finished, so `unfollowers --cache` on an
  account large enough for a walk to take twenty minutes no longer refuses its
  own cache every time.
- A redirect is held to the same rule at every hop, and the exception that let
  the test server be reached over plain `http` can no longer be reached in a
  release build.
- `purge` deletes only directories it owns. The check that a parent was ours
  compared the folder name and not much else, which on Linux and macOS put
  `~/.config` and `~/Library/Application Support` within reach of it.
- The stored username is no longer overwritten with the numeric id when a
  command resolves an account it has seen before, which also stops a rename
  that never happened being filed in the history the monitor will read.
- A challenge now puts the account in cooldown, as the documentation has always
  said it does. It has its own shorter length: waiting is not what clears one.
- Names that came off Instagram are filtered before anything draws them on
  every output path, not most of them, and a name inside a profile link is
  percent-encoded rather than pasted in. A csv or spreadsheet field starting
  with `=`, `+`, `-` or `@` is defused even when whitespace hides it.
- The session is kept out of freed memory: the cookie header is built in a
  buffer that clears itself, and the plaintext of the protected file no longer
  outlives the read.
- The wait before the next request counts down instead of insisting it has
  fifteen seconds left, and the progress bar keeps drawing through the second
  half of a crossing.
- `snob login --paste 2> log` no longer hangs. The masked prompt draws to
  standard error and was gated on standard input alone, so with standard error
  redirected it spun with nothing on screen and never read what was pasted.
- An `--exclude-list` written by PowerShell's `Set-Content -Encoding UTF8`, or
  by Notepad, now hides the first name in it as well. The byte-order mark those
  add is not whitespace, so the first line matched nobody while every line after
  it worked — which is what made the file look like it was being read.
- A username with a soft hyphen, a Mongolian vowel separator or one of the
  Hangul fillers in it can no longer render as a name that already exists. Those
  are letters as far as any character property goes, and blank on screen.
- A long username no longer breaks the table it is in. Its profile link was
  measured by one parser and wrapped by another, so a narrow terminal cut the
  address in half and swallowed the borders and the rows below it; the table
  falls back to plain names and one line saying where profiles live.
- A name that cannot go in a header verbatim gets an answer about the name
  rather than four retried requests that never leave the machine.
- A duration too large to compare against a timestamp is refused where it is
  written, instead of turning `--max-age` from "never expire" into "walk the
  list again, every time".
- The drill-down hints `snob scan` prints no longer carry an at sign. On
  PowerShell an unquoted one is eaten before the command runs, and the answer
  comes back about your own account with exit 0.
- "Followed by @ana, @luis and @eva and 2 others" reads as one list now.
- **`snob login --browser` cleans up after itself.** The profile it drives the
  browser in is 87 MB and holds a second copy of your live session, and it was
  kept for ever. It is removed once the cookies have been captured;
  `--keep-profile` keeps it if you would rather the next login skipped the
  Instagram form.
- Dates name the month — `Aug 3 at 14:12` rather than `03/08 at 14:12`, which
  half the English-speaking world reads as the eighth of March.
- A signing secret has to be at least 32 characters. A short one can be guessed
  offline by anybody who has been sent one signed report.
- On Windows the binary no longer needs the Visual C++ redistributable. It used
  to fail in the loader, before `main`, on a machine that had never had it.
- `snob whoami --json` answers with an object when no session is stored. It
  printed nothing at all, while a session that had *died* produced a full
  object with `alive: false` — so the two states share exit code 3 and one of
  them handed a parser nothing to read.
- Exit code 2 is documented. It is what any unparseable command line returns,
  and it was in neither the help nor the README, so a script branching on the
  documented set met an undocumented code on the commonest mistake there is.
- The three `X-Snob-*` headers every POST carries are documented:
  `X-Snob-Event` to route on, `X-Snob-Delivery` to deduplicate on — the same
  value as `run.id`, and stable across every retry of one report — and
  `X-Snob-Attempt`.

## 0.1.1 — 2026-08-05

- The Linux builds are statically linked against musl. The 0.1.0 ones were
  linked against the glibc of the machine that built them, which made them
  refuse to install on Debian 12 and Ubuntu 22.04; the `.deb` said as much with
  `Depends: libc6 (>= 2.39)`, and the tarballs failed later and less clearly.
  These carry no such requirement and run on any distribution.
- Installable with Scoop, with Homebrew, from a `.deb`, or from an install
  script that verifies the published SHA256 before putting anything on the
  `PATH`. The README lists all of them.

## 0.1.0 — 2026-08-05

First release. The essentials, and no more.

- `unfollowers`, `fans` and `friends` cross an account's two lists;
  `followers` and `following` print one on its own; `scan` gives the whole
  picture at once, and opens with the people you both know when it is pointed
  at somebody else.
- `pfp` downloads a profile picture at full size, which the web page does not
  serve.
- `login` takes the session either from a browser it launches with a profile of
  its own or from a pasted `sessionid`; your password is never involved. The
  session is kept in the system keyring, or in a DPAPI-protected file on a
  machine that has no keyring.
- Filters by verified, private and picture-less; output as a terminal table,
  JSON, NDJSON, CSV, XLSX or Markdown, to standard output or a file.
- Walked lists are stored in a local SQLite database and reused rather than
  walked again, and an interrupted walk resumes where it stopped.
- Every request is paced and paid for from a persisted budget. A 429, a
  `feedback_required` or a challenge stops the run and puts the account into
  cooldown with no retry.
- A list that could not be read in full is never crossed against another one:
  the result would be wrong rather than partial.
- `purge` removes the session, the database and the browser profile — the
  things no package manager can reach — before the binary is uninstalled.

Windows, Linux and macOS, on x86_64 and ARM64.

Not built yet: watching an account over time and reporting what changed, and
reading Instagram's own data export instead of the API.
