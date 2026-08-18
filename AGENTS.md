# AGENTS.md

Context for working on this project. `CLAUDE.md` points here; this is the only
instruction document.

**Detailed reasoning lives in the doc-comment above the code it governs, not
here**: the request pacing in `pace.rs`, the stop conditions in `pager.rs`, the
schema in `store/sql/`, the cookie boundary in `cdp.rs`, the headers in
`client_hints.rs`. Read those before changing any of them — they explain what a
number is for, which is what stops it being changed into something that no
longer does the job it was there to do.

## What this is

`snob`, a terminal tool that tells you who does not follow you back on
Instagram, and tracks changes to your followers and following over time. Single
binary, no runtime. Windows and Linux on x86_64 and ARM64, macOS on Apple
Silicon.

There is no official API for listing followers — Meta removed it in 2018 — so
this uses the private web API with the user's own session cookie. That goes
against Instagram's Terms of Use, and the realistic risk to a user is a
verification checkpoint on their account. **The whole pacing design exists to
lower that probability.** It is the reason for most of the rules below.

## Rules

Standing instructions from the repository owner. They are not up for
re-litigation in a normal change.

- **Branches**: `main` is stable and only good versions land there; `dev` is
  day-to-day work.
- **Everything is written in English**: code, identifiers, comments,
  user-facing strings and documentation. US spelling. A test enforces it —
  `crates/snob-core/tests/language.rs` fails if Spanish turns up. On a false
  positive, fix its word list rather than disabling the test; adding a homograph
  like `red` or `base` turns it into permanent noise and someone switches it off.
- **`cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings`
  before every commit.** Commit messages in English, imperative, no
  conventional-commit prefixes.
- Prefer the compiled, dependency-free option. Native binary, instant start,
  broad platform support — that is the point of the project, not an accident.

And the domain rules, which exist because breaking them puts a real account at
risk:

- **No write operations against Instagram.** `snob` only reads. No follow,
  unfollow, block or remove-follower, ever.
- **Never read or decrypt the user's browser cookie store.** Chrome and Edge on
  Windows have protected it with App-Bound Encryption since v127, and getting
  past that protection is what credential-stealing malware is built to do. This
  project does not go there, and has no reason to. What is allowed, and is the
  main route, is a browser **we launched against our own profile**, which the
  user logs into themselves and which then hands the cookies over through its
  debugging protocol. The boundary is whose profile it is and who hands the data
  over, not whether the cookie happens to be encrypted.
- **On the first 429, `spam:true`, `feedback_required` or `challenge_required`:
  hard stop.** No retry within that run, and the account goes into cooldown.
  When a service says no, the answer is to stop asking — and a retry loop is
  also how a momentary limit becomes a lasting restriction.
- **Request pacing is not changed without a documented reason.** The numbers are
  copied from `InstagramUnfollowers`, which has years of incident-free real use,
  and have only ever been changed to make *fewer* requests. They are in
  `crates/snob-ig/src/pace.rs` with the reasoning attached.
- **Never walk a real account's lists without the limiter.** Live-API testing is
  done with single, counted requests.
- **No test may touch the real keyring.** It belongs to the operating system,
  not the process: a test deleting the real entry wipes the session of whoever
  is developing. Tests use their own service name via `SecretStore::with_service`,
  and `crates/snob-core/tests/keyring.rs` reads the source of every crate to
  check that they do. It reads the source because it has to: an integration test
  compiles the library without `cfg(test)`, so a runtime assertion would be
  blind in exactly the files that matter most — and the guard this replaced
  built a store through a helper that set the service two lines above the
  assertion, so it only ever checked itself.

## Working on it

```bash
cargo test --workspace                     # everything
cargo test -p snob-ig pager                # one module
cargo test -p snob-cli --test cache        # one integration file
cargo test the_first_run_walks_the_list    # one test by name
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p snob-cli -- login --paste     # run it; mind the double dash
```

Without the `--`, cargo keeps the flags instead of passing them through.
`cargo install --path crates/snob-cli` puts a release `snob` on the PATH, but it
has to be repeated after every change; for iterating, `cargo run` is the one.

CI runs fmt, clippy and the suite on Linux, Windows and macOS, then builds five
targets. Two things there are deliberate, and both are the same idea: what is
tested has to be what ships. The Linux job installs a keyring daemon, because
without one the secret store falls back to a file and the backend under test is
not the one users get. And the Linux suite runs against **musl**, which is what
Linux users are given: a glibc build carries the runner's glibc version as a
hard requirement, and built on Ubuntu 24.04 it will not install on Debian 12 or
Ubuntu 22.04.

## Architecture

Three crates:

| Crate | Responsibility |
|---|---|
| `snob-core` | Domain: models, sets, filters, SQLite storage, rate budget, secrets |
| `snob-ig` | Instagram's private API: endpoints, pagination, pacing, browser headers |
| `snob-cli` | The `snob` binary, plus a library so commands can be tested |

The tool is a session, a database and a request budget. Everything else is a way
of asking those three something:

```
                         app::App
             the only place they are assembled:
        client · store · progress · cancellation · viewer
                            │
        ┌───────────────────┼────────────────────┐
        │                   │                    │
     engine::            engine::            engine::         engine::
   target   freshness   cooldown  walk        people           watch
   who?     is stored   what can  page by     who you          what has
            still true? be served page        both know        changed
        └───────────────────┼────────────────────┘
                            │
                       commands::*
              orchestration and presentation only
    lists · sets · scan · pfp · watch · login · logout · purge · whoami
                            │
              output::* · report::* · exit::*
```

Two rules keep it that way, and they are what make a change to `scan` or `pfp`
land in one place:

- **`engine` returns data and where the data came from. It never decides how
  anything looks.** Wording, formats and exit codes live in `commands`,
  `report` and `output`.
- **`commands` never builds a client or opens a database.** It takes an `App`.
  A command that assembles its own dependencies can be handed a different budget
  than the rest, which is how rate control gets bypassed by accident.

Every list, crossing and summary the tool prints comes out of `engine::list`.

Storage is SQLite in the platform's **local** data directory — not the roaming
one, because the database uses WAL and WAL on a synced directory is a documented
cause of corruption. Every table is `STRICT`. The rule that an incomplete
snapshot is never a basis for comparison is structural rather than disciplinary:
comparison code reads the `usable_snapshots` view, which cannot return one.

**The database is shared between processes, and a walk in progress says whose
it is.** `snapshots.claimed_by` and `claimed_at` are a soft lease: `resumable`
takes the claim in the same `UPDATE` that finds the row, so two processes racing
for one partial cannot both win, and `save_page` refreshes it so a long walk
keeps its claim while a dead process loses it after `CLAIM_TTL_SECS`. Before
that existed, one `snob watch` running while somebody typed `snob followers` was
enough for both to continue the same partial — and then either a capture was
closed complete while the other was still paging into it, or the slower one
threw the finished capture back to incomplete, or one process's
`delete_partials` deleted a walk the other was writing to.

Three rules keep the lease honest, and each closed a defect the lease itself
caused. `CLAIM_TTL_SECS` is **strictly shorter** than `RESUME_WINDOW_SECS`:
`claimed_at` is never earlier than `started_at`, so with the two equal "the
claim went stale" and "the partial is still worth resuming" could not both hold
and no interrupted walk could ever be adopted. `save_page` **refuses** a
snapshot this process does not hold, so a walk whose claim was taken stops
instead of interleaving its pages with the adopter's. And a caller that only
wants to *know* whether a walk could be continued asks `is_resumable`, which
does not claim — asking with `resumable` handed the claim back to the process
that was exiting.

Configuration is the other half, and it goes in the **roaming** directory where
it belongs — one file, `watch.toml`, written by `snob watch setup`. Nothing else
writes there and `ensure_dirs` still does not create it, so somebody who never
runs the monitor gets no empty folder. **No secret is in it**: the webhook's
token and signing key go to the keyring, because the file is plain text at a
guessable path and would be in every backup of the home directory.

## Rules the code enforces, and where

Each of these was once something a caller had to remember, and each was
forgotten at least once. They now live in the one place that cannot be bypassed:

| Rule | Where it lives |
|---|---|
| Every request is paid for | `Pacer::clear_to_send`, inside `IgClient::get` |
| A 429 puts the account in cooldown | `IgClient::classify_and_record` |
| The reported request count is what was really spent | `Pacer::spent`, read by `engine::list` |
| Consent before enumerating someone else, **before** resolving | `engine::ask_consent` |
| Only Instagram's CDN is ever downloaded from | `IgClient::check_downloadable` |
| A name is filtered before anything draws it, whoever it came from | `model::printable`, reached through `User::safe_username` / `safe_full_name`, `Viewer::safe_username`, `error::body_excerpt`, `error::missing_message`, `target::label` and `scan::summary_target`. Where a name came from decides whether it can be *trusted*, not whether a control character in it reaches a terminal — so the typed ones go through it too |
| A name inside a URL is encoded, never filtered | `User::profile_url` — filtering removes characters, and a name with one removed is the address of a different account |
| A panic takes the launched browser with it | `cdp::kill_on_panic` |
| Walking without rate control cannot be written | `ListWalker::new` takes only an `IgClient`, which cannot exist without a `Pacer` — and takes its waits from `IgClient::is_live`, so "walk Instagram with no pauses" is not a thing a caller can ask for |
| The credential cannot be printed, and clears itself when dropped | `secret::Secret`, the type of every credential field |
| A session is never reported gone unless it went | `SecretStore::delete`, which carries the keyring's own answer back |
| Two stored lists are crossed only if nothing happened between the walks | `engine::cooldown::check_same_moment`, over the interval each list covers |
| Uninstalling leaves nothing behind | `AppPaths::owned_dirs`, the only list `purge` reads |
| A directory too near the root is never deleted | `paths::is_safe_to_remove` |
| A temporal diff never compares an incomplete capture, or one against itself | `watch::Basis::decide`, over ids read from `usable_snapshots` |
| `snob watch diff` answers without recording the answer | `engine::watch::from_store` takes `&App`, and recording needs the `&mut Store` only `record_from_store` can reach |
| A first run reports nothing rather than announcing the whole list as arrivals | `watch::Basis::Baseline`, which has no diff to take out of it |
| A change is reported once: not twice, and not never | `store::watch::Mark` — the receipt, written where the report was made |
| A list nothing verified is neither compared nor marked | `engine::watch::refusal`, over `Provenance::describes_now` |
| An unattended run reads a stranger's lists only on a recorded answer | `Watched::may_run_unattended`; `yes` is set only where a `Consent` exists |
| The session cannot reach the user's webhook | `WebhookClient::new` takes no `Session`, and `snob_ig::http::plain` has no argument for one |
| A report is never lost because its delivery failed | `store::watch::commit_report` — the queue row and the mark are one transaction, in that order |
| Every secret this tool stores is one `purge` removes | `secrets::Kind::ALL`, walked by `SecretStore::delete_all`, which `purge::execute` calls unconditionally — gating it on there being a session left the monitor's secrets behind after `logout`. `logout` calls `delete`, which takes the session and nothing else |
| Expiring old captures never takes the one a comparison needs | `store::watch::prune`, which excludes what `watch_marks` points at |
| Owed reports are retried by any run, not only by one that had news | `run_one` and `once` drain the queue once per run, after every account, bounded by `DRAIN_LIMIT` — `deliver` deliberately does not. `run_one` takes an `App` rather than opening one, so a test can watch it happen |
| A queued report can only go to the address it was addressed to | `watch_deliveries.destination`, which `deliveries::due` filters on |
| A credential set up for one host is not sent to another | `delivery_from` compares origins before attaching the keyring token or the file's headers |
| A rename is found wherever it happened, and reported once | `engine::watch::compare` reads every list this run verified, from the account's one cursor, and deduplicates by `pk` |
| The rename cursor moves only when a window was read | `commit_report` takes `Option<i64>`; a run that compared nothing passes `None` |
| A resolved account is reused only for the account it was resolved for | `App::resolved_target` keys on the question, not only the answer |
| Two runs never happen inside the minimum gap, whatever moved them | `schedule::next_after`, which starts its search at `last + MIN_GAP_SECS` — so the answer is both far enough away and on the calendar |
| What a receiver deduplicates on is unique | `run_id`, which is `UNIQUE` — not the rowid, which SQLite reuses |
| A configured header cannot be one the request could not carry | `webhook::check`, which builds every name and value before accepting the address |
| A configured header cannot frame the message or forge the protocol | `webhook::check` refuses `Content-Length` and the rest of the framing set, and the whole `X-Snob-` prefix |
| A walk in progress has exactly one writer | `snapshots::save_page` **and** `snapshots::close` both refuse a snapshot this process does not hold; `is_resumable` asks without claiming. `close` had no guard and releases the claim, so a process whose lease had gone stale killed the walk that adopted it |
| Two processes never walk into one capture | `snapshots::resumable`, which takes the claim in the statement that finds the row |
| A finished capture is never unfinished again | `snapshots::close`, whose `WHERE` carries `complete = 0` |
| No request is sent after the user asks it to stop | `Pacer::clear_to_send`, which reads the token before it reserves — it was read only inside the wait, so with nothing owed a canceled run kept sending |
| A push-back the body could not be read from is still a push-back | `IgClient::get` classifies from the status it already has when the body fails, rather than letting the read failure become a retryable network error |
| A credential is sent only to the address it was stored for | `plan`, for the token **and** the signing key, treating an absent or unparseable configured origin as a different destination |
| A calendar moment that went by is taken, not lost | `schedule::next_after` looks back from `now` to the floor before it looks forward |
| Jitter cannot cost the next run | `Schedule::room_for_jitter` — the gap less the floor between two runs, applied to the default as well as to `--jitter` |
| One wall-clock moment is one run, in a zone that repeats an hour | `schedule::already_run_at_this_wall_clock`, asked in the direction ambiguity exists in |
| The rename cursor moves only over what this run could read | `engine::watch::compare` advances it only when every list the account has a capture of was accounted for, baselines included |
| A rename filed mid-comparison waits for the next window | `store::watch::renames_since` bounds above by the `head` the caller read first |
| Everything a scheduled run needs is checked while somebody is there | `snob watch check`, through `engine::check` — which takes `&App`, so it cannot record, and walks no list |
| Whether the monitor is working is an answer, not a reading | `watch_setup::health`, in `status`'s output and in its exit code |

## Running headless

Supported on purpose — a homelab is a first-class place to run this.

`snob login` probes where the session can go **before** asking for anything,
and on a machine with no keyring it uses the protected file instead of refusing.
Secret Service needs a desktop session, so a server, a container or WSL has
none; that is normal rather than an error. The fallback is never silent: the
command says which backend it landed on, because a session stored somewhere
less protected than the user expected is its own kind of failure. `--no-keyring`
still forces the file directly.

Everything else already works without a terminal: `prompt_secret` reads a plain
line when standard input is not a TTY, the progress bar hides itself, `table`
becomes one name per line down a pipe, and the output format defaults to JSON.
The login **method** must be given explicitly there (`--paste`), since there is
no menu to show.

**Redirecting the results does not turn a question into a refusal.** Every
prompt is written to standard error and answered on standard input, so
`ui::can_be_asked` asks about standard input alone and nothing gates on standard
output: `snob scan someone | jq` reaches the consent question and can answer it.
The one exception is `ui::can_show_a_menu`, which also needs standard **error**
to be a terminal, because that is where `dialoguer` draws — not standard output,
which no prompt here touches.

Two judgement calls worth understanding before touching them:

- **`truncated()` in `pager.rs`** decides whether a short list means the counter
  lied — it includes deleted accounts — or Instagram stopped serving pages. The
  threshold is measured against what was **declared**, not against what was
  walked. A shortfall of more than half is truncation at any size.
- **`declares_failure()` in `error.rs`** parses the body rather than searching
  it. Looking for `"status"` and `"fail"` anywhere in the text meant a follower
  named `fail` stopped the walk.

## Settled, so nobody re-opens them

- **The arithmetic holds**: `unfollowers + friends` is everyone you follow, and
  `fans + friends` everyone who follows you. A test asserts it.
- **If the list being crossed against is incomplete, no result is given.** In
  `unfollowers`, someone who does follow you but was never read would appear as
  not following you — not a partial result but a wrong one, and the failure
  tools of this kind carry. The *starting* list being short only warrants a
  warning: results are missing, but the ones shown are true.
- **`profile_pic_url_hd` is not the full size.** It hands back a URL telling the
  CDN to downscale to 320x320, and the signature covers that instruction. The
  1080x1080 comes from `/api/v1/users/{pk}/info/`.
- **`count=50` is accepted**, the cursor advances without repeating, and
  resuming works. Followers are served ~25 per page anyway; budget accordingly.
- **There is no useful logged-out mode.** Signed out, `web_profile_info` answers
  429 on the very first request from the edge, username-to-id cannot be resolved
  by any surviving route, and the one endpoint that does answer gives a stub
  with no counters and a 150x150 picture whose URL is signed for that size.
  Everything needs a session; that is how Instagram has built it, not a gap
  here.
- **The TLS stack is chosen for portability, and is not to be tuned to imitate
  anything.** There would be nothing to imitate in any case: Chrome has
  randomized its ClientHello extension order since v110. Trying would mean
  leaving `rustls`, and the clean static cross-compilation with it, and would
  buy nothing — what determines whether Instagram throttles an account is, in
  order, the address the requests come from, how many there are, and how fast.
  **One target picks a different backend, and that is purely a build concern**:
  Windows on ARM64 uses schannel, because neither of
  rustls's crypto providers builds there without LLVM — the pre-generated
  assembly is GNU syntax and both shell out to clang. Everything else, x86_64
  Windows included, stays on `rustls`. The reasoning, why it is deliberately
  not widened to all of Windows, and what it costs is in
  `crates/snob-ig/Cargo.toml` next to the two dependency tables. The choice is
  about which crypto backend compiles on that target and nothing else, and
  `http2` stays mandatory on both.
- **Headers are sent so that the request is internally consistent, and for no
  other reason.** Instagram answers `Vary: Sec-Fetch-Site, Sec-Fetch-Mode`, so
  those are sent; `Accept` is `*/*` because that is what `fetch()` sends when
  the page sets nothing; `sec-ch-ua` is computed from the version rather than
  hardcoded, including the order of its three entries; no `Origin` on a
  same-origin GET, which the Fetch standard omits there. The stored User-Agent
  follows the major version of a browser actually installed on the machine, at
  most daily, and never when the user pinned one.
- **`@someone` never reaches us on PowerShell.** `@` is the splatting operator,
  so the argument is gone before `main` runs and the tool answers about the
  user's own account. Nothing can detect it from here — do not write unquoted
  `@name` in examples aimed at PowerShell users.
- **Everything is per user, never per directory.** The session, the database and
  the cache come from `directories`, so running `snob` from two folders is one
  session and one cache. The only thing the working directory decides is where
  an export lands without `-o`. The header of `paths.rs` says so; the tests fix
  it.
- **No biometric verification, on any platform.** Investigated in August 2026
  and rejected on the merits, not on difficulty. The principle: any prompt a
  local process of the same user can trigger, that same process can satisfy by
  asking the user — and the attacker that matters never runs `snob` at all: it
  reads the keyring directly, with the same permissions the user has. Concretely:
  Windows `KeyCredentialManager` gives an unpackaged binary **no per-application
  boundary** (Microsoft's own answer: without an AppContainer it scopes to the
  user account, and a second executable can open the same credential); the
  console has no usable window to parent a dialog to under ConPTY; macOS would
  need an App ID entitlement, a provisioning profile and notarization, which
  ends the single static binary; and Linux polkit needs a root-installed policy
  file and falls back to the login password anyway. Two more nails: whoever sits
  at an unlocked session has the browser logged into Instagram one click away,
  where they can *write*, and the v2 monitor is a background service that cannot
  prompt anybody. What is worth doing instead — keep the cookie out of the
  database and out of logs — is already done.
- **A challenge's cooldown is not lifted by clearing the challenge, and that is
  the accepted cost.** Someone who opens the link and passes the check in thirty
  seconds still waits out the half hour: `whoami`, `pfp` and every walk refuse
  until it lifts, and `login` stores a session without validating it because
  during a cooldown not even that one request is spent. Two ways out were
  considered and both were rejected. Letting a successful `login` clear it means
  trusting a login that was never validated — the cooldown would be lifted by
  the one command that cannot tell whether the account is still flagged.
  Spending the validation request during the cooldown to find out is the retry
  the rule above forbids, aimed at an account Instagram has just flagged, which
  is precisely how a checkpoint becomes something longer. The escalation is
  shared across causes for the same reason: a challenge arriving within a day of
  a 429 is evidence the account is in worse shape, not better, so it starts at
  the escalated length rather than at its own. `SNOB_IGNORE_COOLDOWN` exists for
  the person who is certain, and stays undocumented so it is not the first thing
  reached for.
- **`snob purge` deletes the stored data and not the binary.** No package
  manager can do the first half: `winget uninstall`, `brew uninstall` and
  `apt remove` take away files the package owns, and the session, the database
  and the browser profile are in the user's own directories, which it never
  owned. Leaving a live session cookie on a machine whose owner has just
  uninstalled the tool is the failure the command exists to prevent, so the
  session is deleted **first**, before any directory that could turn out to be
  locked. The other half is the binary, and it is left alone deliberately: a
  process cannot reliably delete its own executable on Windows, and one that
  managed it would leave `winget` or `apt` reporting a version that is no longer
  installed. The command prints the path and stops there.
- **The Windows credential is written as `CRED_PERSIST_ENTERPRISE`**, which
  Microsoft documents as visible on other computers for accounts with roamable
  state. `keyring`'s `Entry` does not expose the modifier that would make it
  local, and reaching it means depending on `keyring-core` directly and keeping
  its version in lockstep or the shared default store breaks at run time. The
  reasoning, and what would have to change, is in `secrets.rs::entry_for`.

## State

Every command works and has been exercised against the live API. Two things are
deliberately unfinished:

- **`commands::import`** reads Instagram's data export correctly and is tested,
  but is **not registered in `cli.rs`**. What is unsettled is not the parsing but
  what an import should be allowed to do once it is in — whether it can be
  crossed against a live list, and whether it belongs in the store at all.
  Shipping the subcommand would answer those by accident.
- **The monitor** (`snob watch`) is built. Bare, it stays up and runs on a
  schedule; `once` does one run and exits; `diff` answers the same question out
  of storage without moving anything on; `check` says whether a scheduled run
  would work; `setup` writes the configuration and `status` reads back what has
  happened and whether it is healthy. Any run can POST the report to an address
  the user chose. `lost`/`gained` are its words for the temporal diff —
  `unfollowers` is the static set and must never drift to mean `lost`.

  **`once` is the scheduled run without the loop**, and reads the same
  `watch.toml` for the same accounts. It read the file for the webhook address
  and built the watched set from the command line alone, so an `[[account]]`
  added by `setup` was never walked by the mode the README puts on a timer.

  **`check` is the preflight**, and it is what makes an unattended run not the
  first thing tried: the schedule through the evaluator that decides it, the
  session and which backend the secret store landed on, that each account
  resolves and may be read unattended, its counters — so the truncation wall is
  found before six hours of walking rather than after — and the webhook, by
  posting one `watch.preflight` message with the configured headers and
  signature. It takes `&App`, so it cannot record, and it walks no list: it is
  meant to be safe to point a monitoring system at, and a probe that walks two
  lists every time it is polled is worse than no probe. The baseline offer is
  therefore in `setup`, not here. Warnings are not failures; only what would
  stop a run reaches the exit code.

  Eleven things about it are worth knowing before changing any of it:

  - **It compares against what was last *reported*** — `watch_marks` — and not
    against the previous capture. Those come apart the moment somebody runs
    `snob followers` by hand between two runs, and reading the capture instead
    silently swallows everything that happened before it.
  - **The rename window has one cursor per account, and it moves only when the
    window was read.** It was a column on `watch_marks`, so an account had one
    per list and they could disagree — reading the older re-announced renames a
    refused list had already had sent, reading the newer would have skipped the
    gap for anybody in only one list. And it advanced for every list a run did
    not refuse, including a run that compared nothing, which stepped over
    anything filed in between. The window covers every list the run *verified*,
    which includes an unchanged one: a rename moves nobody in or out of a list.
  - **Both of its windows are bounded by ids, not timestamps**: `snapshots.id`
    for the captures, `username_history.id` for the renames. `taken_at` and
    `changed_at` are in whole seconds, so two events inside one second are
    neither clearly before a report nor clearly after it, and a timestamp bound
    there either announces something twice or loses it for good. Three separate
    defects came from this before the ids went in.
  - **`Provenance::describes_now()` is what decides whether a run may conclude
    anything.** A list served during a cooldown, after a failed poll, or under
    `--cache` was not verified by this run, so it is neither compared nor
    marked — and not marking it is the half that matters, because a mark moved
    over an unreported change loses it permanently.
  - **A run with nothing to report costs one request, not two.**
    `web_profile_info` answers with both counters and `App::remember_counters`
    keeps them, so the second list asks nothing. A test asserts it.
  - **The schedule reads no clock.** `watch::schedule` takes `now` as an
    argument everywhere, the shape `rate_budget::decide` set, so all of it is
    tested with literal timestamps and nothing waits. One evaluator, two
    syntaxes — but they are not the same reading: cron crosses its hour and
    minute fields, which is what `0,30 9,21 * * *` means, while `--at
    09:00,21:30` is two exact moments and not the four that crossing gives. A
    `Calendar` from `--at` carries the pairs; one from cron does not. What the
    two must agree on is the moments they fire at, and a test compares those
    rather than the fields. The parser is written here rather than depended on
    because the hard part of cron is that the two day fields combine with OR
    when both are restricted and AND when either is `*`, and getting it wrong
    fires on the wrong days silently.
    Missed runs are **folded into one and never replayed**: firing twelve to
    catch up is the burst the pacing exists to prevent, and they would all
    report the same present state anyway. How many were missed is counted
    against whatever decides the schedule, not against the interval — with a
    calendar, dividing elapsed time by `--every` gives a number that is simply
    false, and it is printed at the user.
  - **`MIN_GAP_SECS` binds every syntax, and it binds the runs rather than the
    grid.** `Schedule::cron` did not validate at all and `validated` only looked
    at the interval, so the tool refused `--every 5m` while accepting
    `--cron "*/5 * * * *"` and `--at 09:00,09:05`, which run exactly as often;
    `Calendar::tightest_gap` closes that, counting the wrap around midnight only
    when two allowed days can actually be consecutive. But validating the grid is
    not enough, because jitter moves each run off it and the next moment is
    computed from where the run really landed. So the floor lives in
    `next_after`'s **search start**, not in a check after the answer: put after
    it, a calendar never reached it, and when it was reached it answered
    `last + MIN_GAP_SECS` — an instant the calendar forbids.
  - **A moment that went by is taken, and jitter has to fit in the room the
    floor leaves.** The search only ever looked forward and works in whole
    minutes, so its answer was always the *ceiling* minute of `now`: `Due::Now`
    for a calendar needed the clock to read `:00` to the second, which
    `store::now()` does about once in nine hundred times. A machine powered on
    at 09:05 with `--at 09:00` waited a day, a laptop that suspended across the
    moment lost the run, and the calendar branch of `missed_since` was
    unreachable code. `next_after` looks back from `now` to the floor first, and
    `missed_since` takes one off the end because the moment being served is
    itself in the past.

    The floor is also why the jitter has a ceiling of its own. It is measured
    from where a run really landed, so every second of jitter comes out of the
    next gap: a run at `m` pushed to `m + s` reaches `m + gap` only when
    `s <= gap - step`. Nothing subtracted the step and the calendar default was
    a flat fifteen minutes — exactly `MIN_GAP_SECS` — so `--cron "*/15 * * * *"`
    ran every half hour while the banner printed what was typed.
    `Schedule::room_for_jitter` is the whole of it, and it takes the interval as
    the step when there is one: `--every 2w --on mon` has a weekly grid and a
    fortnightly floor, so its room is zero.
  - **An hour a fall-back repeats is one run, not two.** `--at 01:30` in a zone
    that puts its clocks back names two instants an hour apart, and the floor is
    fifteen minutes. Two comments claimed this was handled and neither was:
    `timestamp_opt(..).single()` goes from an instant to a local time, a
    direction that is never ambiguous, and the test that credited `MIN_GAP_SECS`
    used a `FixedOffset`, which has no transitions. The question is asked in the
    other direction now, and the second showing is refused only when the first
    was at or before the last run — a machine switched off through the first
    still runs at the second.
  - **The webhook is queued before the mark moves, in one transaction.** A
    change that has been reported is one the next run will not find, so if the
    mark moved without the queue row the change would be gone. Delivery is
    therefore at-least-once, which is what `X-Snob-Delivery` is for. Retrying
    here does **not** contradict the hard-stop rule: that rule is about
    Instagram, a service that did not ask to be talked to; this is the user's
    own server. **Every answer the far end gives is retried**, 4xx included: a
    4xx was treated as final, and because the mark has already moved by then,
    one 404 from a workflow that happened not to be registered threw away the
    only copy of a set of arrivals and departures. What bounds the retrying is
    the attempt count and the age, not a guess about a status. The body is
    stored as the exact string that was signed, because the signature covers
    bytes and a second rendering could differ — and `watch_deliveries.destination`
    records where it was addressed, so a run pointed somewhere else by
    `--webhook` cannot flush the backlog to a host nobody configured.
  - **Retention keeps three things whatever their age**, and each is
    load-bearing: the capture every mark points at (it is the next diff's
    baseline, and taking it costs one silently missed report), the newest
    complete capture of each list (what the cache serves), and any incomplete
    one (a resume somebody may be mid-way through). `store::watch::prune` says
    so; `secure_delete` is on for exactly this.

## Known walls

- **A list of tens of thousands does not come back.** On an account declaring
  21631 followers, Instagram served 39 on the first page and offered no cursor.
  `pager::verify_completion` catches that — the shortfall is far past what deleted
  accounts explain — and `scan` and the set commands refuse rather than cross a
  list that is 0.2% of the account. This walk cannot be resumed either: the
  pagination ended, so there is no cursor to store. That is a property of
  **this** wall and not of `Truncated`, which also arrives from four guards
  that stop in the middle of the pagination with a cursor saved — so
  `try_again_advice` asks the store what was kept rather than reading the stop
  reason as an answer.
  Whether the limit is the account, the session or the endpoint is not known;
  what is known is that the tool reports it instead of answering wrongly.
- **Real behavior on a 429 has never been provoked on purpose.** The handling is
  verified against a recorded body. Everything downstream of it — the cooldown,
  the hard stop, the exit code — is tested; the classification of a live one is
  not. The cooldown half only became true in August 2026: every test that drove
  a throttling body through a real client hung `Pacer::unlimited()` off it,
  whose `start_cooldown` answers `Ok(0)` and forgets, so the recording could be
  deleted whole with the suite still green. There is a budget double that
  remembers now.
