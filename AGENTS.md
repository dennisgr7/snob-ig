# AGENTS.md

Context for working on this project. `CLAUDE.md` points here; this is the only
instruction document. It describes the current state — history and the
reasoning behind individual changes live in the commit messages, not here.

**Detailed reasoning lives in the doc-comment above the code it governs, not
here**: the request pacing in `pace.rs`, the stop conditions in `pager.rs`, the
schema in `store/sql/`, the cookie boundary in `cdp.rs`, the headers in
`client_hints.rs`. Read those before changing any of them — they explain what a
number is for, which is what stops it being changed into something that no
longer does the job it was there to do.

## What this is

`snob`, a terminal tool that tells you who does not follow you back on
Instagram, and tracks changes to your followers and following over time. It
also follows and unfollows one account at a time, shows an account's page the
way Instagram does, and shows and downloads the stories an account has up and
the highlights its profile keeps. Single binary, no runtime. Windows and Linux
on x86_64 and ARM64, macOS on Apple Silicon.

It is a convenience tool for a person's own account, signed in as themselves.
Everything it shows is what the app already shows the same person, read faster
and in a form they can keep. That gap is the whole reason the project exists,
and it bounds the scope — one account, at human scale, doing by command what
the person could do by scrolling.

There is no official API for listing followers — Meta removed it in 2018 — so
this asks the same web API instagram.com asks, with the user's own session
cookie. Automating that is outside Instagram's Terms of Use, as it is for every
tool in this category, and the realistic outcome for a user is that Instagram
asks their account to verify itself. **Most of the design goes into being a
light, well-behaved client** — modest volume, honest requests, and an immediate
stop when the service pushes back. That is the reason for most of the rules
below.

The commands: `login`, `whoami`, `logout`, `purge`; the five lists
(`unfollowers`, `fans`, `friends`, `followers`, `following`) and `scan`;
`profile`, `pfp`, `stories`, `highlights`; `follow` and `unfollow`; and the
`watch` monitor (`watch`, `once`, `diff`, `check`, `setup`, `status`).

## Rules

Standing instructions from the repository owner. They are not up for
re-litigation in a normal change.

- **Branches**: `main` is stable and only good versions land there; `dev` is
  day-to-day work.
- **Everything is written in English**: code, identifiers, comments,
  user-facing strings and documentation. US spelling.
  `crates/snob-core/tests/language.rs` enforces it — and it walks Markdown and
  config files too, this one included. On a false positive, fix its word list
  rather than disabling the test.
- **`cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings`
  before every commit.** Commit messages in English, imperative, no
  conventional-commit prefixes.
- Prefer the compiled, dependency-free option. Native binary, instant start,
  broad platform support — that is the point of the project, not an accident.
- **The command line keeps three conventions**:
  - **One question per command, and `-y` answers it in advance.** A command
    that wanted a second question would be two commands. The flag groups in
    `cli.rs` (`ConsentArgs`, `ProgressArgs`, `StatusOutputArgs`, `FilterArgs`,
    `OutputArgs`, `WalkArgs`) hold this structurally: a flag a command would
    ignore is refused by clap rather than warned about.
  - **`--json` is for a status object; `--format` is for a document.** `whoami`
    and the `watch` subcommands take `--json`; everything that prints a thing a
    person reads takes `--format`, with an enum narrowed to the forms it
    actually has, so an impossible format is refused rather than ignored.
  - **Interactivity is detected, and said beats detected.** Decoration reads
    standard output; whether a question can be asked reads standard input and
    standard error; the full-screen browsers are the default exactly when all
    three streams are a human's terminal and no flag asked for the printed or
    downloaded form. `-i` forces a browser and fails where none can be drawn;
    `--no-interactive` prints. The one copy of the ordering is
    `cli::browse_decision`, fed by all four browse groups; the three-stream
    predicate is `ui::a_human_would_watch_the_listing_scroll_by`. The two
    wizards (`login` with no method flag, `watch setup`) are the written
    exception: a wizard's one job is its questions, and each leaves a
    non-interactive route beside it (`login --paste`; `watch.toml` by hand).
  - `--offline` is the one word for "spend no network". The short-flag space is
    deliberately almost empty — `-y -o -d -i` is the whole list — and a new
    short flag has to argue for itself here first. There is no REPL and no
    global interactive mode.

And the domain rules, which exist because breaking them puts a real account at
risk:

- **Two write operations exist, and no third one may be added.** Follow and
  unfollow, one account per invocation, no bulk mode and no flag that makes
  one. Every write is paid out of its own, far slower budget; every write is
  confirmed before it is sent, with `-y` as the advance answer; a
  `feedback_required` on a write is an action block and earns the twelve-hour
  cooldown. **Nothing may mark a story as seen** — a write dressed as a read.
  The enforcement is structural: `IgClient::post` is the only function that
  sends anything but GET, it cannot be reached without paying the write
  budget, and it takes a `graphql::Mutation` — so a new write is a build error
  until somebody has written the variant. `crates/snob-core/tests/no_seen.rs`
  is the backstop.
- **Never read or decrypt the user's browser cookie store.** The allowed
  route is a browser **we launched against our own profile**, which the user
  logs into themselves and which hands the cookies over through its debugging
  protocol. The boundary is whose profile it is and who hands the data over.
- **On the first 429, `spam:true`, `feedback_required` or
  `challenge_required`: hard stop.** No retry within that run, and the account
  goes into cooldown. When a service says no, the answer is to stop asking.
- **Request pacing is not changed without a documented reason.** The numbers
  and their sources are in `crates/snob-ig/src/pace.rs`, and they have only
  ever been changed to make *fewer* requests.
- **Never walk a real account's lists without the limiter.** Live-API testing
  is done with single, counted requests.
- **No test may touch the real keyring.** Tests use their own service name via
  `SecretStore::with_service`, and `crates/snob-store/tests/keyring.rs` reads
  the source of every crate to check that they do.

## Working on it

```bash
cargo test --workspace --locked            # everything
cargo test -p snob-ig pager                # one module
cargo test -p snob-cli --test cache        # one integration file
cargo test the_first_run_walks_the_list    # one test by name
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo run -p snob-cli -- login --paste     # run it; mind the double dash

cargo test -p snob-cli --features testing --locked   # the binary itself
```

**The `testing` feature is what lets a test drive the program**; everything
else in the suite drives a library. It adds two flags a released binary
contains neither of (`crates/snob-store/tests/sandbox.rs` reads the source to
hold that down): `--sandbox-root` puts every file a run touches under one
directory, forces the file secret backend, and derives a keyring service name
from that root — which is what actually separates a sandbox from the real
credentials. `--ig-base-url` **requires** `--sandbox-root`; that pairing is
the whole safety argument, and `main::wiring` carries the reasoning. Plain
`cargo test --workspace` compiles none of this, so run the `testing` suite
too before pushing — CI runs it as its own step on every platform.

**The Linux CI job runs on this machine too**, in a container: `bash
tools/ci/run.sh` (from Git Bash on Windows). It is the job that differs most
from a developer's host — musl, a real Secret Service keyring behind a session
bus, a static link. First run builds the image from cold; after that a run is
under a minute. `tools/ci/ci.sh` mirrors the remote job and has to be kept in
step with `ci.yml`; both pin the compiler `rust-toolchain.toml` pins, so bump
the two together. The musl target is the container's own architecture.

Remote CI (`.github/workflows/ci.yml`) runs fmt, clippy with `--all-features`,
and the suite on Linux x86_64 and ARM64 (against musl, which is what Linux
users get), Windows and macOS, then builds the five release targets. The Linux
jobs install a keyring daemon so the backend under test is the one users get.

**The browser recorder is `node tools/capture/record.js`** (needs
`playwright`): it launches Chrome with a throwaway profile, the person browses
by hand, and every API call lands redacted in `tools/capture/out/events.jsonl`;
`summarize.js` reads it back. `out/` is gitignored — the redacted log still
carries real names; delete it once what it showed is written down.

Releases are tag-driven (`.github/workflows/release.yml`) and reproducible:
`rust-toolchain.toml` pins an exact compiler, `/Brepro` is in both Windows
tables of `.cargo/config.toml`, the workflow remaps paths through a second
cargo config, and the archives get a deterministic mtime from the tag. Two
builds of one tag produce the same bytes; keep it that way. Packaging lives
under `packaging/` (winget, install scripts), `bucket/` (Scoop) and `Formula/`
(Homebrew).

## Architecture

Four crates, and the line between the first two is the one worth knowing:

| Crate | Responsibility |
|---|---|
| `snob-core` | Domain: models, sets, filters, the diff, the schedule, the webhook signature, the request-budget **interface** (`snob_core::budget`), the clock. **No I/O** |
| `snob-store` | Everything kept on the machine: the SQLite database and its migrations, the platform directories, the keyring, `watch.toml` |
| `snob-ig` | Instagram's web API: endpoints, pagination, pacing, browser headers |
| `snob-cli` | The `snob` binary, plus a library so commands can be tested |

**`snob-ig` depends on `snob-core` and on nothing under it** — that is what
the split is for: the Instagram client compiles no SQLite, no keyring, no TOML
parser. It is a dependency-graph split, not a compile-time one; the dominant
build cost is `snob-cli` either way.

The tool is a session, a database and a request budget. Everything else is a
way of asking those three something:

```
                         app::App
             the only place they are assembled:
        client · store · progress · cancellation · viewer
                            │
     engine::  target · freshness · cooldown · walk · people · watch
                            │
                       commands::*
              orchestration and presentation only
                            │
              output::* · report::* · exit::*
```

Two rules keep it that way:

- **`engine` returns data and where the data came from. It never decides how
  anything looks.** Wording, formats and exit codes live in `commands`,
  `report` and `output`. Every list, crossing and summary comes out of
  `engine::list`.
- **`commands` never builds a client or opens a database. It takes an `App`.**
  A command that assembles its own dependencies can be handed a different
  budget than the rest, which is how rate control gets bypassed by accident.
  Three places are the listed exceptions, each saying why at the line that
  does it: `whoami` builds an `IgClient` directly (it reports on a session
  that may be dead) but still takes its budget through `app::pacer`;
  `commands::watch`'s scheduled loop and `watch::status` call `Store::open`
  directly (a connection is not held across a day-long sleep; `status`
  answers without a session).

The interactive views draw with `ratatui` (`default-features = false`) and
read keys with `crossterm` through `ui::browser::input`; `ui::tui::Tui` is the
one terminal guard, browsers collect receipt lines and print them after it
drops, and every printed form — a pipe, `--format`, `-o`, `--no-interactive` —
never touches any of it.

Storage is SQLite in the platform's **local** data directory (WAL on a synced
directory is a documented corruption cause). Every table is `STRICT`.
Comparison code reads the `usable_snapshots` view, which cannot return an
incomplete capture. The database is shared between processes: a walk in
progress holds a soft lease (`snapshots.claimed_by`/`claimed_at`), taken in
the same statement that finds the row; the invariants and their reasoning are
at `store/snapshots.rs`. Configuration is one file, `watch.toml`, in the
roaming directory, written by `snob watch setup`; **no secret is in it** — the
webhook token and signing key go to the keyring.

## Rules the code enforces, and where

Each of these was once something a caller had to remember. They now live in
the one place that cannot be bypassed; the reasoning is in the doc-comment at
each location.

| Rule | Where it lives |
|---|---|
| Every request is paid for, once per redirect hop | `Pacer::clear_to_send`, inside `IgClient::get_body` |
| Nothing is spent while the account is in cooldown | `Pacer::clear`; `SNOB_IGNORE_COOLDOWN` is the undocumented escape hatch |
| A 429 or push-back puts the account in cooldown | `IgClient::classify_and_record` |
| A refusal is never worked around by asking somewhere else | `IgError::worth_a_second_route` |
| An off-origin or looping redirect aborts the walk | `IgError::OffOrigin`, `IgError::TooManyRedirects` |
| The reported request count is what was really spent | `Pacer::spent`, read by `engine::list` |
| Consent before enumerating someone else, **before** resolving | `engine::ask_consent` |
| An unattended run reads a stranger's lists only on a recorded answer | `Watched::may_run_unattended` |
| Only Instagram's CDN is ever downloaded from | `IgClient::check_downloadable` |
| A name is filtered before anything draws it, whoever it came from | `model::printable`, via `safe_username`/`safe_full_name` and `report::filtered` |
| A name inside a URL or header is encoded, never filtered | `model::in_a_path` |
| An account id, a moment and a count cannot be confused | `snob_core::{Pk, Epoch, EpochMs}` newtypes — the mix-ups are build errors |
| Walking without rate control cannot be written | `ListWalker::new` takes only an `IgClient`, which cannot exist without a `Pacer` |
| The credential cannot be printed, and clears itself when dropped | `secret::Secret` |
| The login browser's debugging protocol has no address, and the browser dies with this process | `pipe::spawn` — `--remote-debugging-pipe` on inherited descriptors, plus the Windows job object; `cdp::kill_on_panic` for panics |
| Two stored lists are crossed only if nothing happened between the walks | `engine::cooldown::check_same_moment` |
| A walk in progress has exactly one writer, and two processes never share one | `snapshots::resumable`/`save_page`/`close`; `is_resumable` asks without claiming |
| A temporal diff never compares an incomplete capture; a first run reports nothing | `watch::Basis::decide` over `usable_snapshots`; `Basis::Baseline` |
| A change is reported once, and only from a list this run verified | `store::watch::Mark`; `engine::watch::refusal` over `Provenance::describes_now` |
| A report is never lost because its delivery failed | `store::watch::commit_report` — queue row and mark are one transaction; retries bounded by attempts and age (`deliveries::still_news_after`), drained once per run (`run_accounts`, `DRAIN_LIMIT`) |
| A queued report goes only to the address it was addressed to | `watch_deliveries.destination`, filtered by `deliveries::due` |
| Credentials and headers configured for one origin are not sent to another | `delivery::plan` |
| The session cannot reach the user's webhook, and neither can a narrowed trust store | `WebhookClient::new` takes no `Session`; `http::plain` takes no trust argument |
| A configured header cannot frame the message or forge the protocol | `webhook::check` |
| What a receiver deduplicates on is unique | `run_id`, `UNIQUE` in the schema |
| There is one spelling of each outcome token | `snob_core::watch::RunOutcome::as_str` |
| Two runs never serve moments inside the minimum gap; a missed moment is taken, not lost; a repeated hour is one run | `schedule::next_after`, `room_for_jitter`, `already_run_at_this_wall_clock` — the schedule reads no clock, `now` is always an argument |
| A file named by a server is created, never written over | `output::create_new` |
| A scratch directory with a guessable name is made, never adopted, and never swept through a planted link | `paths::create_fresh_private_dir`, `create_private_root`, `sweep_old_scratch` |
| The data directory is limited to this account, on both platforms | `paths::create_private_dir` — 0700, or a protected DACL on Windows |
| Uninstalling leaves nothing behind, and never deletes near the root | `AppPaths::owned_dirs`, `paths::is_safe_to_remove`, `secrets::Kind::ALL` via `delete_all` |
| No request is sent after the user asks to stop, and one in flight is given up | `Pacer::clear_to_send` reads the token before reserving; `IgClient::send_or_cancel` |
| A write in flight is the one thing Ctrl+C does not abandon | `IgClient::post` — giving up on a write costs knowing whether it happened |
| A write is never replayed by a redirect, sent without a CSRF token, or resent after an ambiguous answer | `redirect::Policy::none()` on the POST client; `IgError::NoCsrfToken`; `client::worth_rediscovering` |
| Nothing tells anybody you looked at their story | no `Mutation` variant exists for it; `crates/snob-core/tests/no_seen.rs` is the backstop |
| The reader leaving is not an error of this program's | `ui::say!` — `println!` panics on a closed pipe |
| A failure is told in the language the answer was going to be in | `report::Wording`, decided once in `main::wording_for` |

## Running headless

Supported on purpose — a homelab is a first-class place to run this. `snob
login` probes where the session can go before asking for anything and falls
back from the keyring to a file (DPAPI-sealed on Windows, plain JSON at 0600
elsewhere), saying which backend it landed on; `--no-keyring` forces the file.
Prompts write to standard error and read standard input, so redirecting the
results does not turn a question into a refusal; the two prompts that *draw*
(the menu, the masked secret) also need standard error to be a terminal.
`SNOB_LOG` takes `target=level` directives; `--verbose` is `debug` for this
workspace's crates. Down a pipe the output defaults to JSON and the login
method must be given explicitly (`--paste`).

## Settled, so nobody re-opens them

One line each; the fuller reasoning is in the doc-comment at the pointer.

- **`friendships/show_many` is not used**: the real client batches ~12 ids per
  call and the reply carries no `followed_by`, so it cannot answer the
  expensive half of a crossing — and it is a POST, and POST means write here.
  One door with one guard is worth more than fewer requests. The same goes for
  migrating reads to GraphQL.
- **The arithmetic holds**: `unfollowers + friends` is everyone you follow,
  `fans + friends` everyone who follows you. A test asserts it.
- **An incomplete list is never crossed against** — that would be a wrong
  answer, not a partial one. A short *starting* list only warrants a warning.
- **`profile_pic_url_hd` is not the full size**; the 1080 comes from
  `/api/v1/users/{pk}/info/`.
- **Followers are served ~25 per page whatever `count` asks**; the following
  list honors 50. Budget accordingly (`pace.rs` carries the note).
- **There is no useful logged-out mode.** Everything needs a session; that is
  how Instagram built it.
- **The pacing has no published reference behind it and stays where it is** —
  the reasoning is at `pace.rs`, where somebody changing a number will look.
- **The wire signature is not tuned to resemble anything** — headers are sent
  for internal consistency, not disguise (`client_hints.rs`, `headers.rs`).
  One target differs for build reasons alone: Windows ARM64 uses schannel,
  documented in `crates/snob-ig/Cargo.toml`, and `--strict-roots` is refused
  there rather than ignored.
- **The trust store is narrowed only when asked** (`--strict-roots`,
  `--tls-extra-root`); platform verification is the default because only the
  person running it knows whether their middlebox is friend or foe.
  Certificate pinning stays rejected.
- **No biometric verification, on any platform** — any prompt a local process
  of the same user can trigger, that process can satisfy; the attacker that
  matters reads the keyring directly.
- **A challenge's cooldown is not lifted by clearing the challenge** — lifting
  it would trust a login that was never validated, or spend the retry the
  hard-stop rule forbids.
- **The login browser is talked to over a pipe; there is no debugging port.**
  A port was demonstrated to hand the session cookie to any local process.
  `pipe.rs` carries the launch mechanics; `tests/browser_pipe.rs` holds it.
- **`snob purge` deletes the stored data and not the binary**, session first;
  the package manager owns the binary.
- **The Windows credential is `CRED_PERSIST_LOCAL_MACHINE`** — it stays on the
  machine that created it; `secrets.rs::entry_for` carries the experiment.
- **There is no lease over `watch_marks`**: two overlapping runs can report
  one arrival twice, and that costs duplicates but never loses a window. The
  clauses that look like fixes and are not are at `store::watch::set_mark`.
- **Highlights are a command of their own**, not a flag on `stories`: a tray
  level and an items level, numbered the way the listing on screen is.
- **Stories are handed to the system viewer and not drawn in the terminal** —
  terminal graphics protocols do not survive this project's platform set, the
  decoded-image floor is +312 KiB, and no protocol plays video, which a large
  share of stories are.
- **Instagram commands never carry an at sign in examples** — on PowerShell
  `@` is the splatting operator and the argument vanishes before `main` runs;
  `language.rs` fails the build on one. Names are accepted without the sign.
- **Everything is per user, never per directory** — the working directory only
  decides where an export lands without `-o`.

## Known walls

- **A list of tens of thousands does not come back** — Instagram may serve one
  short page with no cursor. `pager::verify_completion` catches it and the set
  commands refuse rather than cross a 0.2% list; there is no cursor to resume.
- **Some business accounts cannot be resolved** — `web_profile_info` answers
  400 over Instagram's own serialization failure. The search fallback in
  `IgClient::web_profile_info` resolves the id but carries no counters, so
  truncation stops being detectable for that account;
  `WebProfileInfo::counters_are_knowable` is the question, and both
  `engine::target` and `watch check` say it out loud.
- **Real behavior on a live 429 has never been provoked on purpose.** The
  handling is verified against a recorded body; `Retry-After` and the load
  headers are logged (`IgClient::note_push_back`) and deliberately not acted
  on until somebody has actually been refused.

## Open, not yet taken

- **`commands::import`** reads Instagram's data export and is tested, but is
  not registered in `cli.rs` — what an import may do once it is in (crossed
  against a live list? stored?) is the unsettled half.
- Two endpoints worth a command someday, both GETs about the viewer's own
  account: `friendships/pending/` (follow requests awaiting an answer) and
  `archive/reel/day_shells/` (the story archive).

## Costs worth knowing

Measured August 2026 and recorded so nobody re-measures them; the dependency
trades that were weighed and declined are in the workspace `Cargo.toml`
comments next to the tables they concern.

- The binary is ~5.2 MB on `aarch64-pc-windows-msvc` (x86_64 about a third
  larger). Bundled SQLite is ~9% of it; the `xlsx` feature ~9% more (it is a
  default feature a source build can leave out); `ratatui` cost +106 KiB.
- The Chromium profile `snob login --browser` creates is ~87 MB, which dwarfs
  all of that — `login` removes it when it is done.
- `clap` without `color` and `zstd` out of `Accept-Encoding` would save real
  bytes and are **kept anyway**, as positions: help in color, and an
  `Accept-Encoding` that is Chrome's character for character.
