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
  and a test checks that they do.

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
| Walking without rate control cannot be written | `ListWalker::new` takes only an `IgClient`, which cannot exist without a `Pacer` |
| The credential cannot be printed, and clears itself when dropped | `secret::Secret`, the type of every credential field |
| A session is never reported gone unless it went | `SecretStore::delete`, which carries the keyring's own answer back |
| Two stored lists are crossed only if nothing happened between the walks | `engine::cooldown::check_same_moment`, over the interval each list covers |
| Uninstalling leaves nothing behind | `AppPaths::owned_dirs`, the only list `purge` reads |
| A directory too near the root is never deleted | `paths::is_safe_to_remove` |
| A temporal diff never compares an incomplete capture, or one against itself | `watch::Basis::decide`, over ids read from `usable_snapshots` |
| A first run reports nothing rather than announcing the whole list as arrivals | `watch::Basis::Baseline`, which has no diff to take out of it |
| A change is reported once: not twice, and not never | `store::watch::Mark` — the receipt, written where the report was made |
| A list nothing verified is neither compared nor marked | `engine::watch::refusal`, over `Provenance::describes_now` |
| An unattended run reads a stranger's lists only on a recorded answer | `Watched::may_run_unattended`; `yes` is set only where a `Consent` exists |
| The session cannot reach the user's webhook | `WebhookClient::new` takes no `Session`, and `snob_ig::http::plain` has no argument for one |
| A report is never lost because its delivery failed | `store::watch::commit_report` — the queue row and the mark are one transaction, in that order |
| Every secret this tool stores is one `purge` removes | `secrets::Kind::ALL`, walked by `SecretStore::delete` |
| Expiring old captures never takes the one a comparison needs | `store::watch::prune`, which excludes what `watch_marks` points at |

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
  of storage without moving anything on; `setup` writes the configuration and
  `status` reads back what has happened. Any run can POST the report to an
  address the user chose. `lost`/`gained` are its words for the temporal diff —
  `unfollowers` is the static set and must never drift to mean `lost`.

  Seven things about it are worth knowing before changing any of it:

  - **It compares against what was last *reported*** — `watch_marks` — and not
    against the previous capture. Those come apart the moment somebody runs
    `snob followers` by hand between two runs, and reading the capture instead
    silently swallows everything that happened before it.
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
    tested with literal timestamps and nothing waits. Its cron parser produces
    the same `Calendar` `--on mon --at 09:00` does — one evaluator, two
    syntaxes, so "Monday at nine" cannot come to mean two things — and that is
    why there is a parser rather than a dependency: the hard part of cron is
    that the two day fields combine with OR when both are restricted and AND
    when either is `*`, and getting it wrong fires on the wrong days silently.
    Missed runs are **folded into one and never replayed**: firing twelve to
    catch up is the burst the pacing exists to prevent, and they would all
    report the same present state anyway.
  - **The webhook is queued before the mark moves, in one transaction.** A
    change that has been reported is one the next run will not find, so if the
    mark moved without the queue row the change would be gone. Delivery is
    therefore at-least-once, which is what `X-Snob-Delivery` is for. Retrying
    here does **not** contradict the hard-stop rule: that rule is about
    Instagram, a service that did not ask to be talked to; this is the user's
    own server. A refusal — anything but 408 and 429 — is still not retried.
    The body is stored as the exact string that was signed, because the
    signature covers bytes and a second rendering could differ.
  - **Retention keeps three things whatever their age**, and each is
    load-bearing: the capture every mark points at (it is the next diff's
    baseline, and taking it costs one silently missed report), the newest
    complete capture of each list (what the cache serves), and any incomplete
    one (a resume somebody may be mid-way through). `store::watch::prune` says
    so; `secure_delete` is on for exactly this.

## Known walls

- **A list of tens of thousands does not come back.** On an account declaring
  21631 followers, Instagram served 39 on the first page and offered no cursor.
  `pager::verdict` catches that — the shortfall is far past what deleted
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
  not.
