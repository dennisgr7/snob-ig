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
Instagram, and tracks changes to your followers and following over time. It also
follows and unfollows one account at a time, shows an account's page the way
Instagram does, and shows and downloads the stories an account has up. Single binary, no runtime. Windows and Linux on x86_64 and
ARM64, macOS on Apple Silicon.

It is a convenience tool for a person's own account, signed in as themselves.
Everything it shows is what the app already shows the same person, read faster
and in a form they can keep: two lists crossed against each other, a picture at
the size the CDN actually holds, what changed since last week. The app has never
put a button on any of it. That gap is the whole reason the project exists, and
it bounds the scope — one account, at human scale, doing by command what the
person could do by scrolling.

There is no official API for listing followers — Meta removed it in 2018 — so
this asks the same web API instagram.com asks, with the user's own session
cookie. Automating that is outside Instagram's Terms of Use, as it is for every
tool in this category, and the realistic outcome for a user is that Instagram
asks their account to verify itself. **Most of the design goes into being a
light, well-behaved client** — modest volume, honest requests, and an immediate
stop when the service pushes back. That is the reason for most of the rules
below.

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
- **The command line keeps three conventions**, settled in August 2026 after
  reading every flag against every other:
  - **One question per command, and `-y` answers it in advance.** Consent to
    enumerate somebody else's lists, confirmation of a write, confirmation of
    a purge — each command asks at most one thing, and `-y` is always the
    answer to that one thing. A command that wanted a second question would
    need a second flag, which is the signal it is two commands.
  - **`--json` is for a status object; `--format` is for a document.**
    `whoami` and every `watch` subcommand emit one object about the state of
    things and take `--json`. Everything that prints a thing a person reads —
    a list, a summary, a profile, a story listing — takes `--format`, with an
    enum narrowed to the forms it actually has (`StoryFormat`,
    `ProfileFormat`), so `--format xlsx` on five stories is refused rather
    than accepted and ignored. A new command takes one or the other, never a
    third spelling.
  - **Two kinds of interactivity are detected and one is asked for.** Whether
    the output is decorated (a table, color, hyperlinks, hints) is read from
    standard output being a terminal; whether a question can be asked is read
    from standard input and standard error being one. Taking the terminal
    over — the story browser — is never inferred from either: it is `-i`, per
    command, and a command without `-i` prints and exits however it was run.
    There is no REPL and no global interactive mode; a second command language
    would need a second parser for every flag above.
  The list commands' options are composed from three `flatten` groups —
  `FilterArgs`, `OutputArgs`, `WalkArgs` — so a command takes the groups it
  acts on and a flag it would ignore is refused by clap rather than warned
  about. `scan` is the list options without `--limit`.

And the domain rules, which exist because breaking them puts a real account at
risk:

- **Two write operations exist, and no third one may be added.** `snob` may
  follow an account and unfollow an account. That is the whole list. No block,
  no remove-follower, no like, no comment, no message, and **nothing that marks
  a story as seen** — which is a write dressed as a read, because it puts the
  user in somebody else's viewer list.
  This rule used to say that `snob` only reads, and it was right about the
  distinction: reading asks a service for what it already shows you, while
  writing acts on your behalf, and the tool spent its first two versions doing
  none of the second. The rule was lifted deliberately, for those two verbs
  and no others, and what replaced it is not permission but a regime:
  - **One account per invocation.** There is no bulk mode and there is no flag
    that makes one. What strains a service is not the daily total but the burst
    — a hundred unfollows in half an hour is a script however unremarkable the
    day's count is — and the follow-then-unfollow churn that `snob unfollowers`
    makes so easy to automate is a growth-hacking trick rather than
    housekeeping, and outside what this tool is for. Piping a list of names into
    a loop is the user's business; handing them the loop is ours, and we do
    not.
  - **Every write is paid for out of its own budget**, the `writes` bucket in
    `rate_budget`, which is far slower than the one reads come out of. The
    numbers and their sources are in `crates/snob-ig/src/pace.rs`.
  - **Every write is confirmed before it is sent**, on standard error, with
    `-y` as the way to answer in advance. Nobody loses a follow to a typo.
  - **A `feedback_required` on a write is an action block**, not a throttle, and
    it earns the twelve-hour cooldown rather than the two-hour one.
  The enforcement is structural, not a promise: writing goes through
  `IgClient::post`, which cannot be reached without paying the write budget and
  which takes a `graphql::Mutation` rather than a path — so the set of writes
  this program can make is the set of variants that enum has, and adding one is
  a build error rather than a code review. A test reads the source of all three
  crates as a backstop, for the mistake that arrives before the request does.
- **Never read or decrypt the user's browser cookie store.** Chrome and Edge on
  Windows have protected it with App-Bound Encryption since v127. That
  protection is there on purpose and this project stays on the outside of it,
  which costs nothing because there is a legitimate route to the same cookie.
  What is allowed, and is the main route, is a browser **we launched against our
  own profile**, which the user logs into themselves in front of them and which
  then hands the cookies over through its debugging protocol. The boundary is
  whose profile it is and who hands the data over, not whether the cookie
  happens to be encrypted.
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
  and `crates/snob-store/tests/keyring.rs` reads the source of every crate to
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

cargo test -p snob-cli --features testing            # the binary itself
```

Without the `--`, cargo keeps the flags instead of passing them through.

**The `testing` feature is what lets a test drive the program.** Everything else
in the suite drives a library: `watch_tick.rs` runs the comparison through
`engine::watch::tick`, `watch_webhook.rs` runs the outbox through
`WebhookClient`. Nothing ran `main` — not the dispatch, not `AppPaths::discover`,
not the secret store choosing a backend, not the exit code a timer reads. The
feature adds two flags for that and **a released binary contains neither them nor
the code they reach**, which `crates/snob-store/tests/sandbox.rs` reads the source
to hold down. `--sandbox-root` puts every file a run touches under one directory,
forces the file backend, **and gives the run a keyring service name derived from
that root** — `crates/snob-store/tests/keyring.rs`'s rule, applied to the binary.
The third of those is what actually separates a sandbox from the real
credentials, and forcing the backend was mistaken for it for a while.
`SecretStore` reaches the keyring on every backend and has to: `save` clears the
stale entry, because `load` reads the keyring first and would otherwise go on
serving a session the file has replaced, and the watch token and signing key
have no file form at all. Under the real service name that meant a sandbox
`login` deleted the developer's session, a sandbox `purge` took the webhook
secrets with it, and a sandbox that had not logged in yet loaded the **real**
cookie — which, with the redirect flag set, is the one outcome this seam exists
to make impossible. `main::wiring` assigns the namespace and carries the
reasoning; its own tests hold it down.
`--ig-base-url` **requires** `--sandbox-root`, and that pairing is the whole
safety argument: a redirected client can only carry a session out of a store
inside that root, so the real stored session is not reachable from a redirected
run. Nothing about it is loopback-only, deliberately — `IgClient::is_live`
decides whether the pace is real by address, so a proxy on `127.0.0.1` forwarding
to Instagram would be a test server by address and Instagram by content, and the
walk it produced would be a real account read with no waits between pages.
`cargo install --path crates/snob-cli` puts a release `snob` on the PATH, but it
has to be repeated after every change; for iterating, `cargo run` is the one.

**The Linux job runs on this machine too**, in a container: `bash
ci/local/run.sh`. It is the job that differs most from a developer's host —
musl, a real Secret Service keyring behind a session bus, a static link — and
the one worth not waiting ten minutes on. The first run builds the image and
compiles from cold; after that the two named volumes keep the registry and the
target directory, and a run is under a minute. `ci/local/ci.sh` is the four
commands the remote job runs and has to be kept in step with `ci.yml`; the
image pins the compiler `rust-toolchain.toml` pins, so bump the two together.
The Windows and macOS jobs are not there because a container cannot run either;
the host is the Windows job.

CI runs fmt, clippy and the suite on Linux, Windows and macOS, then builds five
targets. Three things there are deliberate, and all three are the same idea:
what is tested has to be what ships. The Linux job installs a keyring daemon,
because without one the secret store falls back to a file and the backend under
test is not the one users get. **Every platform runs the `testing` feature as
its own step, and clippy lints with `--all-features`**: `--workspace` alone
compiles neither `tests/sandbox.rs` nor the sandbox seam in `main.rs`, so for a
while the only tests that drive the binary ran on no gate at all — including the
one holding down the keyring namespace, whose absence had already destroyed a
real session twice. The step takes no `--test` filter on purpose, because that
spelling builds only the integration target and skips the bin's own unit tests. And the Linux suite runs against **musl**, which is what
Linux users are given: a glibc build carries the runner's glibc version as a
hard requirement, and built on Ubuntu 24.04 it will not install on Debian 12 or
Ubuntu 22.04.

## Architecture

Four crates, and the line between the first two is the one worth knowing:

| Crate | Responsibility |
|---|---|
| `snob-core` | Domain: models, sets, filters, the diff, the schedule, the webhook signature, the request-budget **interface**, and the clock. **No I/O** |
| `snob-store` | Everything kept on the machine: the SQLite database and its migrations, the platform directories, the keyring, `watch.toml` |
| `snob-ig` | Instagram's web API: endpoints, pagination, pacing, browser headers |
| `snob-cli` | The `snob` binary, plus a library so commands can be tested |

**`snob-ig` depends on `snob-core` and on nothing under it.** That is what the
split is for. `snob-core` used to be both halves, so an HTTP client compiled
SQLite, three keyring backends, `directories` and a TOML parser it never called
— `cargo tree -i rusqlite` showed the edge — and a change to the database schema
recompiled the Instagram client. The trait `RateBudget` was the specific cause:
`Pacer` cannot be built without one, and it lived inside `store::rate_budget`
next to its SQLite implementation. The port is `snob_core::budget` now and the
adapter stayed behind.

Two things it does **not** buy, written down so nobody reopens it expecting them.
It is not a compile-time win: measured in this tree, touching
`store/snapshots.rs` rebuilt all three crates in 6.22 s against 5.08 s for
touching `report.rs` alone, because the dominant cost is `snob-cli` and that
depends on both halves either way. And it costs one thing — `RateBudgetError`
lives in `snob-core` while `rusqlite::Error` does not, so the two `From` impls
that used to let a `?` convert on its own are not writable by either crate. The
orphan rule is why `store::rate_budget::budget_err` exists.

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
    lists · sets · scan · profile · pfp · stories · watch · login · logout · purge · whoami
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

  **Three places do, and each says why at the line that does it.** They are
  written down here because a rule with unlisted exceptions is a rule the next
  reader stops believing:

  - `whoami` builds an `IgClient` directly, because it reports on a session that
    may be dead and `App::open` refuses to exist without a live one. It goes
    through `app::pacer` for the budget, so the thing the rule protects — one
    budget for the whole process — is not what is being skipped.
  - `commands::watch`'s scheduled loop and `watch::status` call `Store::open`
    directly. The loop does it so the SQLite connection is not held open across
    a sleep that can be a day long; `status` does it because it answers without
    a session at all, which is the case somebody runs it in.

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

The two of them ask one predicate — `RESUMABLE`, one string — and differ only in
a bound value: `resumable` binds `this_process()`, so a run may take back its own
claim after a restart inside the window, while `is_resumable` binds `None`, so
`claimed_by = NULL` is NULL and the clause goes inert. That asymmetry is
load-bearing in one direction only. Bind `this_process()` on the asking side and
the advice on screen promises a continuation the next invocation refuses, which
is the defect that made every interrupted walk start again at page one.

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
| Every request is paid for | `Pacer::clear_to_send`, inside `IgClient::get_body` — **once per hop**, because the redirects it follows are requests too. Leaving them to reqwest's policy meant a chain of two reported one request and sent three, unpaced and uncounted, and the count is what the user is shown |
| A redirect this client will not follow stops the walk | `IgError::OffOrigin` and `IgError::TooManyRedirects`, whose reaction is `Abort`. They are variants of their own for that reason alone: when the refusal was reqwest's it arrived as `Network`, whose reaction is `Retry`, and the pager sent the same impossible request three more times with the session on it |
| A 429 puts the account in cooldown | `IgClient::classify_and_record` |
| A refusal is never worked around by asking somewhere else | `IgError::worth_a_second_route`, which is true only for a 4xx that is none of the refusals — a 429, an action block, a challenge, an expired session, a cancel and a 404 all answer `false`. It is what stops `web_profile_info`'s search fallback becoming a retry against an endpoint that has just said no |
| The reported request count is what was really spent | `Pacer::spent`, read by `engine::list` |
| Consent before enumerating someone else, **before** resolving | `engine::ask_consent` |
| Only Instagram's CDN is ever downloaded from | `IgClient::check_downloadable` |
| A name is filtered before anything draws it, whoever it came from | `model::printable`, reached through `User::safe_username` / `safe_full_name`, `Viewer::safe_username`, `app::target_label`, `error::body_excerpt`, `error::missing_message`, `target::label` and `scan::summary_target` — and, for everything a failure prints, through `report::filtered`, which every branch of `print_error` goes through including the one that carries no label. Where a name came from decides whether it can be *trusted*, not whether a control character in it reaches a terminal — so the typed ones go through it too. `printable` covers the invisibles that are `Cf` **and** the four Hangul fillers, which are ordinary letters by category and blank by rendering |
| A name inside a URL or a header is encoded, never filtered | `model::in_a_path`, used by `User::profile_url` and by the `Referer` the Instagram client sends — filtering removes characters, and a name with one removed is the address of a different account. A header value cannot hold a byte below 0x20 at all, so an unencoded name there produces no request rather than a wrong one, and reqwest reports that as `Network`, which the pager retries |
| A panic takes the launched browser with it | `cdp::kill_on_panic` |
| An account id cannot be mistaken for a count or a moment | `snob_core::Pk`, a newtype over `u64` with no `Deref` and no `From<Pk> for u64` — so `pk + 1`, `pk == followers` and `params![pk]` are all build errors rather than things a reader has to catch. `ListOutcome` is where it came closest: `account_pk` sits beside `started_at` and `taken_at`, and its doc-comment records the transposition that shape was built to dodge. Nothing moved on the wire or on disk — `Display` writes the bare digits and `#[serde(transparent)]` keeps every JSON `pk` the number it was, with `snob_ig::model::flexible_pk` still reading the `"123"` Instagram sometimes sends instead. SQLite goes on through `store::pk_to_sql` / `pk_from_sql`, which stay two functions rather than a `ToSql` impl because `snob-core` may not compile SQLite and the orphan rule bars `snob-store` from writing one for a type it does not own |
| A moment cannot be mistaken for a count, and seconds cannot be mistaken for milliseconds | `snob_core::{Epoch, EpochMs}`, the same shape as `Pk` one row up and for the same reason: every timestamp in the tool was an `i64`, and the only thing telling the two units apart was a hand-applied `_ms` on the name — `cooldown_ends_at_secs(until_ms)` was a convention, not a rule. No `Deref`, no `From<Epoch> for i64`, no `Add<i64>` and no conversion between the two beyond `EpochMs::to_epoch` and `Epoch::to_ms`, so `taken_at + 1`, `until_ms - taken_at` and `params![taken_at]` are build errors. What a moment does take is a length of time: `Epoch + Duration`, `Epoch - Duration`, and `Epoch - Epoch` for the **signed** seconds between two of them, all saturating — a capture dated in the future is a clock that went backwards, and `engine::cooldown::gap_between` subtracts in both orders precisely so the sign decides. Nothing moved on the wire or on disk: `#[serde(transparent)]` and `Display` keep `at`, `taken_at`, `last_reported_at` and `run.at` the bare numbers they were, and SQLite keeps its `INTEGER` columns, read and written through `get()` / `new()` at the row the way an account id goes through `store::pk_to_sql` — and no `ToSql` impl, barred by the same crate split and the same orphan rule |
| The login browser's debugging protocol has no address | `pipe::spawn`, which starts it with `--remote-debugging-pipe` on two inherited descriptors. There is no port to guess and no `DevToolsActivePort` to read, which is what the demonstrated read of the session cookie needed |
| The launched browser dies with this process however this process dies | the job object in `pipe::spawn`, carrying `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The child is created suspended and joined to the job before it is resumed, so it never exists outside one — and this is the only one of the three exits that no code of ours can reach, because nothing runs when snob is killed from outside |
| Walking without rate control cannot be written | `ListWalker::new` takes only an `IgClient`, which cannot exist without a `Pacer` — and takes its waits from `IgClient::is_live`, so "walk Instagram with no pauses" is not a thing a caller can ask for |
| The credential cannot be printed, and clears itself when dropped | `secret::Secret`, the type of every credential field |
| A session is never reported gone unless it went | `SecretStore::delete`, which carries the keyring's own answer back |
| Two stored lists are crossed only if nothing happened between the walks | `engine::cooldown::check_same_moment`, over the interval each list covers |
| Uninstalling leaves nothing behind | `AppPaths::owned_dirs`, the only list `purge` reads |
| A directory too near the root is never deleted | `paths::is_safe_to_remove` |
| The data directory is limited to this account, on both platforms | `paths::create_private_dir` — 0700 on Unix, and on Windows a DACL of its own with `PROTECTED_DACL_SECURITY_INFORMATION`, which is the flag that stops the profile's inherited entries applying. It used to do nothing at all on Windows and say in a comment that `%LOCALAPPDATA%` already limited access; on a machine whose profile ACL is not the default it does not, and the database is the whole follower history in the clear. A failure is `PathError::NotPrivate` rather than a warning, for the same reason the Unix half has always been an error |
| A temporal diff never compares an incomplete capture, or one against itself | `watch::Basis::decide`, over ids read from `usable_snapshots` |
| `snob watch diff` answers without recording the answer | `engine::watch::from_store` takes `&App`, and recording needs the `&mut Store` only `record_from_store` can reach |
| A first run reports nothing rather than announcing the whole list as arrivals | `watch::Basis::Baseline`, which has no diff to take out of it |
| A change is reported once: not twice, and not never | `store::watch::Mark` — the receipt, written where the report was made |
| A list nothing verified is neither compared nor marked | `engine::watch::refusal`, over `Provenance::describes_now` |
| An unattended run reads a stranger's lists only on a recorded answer | `Watched::may_run_unattended`; `yes` is set only where a `Consent` exists |
| The session cannot reach the user's webhook | `WebhookClient::new` takes no `Session`, and `snob_ig::http::plain` has no argument for one |
| Narrowing the trust store cannot reach the user's webhook | the same shape, one field over: `http::builder` takes the trust as an argument and `plain`/`plain_direct` pass `Trust::Platform` with no parameter for anything else. A private CA in front of somebody's own receiver is legitimate, and the client that would be narrowed carries no session to protect |
| A report is never lost because its delivery failed | `store::watch::commit_report` — the queue row and the mark are one transaction, in that order |
| There is one spelling of each outcome token | `snob_core::watch::RunOutcome::as_str`, with `from_token` derived from it over `RunOutcome::ALL` rather than written as a second match. It is in `snob-core` because both ends of the token need it and neither can name the other: `ExitCode::as_str` is this one, reached through `ExitCode::outcome`, and `watch_runs.outcome` is a `RecordedOutcome` parsed once in the store's row mapping rather than an `Option<String>` each reader compares its own literals against. `watch::status::health` matched the literals inline: respell one there and every recorded cooldown falls through to the failing arm, so `status` exits 1 for a monitor that will resume on its own — and the fixture those tests build their rows from spelled the same literals, so the suite would have moved with the defect. A token this build does not write reads back as `RecordedOutcome::Unknown`, which is equal to no outcome at all, so it reaches the failing arm deliberately rather than by matching nothing |
| A report is too old to be news in one place | `deliveries::still_news_after`, which `due`, `failed` and `expire_stale` all read. It was three hand-written comparisons and they disagreed at exactly a day: `due` handed the report out as news, `failed` gave up on it, and `prune` left it `pending` for ever |
| The row retention keeps is the row `status` shows | the `newest_run` view: `prune` exempts what it selects and `last_runs` reads from it, so `started_at DESC, id DESC` cannot mean one thing in one place and another in the other. `started_at` is whole seconds and two runs of one account inside a second are reachable — `snob watch once` beside a scheduled tick consults no schedule and no gap — so a tie-break added to one site alone has `prune` delete the row `status` is displaying |
| Every secret this tool stores is one `purge` removes | `secrets::Kind::ALL`, walked by `SecretStore::delete_all`, which `purge::execute` calls unconditionally — gating it on there being a session left the monitor's secrets behind after `logout`. `logout` calls `delete`, which takes the session and nothing else |
| Expiring old captures never takes the one a comparison needs | `store::watch::prune`, which excludes what `watch_marks` points at |
| Owed reports are retried by any run, not only by one that had news | `run_accounts` drains the queue once per run, after every account and whatever the accounts did, bounded by `DRAIN_LIMIT` — `deliver` deliberately does not. Both modes go through it, which is what stops one of the two keeping the rule and the other not. It takes an `App` rather than opening one, so a test can watch it happen |
| A queued report can only go to the address it was addressed to | `watch_deliveries.destination`, which `deliveries::due` filters on |
| Headers configured for one host are not sent to another | `plan`, in the same origin comparison that gates the token — the file's `[webhook.headers]` are dropped when `--webhook` names a different origin, and a warning says which address they were configured for rather than the run sending them silently |
| A rename is found wherever it happened, and reported once | `engine::watch::compare` reads every list this run verified, from the account's one cursor, and deduplicates by `pk` |
| The rename cursor moves only when a window was read | `commit_report` takes `Option<i64>`; a run that compared nothing passes `None` |
| A resolved account is reused only for the account it was resolved for | `App::resolved_target` keys on the question, not only the answer |
| Two runs never serve moments inside the minimum gap, whatever moved them | `schedule::next_after`, which starts its search at the floor — so the answer is both far enough away and on the calendar. **With a grid the floor counts from the moment the last run served, not from the row it wrote**: `watch_runs.started_at` is stamped after both lists are walked, so measuring from it pushed the floor past the next grid moment and dropped it, and `*/15` ran every half hour on an expression the tool had accepted. The cost is deliberate and written down at the code: two runs can be closer in wall clock than the gap by however long the walk took |
| What a receiver deduplicates on is unique | `run_id`, which is `UNIQUE` — not the rowid, which SQLite reuses |
| A configured header cannot be one the request could not carry | `webhook::check`, which builds every name and value before accepting the address |
| A configured header cannot frame the message or forge the protocol | `webhook::check` refuses `Content-Length` and the rest of the framing set, and the whole `X-Snob-` prefix |
| A walk in progress has exactly one writer | `snapshots::save_page` **and** `snapshots::close` both refuse a snapshot this process does not hold, and both **say so** with `ClaimTaken`; `is_resumable` asks without claiming. `close` had no guard and releases the claim, so a process whose lease had gone stale killed the walk that adopted it — and once guarded it dropped the row count and answered `Ok`, so that process reported a finished capture it did not own and the monitor's mark pointed at a row `usable_snapshots` never returns |
| Two processes never walk into one capture | `snapshots::resumable`, which takes the claim in the statement that finds the row |
| A finished capture is never unfinished again | `snapshots::close`, whose `WHERE` carries `complete = 0` |
| No request is sent after the user asks it to stop | `Pacer::clear_to_send`, which reads the token before it reserves — it was read only inside the wait, so with nothing owed a canceled run kept sending |
| A request already in flight is given up on when the user asks | `IgClient::send_or_cancel` and `read_or_cancel`, racing the token against the socket. Cancellation was read in every wait this program chose to take, which is where about nine interrupts in ten land; the tenth landed in the request itself, where nothing was watching and the exit tracked the server's patience — up to `REQUEST_TIMEOUT`, or 254 s on a black-holed connection because `Network` is retried. The cancel branch answers `Canceled` and not `Network`, or the reaction would be `Retry` and a Ctrl+C would send the request three more times |
| A push-back the body could not be read from is still a push-back | `IgClient::get` classifies from the status it already has when the body fails, rather than letting the read failure become a retryable network error |
| A credential is sent only to the address it was stored for | `plan`, for the token **and** the signing key, treating an absent or unparseable configured origin as a different destination |
| A calendar moment that went by is taken, not lost | `schedule::next_after` looks back from `now` to the floor before it looks forward |
| Jitter cannot cost the next run | `Schedule::room_for_jitter` bounds the grid — the gap less the floor between two runs, and no further than midnight when the days are restricted — applied to the default as well as to `--jitter`. `schedule::wake_at` narrows it again at each moment, in the zone, because seconds-of-day arithmetic is an hour too generous on the day a zone springs forward |
| One wall-clock moment is one run, in a zone that repeats an hour | `schedule::already_run_at_this_wall_clock`, asked in the direction ambiguity exists in |
| The rename cursor moves only over what this run could read | `engine::watch::compare` advances it only when every list the account has a capture of was accounted for, baselines included |
| A rename filed mid-comparison waits for the next window | `store::watch::renames_since` bounds above by the `head` the caller read first |
| Everything a scheduled run needs is checked while somebody is there | `snob watch check`, through `engine::check` — which takes `&App`, so it cannot record, and walks no list |
| Whether the monitor is working is an answer, not a reading | `watch::status::health`, in `status`'s output and in its exit code |
| A write is paid for out of the write budget, and there is no other way to send one | `Pacer::clear_to_send_write`, inside `IgClient::post`, which is the only function in the workspace that sends a method other than GET to Instagram |
| There are two writes, and a third one does not compile | `IgClient::post` takes a `graphql::Mutation`, not a path, so what this program can write is the set of variants that enum has — and `path`, `friendly_name` and `seed_doc_id` have no wildcard arm between them, so a new variant is a build error until somebody has written it into all three. `Mutation::ALL` and its test pin the quieter half: pointing a variant that already exists at a different operation |
| **Nothing is spent while the account is in cooldown** | `Pacer::clear`, which reads `budget.cooldown()` before it reserves and answers `IgError::InCooldown`. Eight callers in `snob-cli` still gate explicitly, and should: they refuse before asking a person for consent, and they serve a stored list instead of failing, neither of which a backstop can do. But the rule used to live only in those eight, and `engine::check` — the one command written to be polled — was the ninth that forgot: it knocked on a door Instagram had just closed, once per configured account, at whatever interval the poller ran. `SqliteRateBudget::reserve` charges its buckets and has never read the `cooldowns` table, so before this the guarantee rested on every caller remembering. `SNOB_IGNORE_COOLDOWN` still lifts it, because the escape hatch is read inside `cooldown()` |
| A write in flight is the one thing Ctrl+C does not abandon | `IgClient::post` sends and reads without racing the cancel token, unlike every read. Giving up on a read costs nothing; giving up on a write costs knowing whether it happened. `Pacer::clear_to_send_write` still reads the token before reserving and inside the wait, so a write is cancelable up to the moment it is sent and not after it |
| A write is never replayed by a redirect | the POST client is built on `redirect::Policy::none()` — following a hop on a write means doing the thing twice, which is not what "follow the redirect" costs on a read |
| A write without a CSRF token is refused before it is sent | `IgClient::post` returns `IgError::NoCsrfToken` on an absent token rather than sending a request that will fail, so a `--paste` session cannot spend budget discovering it cannot write |
| A file named by a server is created, never written over, wherever it lands | `output::create_new`, which `write_new` and both story writes in the browser go through. The browser's scratch write built its name from the username Instagram sent, filtered for the terminal and nothing else, and wrote it with `fs::write`: `printable` leaves `..`, a drive letter and a UNC share alone, `Path::join` hands an absolute name the whole path, and `fs::write` follows a link |
| A scratch directory with a guessable name is made, never adopted | `paths::create_fresh_private_dir` — the parent restricted first, the leaf with `create_dir` so a planted link fails instead of being followed, a leftover removed as a link and never through it. `create_dir_all` answered `Ok` on a link planted under `/tmp/snob-ig-stories/run-<pid>` and the `0700` landed on the target |
| A write is sent again only after an answer that says it did not happen | `client::worth_rediscovering`: a 4xx, a 404, or a 200 carrying `errors`. A 5xx, the redirect `post` refuses, and a 200 that would not decode are ambiguous — the write may have happened — and each used to earn a discovery walk and a second `mutate` |
| A tick is reported under the account it looked at, or under nobody | `engine::watch::tick` starts with no id and takes the engine's; when both lists were refused before either answered, what is stored about the name is the fallback and with nothing stored the run refuses by name. It started at the viewer's id, so a cooldown on a freshly added account reported `account.pk 42` to the stream and wrote a `rate_limited` row against the viewer |
| A failure is told in the language the answer was going to be in | `report::Wording`, decided once in `main::wording_for` from the same rule the command applies to its result. `report::error_json` is the one shape: the code the exit status names, message, causes, hint, challenge address, when a cooldown lifts |
| The reader leaving is not an error of this program's | `ui::say!`, which every line of prose on standard output goes through — `println!` panics on a closed pipe and Rust ignores `SIGPIPE`, so `snob watch status \| head -1` aborted |
| Nothing tells anybody you looked at their story | The row above is what holds this: registering a view is a write, and there is no variant for one. `crates/snob-core/tests/no_seen.rs` is the backstop, reading all four crates for `media/seen` and its spellings — including the Relay operation the web client really sends, which a capture turned up in August 2026. It is a denylist and it says so: the identifier Instagram acts on is a `doc_id`, and no list of words contains a number |

## Running headless

Supported on purpose — a homelab is a first-class place to run this.

`snob login` probes where the session can go **before** asking for anything,
and on a machine with no keyring it uses the file instead of refusing. Secret
Service needs a desktop session, so a server, a container or WSL has none; that
is normal rather than an error. On Windows that file is DPAPI-sealed and as
strong as the credential store; everywhere else it is plain JSON at `0600`, so
it is protected from other users of the machine and from nothing else — a copy
of it is a working session anywhere. That is why the fallback is never silent:
the command says which backend it landed on, because a session stored somewhere
less protected than the user expected is its own kind of failure. `--no-keyring`
still forces the file directly.

`SNOB_LOG` is a list of `target=level` directives — `snob_ig=debug,warn` — read
by `tracing_subscriber::filter::Targets`, not `EnvFilter`: that one parses span
and field matchers and links a regular-expression engine to do it, and nothing
here logs a span. A value that does not parse is said so once on standard error
and read as `warn`. `--verbose` is `debug` for this workspace's crates alone.

Everything else already works without a terminal: `prompt_secret` reads a plain
line when either stream it uses is not a TTY, the progress bar hides itself, `table`
becomes one name per line down a pipe, and the output format defaults to JSON.
The login **method** must be given explicitly there (`--paste`), since there is
no menu to show.

**Redirecting the results does not turn a question into a refusal.** Every
prompt is written to standard error and answered on standard input, so
`ui::can_be_asked` asks about standard input alone and nothing gates on standard
output: `snob scan someone | jq` reaches the consent question and can answer it.

The two exceptions are the prompts that **draw** rather than only ask, and both
need standard **error** to be a terminal as well — not standard output, which no
prompt here touches. `ui::can_show_a_menu` gates the menu, because that is where
`dialoguer` draws. `ui::can_mask` gates the masked secret prompt, which reads
its keys through `console::Term::stderr()`: that call answers `Key::Unknown`
immediately and forever when the stream is not a TTY, so gated on standard input
alone `snob login --paste 2> log` spun a core and never read what was pasted.

Two judgment calls worth understanding before touching them:

- **`truncated()` in `pager.rs`** decides whether a short list means the counter
  lied — it includes deleted accounts — or Instagram stopped serving pages. The
  threshold is measured against what was **declared**, not against what was
  walked. A shortfall of more than half is truncation at any size.
- **`declares_failure()` in `error.rs`** parses the body rather than searching
  it. Looking for `"status"` and `"fail"` anywhere in the text meant a follower
  named `fail` stopped the walk.

## Settled, so nobody re-opens them

- **`friendships/show_many` is not going to be used, and the reason is
  structural rather than cautious.** It would take a 1000/500 crossing from 52
  requests to 14, which is a real saving in the direction this project's pacing
  has only ever moved. It is still a no, on three grounds in increasing order of
  weight:
  - The 52-to-14 was an estimate, and the August 2026 capture makes it look
    like the wrong way round. **The real client batches twelve or thirteen ids
    per call, never more** — 41 calls in one session, thirty of them exactly
    twelve, tracking the rendered page one for one. At that batch size a
    thousand accounts is roughly eighty calls, which is worse than walking the
    second list rather than better. Whether twelve is the endpoint's limit or
    only what the page asks for is unverified, so this does not settle the
    saving; it removes the reason to assume there is one.

    And a fourth ground, which is the one that closes it. Across 461 statuses
    in that session the fields returned were `following`, `is_bestie`,
    `is_feed_favorite`, `is_private`, `is_restricted`,
    `text_post_app_pre_following`, `incoming_request` and `outgoing_request`.
    **`followed_by` is not among them, not once.** So `show_many` cannot answer
    "does this account follow me" — it could only ever have replaced the
    *following* half of a crossing, and the followers half is the expensive
    one.
  - It is a POST, and **in this codebase POST means write**. `IgClient::post` is
    the one function that sends anything other than a GET, and it is where the
    write budget is paid, the CSRF token is required and the redirect policy
    refuses to replay. A non-mutating POST would have to either go through that
    door — paying a fifteen-minute write slot to read a page of relationships,
    which exhausts the write budget doing reads — or around it, which means a
    second POST path that pays nothing.
  - And that second option is the one that matters. What makes the write regime
    checkable is that it is one door with one guard: `AGENTS.md` can promise
    that every write is paid for and confirmed, and a reader can verify it by
    reading one function. A read-only POST removes that invariant in exchange
    for requests on a crossing that is already inside the budget. The invariant
    is worth more.

  If it is ever reopened, the thing that would change the answer is not a
  measurement of the saving but a way to keep "one door" true — and the honest
  version of that is a separate function with its own name, its own budget line
  and its own row in the table below, not a flag on the existing one.
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
- **The pacing rate has no published reference behind it, and it stays where it
  is.** Looked for in August 2026 and written down at `pace.rs` rather than
  here, because that is where somebody changing a number will be. The short of
  it: the best public figure for this endpoint family is instaloader's
  field-report guess of 75 requests per 660 s, snob walks at about 172, and the
  difference is that the cadence was copied from a project that walks GraphQL.
  It is recorded rather than acted on, because matching that guess means 8.8 s
  a request and a 235-page walk of 34 minutes against a 900-second resume
  window — an interrupted walk could never be continued, and a list that took
  34 minutes to read no longer describes one moment, which is the thing every
  comparison here rests on. The ubiquitous "200 calls per user per hour" is
  Meta's Graph API platform limit for `graph.facebook.com` and is not evidence
  about these endpoints at all.
- **The wire signature is chosen for portability, and is not to be tuned to
  resemble anything** — the TLS handshake, the HTTP/2 SETTINGS and the header
  order alike. The reason written here used to be that there is nothing to copy,
  because Chrome has randomized its ClientHello extension order since v110. That
  premise is true and the conclusion stopped holding in 2023: JA4 sorts the
  extension list before hashing it, exactly so the shuffling changes nothing. So
  something copyable does exist, and the reasons not to copy it are these.
  Adopting a browser's cryptographic identity would be a claim to be a browser,
  which snob is not — the aim here is to ask honestly and lightly, not to pass
  for something else. Trying would also mean leaving `rustls`, and the clean
  static cross-compilation with it, and would buy nothing: what determines
  whether Instagram throttles an account is, in order, the address the requests
  come from, how many there are, and how fast.
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
  other reason.** `Sec-Fetch-Site` and `Sec-Fetch-Mode` are sent because a
  browser sends them and a request without them is the anomaly — this used to
  say Instagram answers `Vary` on the pair, which is **unverified**: the August
  2026 capture saw `Vary` on `Origin`, on `Accept-Encoding` and on
  `Accept-Language, Cookie`, and on no `Sec-Fetch-*`, but it kept no response
  headers that survive to be re-read, so nobody has actually checked. It changes
  nothing either way. `Accept` is `*/*` because that is what `fetch()` sends when
  the page sets nothing; `sec-ch-ua` is computed from the version rather than
  hardcoded, including the order of its three entries; no `Origin` on a
  same-origin GET, which the Fetch standard omits there. The stored User-Agent
  follows the major version of a browser actually installed on the machine, at
  most daily, and never when the user pinned one.
- **`@someone` never reaches us on PowerShell.** `@` is the splatting operator,
  so the argument is gone before `main` runs and the tool answers about the
  user's own account, with exit 0 and nothing to say a different question was
  asked. Nothing can detect it at run time — but a command written to be typed
  back can be, and `tests/language.rs` walks the sources for one: a `snob …`
  command carrying an at sign fails the build. Do not write unquoted `@name` in
  examples aimed at PowerShell users; a name is accepted without the sign, which
  is the spelling that needs no explanation next to it.
- **Everything is per user, never per directory.** The session, the database and
  the cache come from `directories`, so running `snob` from two folders is one
  session and one cache. The only thing the working directory decides is where
  an export lands without `-o`. The header of `paths.rs` says so; the tests fix
  it.
- **The trust store is narrowed only when asked.** reqwest 0.13 made
  `rustls-platform-verifier` the default, so the four rustls targets honor
  enterprise roots — which is what makes snob work on a managed machine, and is
  also how a laptop carrying a TLS-inspecting root lets that middlebox read the
  session in transit. `--strict-roots` replaces the platform store with
  Mozilla's published roots, and `--tls-extra-root` is the way back out for
  somebody who needs one private CA and no others; clap requires the second to
  come with the first, because on the platform store there is nothing to add to.
  It is off by default deliberately: narrowing is the safer setting for somebody
  being inspected and the broken one for somebody behind a corporate proxy, and
  only the person running it knows which they are. It is **never** applied to
  the webhook client, and that is structural rather than remembered — see the
  rules table. On Windows for ARM64 the backend is schannel, which cannot
  express "these roots and no others", so the flag is **refused** there rather
  than accepted and ignored: a security option that silently does nothing is
  worse than one that is not offered, because somebody believes it. Certificate
  **pinning** stays rejected separately: Meta rotates leaves across issuers and
  there is no fast update channel behind this binary.
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
- **The login browser is talked to over a pipe, and there is no debugging
  port.** It used to be `--remote-debugging-port=0`, and what that cost was
  demonstrated rather than argued: a second local process read the port out of
  `DevToolsActivePort`, called `/json/version` with no credential at all, and
  got the session cookie back from `Storage.getCookies` — `httpOnly` is a rule
  for page scripts and means nothing to the protocol itself. Loopback sockets
  carry no per-user access control, so that was every account on the machine
  for as long as the window was open. `--remote-debugging-pipe` moves the same
  protocol onto two anonymous descriptors that only this process and the
  browser hold, and an address nobody can name is not one anybody can connect
  to. The price is that `std::process::Command` cannot start that browser:
  Chromium reads descriptor 3 and writes descriptor 4, and on Windows it
  reaches them through `_get_osfhandle`, which only answers if the C runtime
  found them in the handle-inheritance blob the parent passed in
  `STARTUPINFO.lpReserved2` — a structure no Windows header describes and
  `Command` does not expose. So the launch goes through `CreateProcessW`
  directly, with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` so that those two
  descriptors are the only handles inherited; on Unix the same idea is two
  `dup2` calls in a `pre_exec` hook. It retired `tokio-tungstenite` and
  `futures-util`, because NUL-separated JSON needs no library. The reasoning
  and the layout are in `crates/snob-cli/src/pipe.rs`; `tests/browser_pipe.rs`
  holds it down against a real browser, and skips rather than fails where none
  is installed.
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
- **The Windows credential is written as `CRED_PERSIST_LOCAL_MACHINE`**, so it
  stays on the machine that created it rather than following the user around a
  network with Credential Roaming on. Getting there meant dropping the
  `keyring` wrapper for `keyring-core` and the platform stores it was already
  choosing, which is where the `persistence` modifier lives. What had to be
  answered first was whether an entry written the old way survives the change,
  and it was answered against a real Credential Manager rather than reasoned
  about: `CredReadW` is keyed on the target name and the credential type and
  takes nothing else, so an Enterprise entry is found unchanged by a lookup
  from a store configured for Local, and the next save rewrites that one record
  in place — `Persist` 3 becomes 2, no second entry, nothing orphaned. Nobody
  is logged out and no read-under-both-persistences path is needed. The target
  names did not move either, because these are the same stores in the same
  default configuration `keyring 4` was building. `secrets.rs::entry_for`
  carries the experiment; `the_windows_credential_is_local_to_this_machine`
  keeps the modifier from being silently mistyped. Going direct also dropped
  `regex` and `aho-corasick` with the Windows store's `search` feature:
  **821,248 bytes, 10.41% of the x86_64 Windows binary**, measured either side
  of the change.
- **There is no lease over `watch_marks`, and the upsert is unconditional.**
  `snapshots` has a whole lease because the database is shared between
  processes; the marks have none, while `compare` reads the mark minutes before
  `commit_report` writes it back. Two overlapping runs therefore report one
  arrival twice under two `run_id`s. It costs duplicates and never loses a
  window, and by the time the mark regresses both reports have already been
  delivered — so an ordering clause on the upsert suppresses only the third copy
  while introducing a write that declines without saying so, on the one
  statement that retires a report. What would prevent the first two is a
  per-account run lease, and half of one is worth nothing. The two clauses that
  look like they would work, and why neither does, are written out at
  `store::watch::set_mark`, with a test for each.

- **Highlights are not `stories`, and will be a command of their own.** A
  story is what is up for a day; a highlight is what an account chose to keep,
  for years, and a profile carries several of them with a title each. Folding
  them into `snob stories` would need a flag to say which of the two is meant
  and a second index to say which highlight, on a command whose `-d 2` means
  "the second thing in the one list". The shape that reads is `snob highlights
  someone` for the numbered tray, `snob highlights someone 2` for the items of
  the second, and `-d`, `--all`, `-o`, `-i` on that exactly as `stories` has
  them. `snob profile` lists the tray; downloading is the command that does
  not exist yet. The client half is in and verified: `IgClient::highlights_tray`
  and `IgClient::highlight`, which is `reels_media` asked with `highlight:<id>`.
- **The story browser draws with `console` and reads keys with `crossterm`, and
  is not built on a terminal-UI framework.** Measured in August 2026 by building
  this tree twice: the browser as it stands is **+18,432 B and 0 new crates on
  Windows and macOS** — `comfy-table` was already compiling `crossterm`, so
  only its `events` and `bracketed-paste` features are new, and on Linux that
  is `signal-hook` and `signal-hook-mio` — against **+106,496 B and +27
  crates** for the same five behaviors on `ratatui` 0.30 with `crossterm` 0.29.
  What the difference buys is a cell buffer with a diff and a Cassowary solver
  (`kasuari`), plus `compact_str`, `castaway`, `lru`, `strum`, `itertools`,
  `line-clipping` and `unicode-truncate`. A list of rows of text uses none of
  it: a line diff is 115 lines and is in `ui/browser/screen.rs`.
  **What would reverse this** is a second pane, a navigable table with columns,
  or a view that scrolls independently of the focus. At that point the cell
  buffer stops being overhead and starts being the right tool, and 106 KB is not
  a lot to pay for it. One list with one highlighted row is not that point.
  `termwiz` was measured too and is not close: **+688,640 B and 86 crates**, six
  times `ratatui`, because it is WezTerm's whole terminal layer. `cursive` is
  comparable to `ratatui` in size and wrong in shape — it owns the event loop,
  and `browse` is an `async` function making HTTP requests between keystrokes.
- **Stories are handed to the system viewer and are not drawn in the terminal.**
  Re-examined in August 2026 against what terminals actually do now, because
  2026 genuinely moved: kitty's graphics protocol is no longer kitty's alone —
  Ghostty has it since 1.0 (December 2024), iTerm2 since the 3.6 series
  (September 2025) and VS Code since 1.110 (February 2026) — and Windows
  Terminal shipped sixel in 1.22 (August 2024). It still does not survive
  contact with this project's platforms. Windows Terminal speaks sixel and
  nothing else (256 colors, no alpha; kitty graphics is microsoft/terminal#8389,
  open since 2020 and unassigned), Alacritty speaks neither, and VTE — GNOME
  Terminal, Tilix — speaks neither. Three things settle it:
  - **The floor is +312,320 B and 17 crates**, for `image` with only `jpeg` and
    `png`, and it is unavoidable: every protocol including the half-block
    fallback needs the picture decoded and resized to the cell grid first.
    `viuer` is +431,104 B and 44 crates on top of that; `ratatui-image` is
    +637,952 B and 125 crates, and does not build on Windows with its default
    features at all, because `chafa-dyn` wants `pkg-config` and libchafa.
  - **What would be on screen is not the story.** A story is 9:16. On an 80x24
    terminal, half-blocks give 80x48 subpixels, so fitting to the height
    yields **27x48** — smaller than a favicon; octants, where a terminal draws
    them at all, reach 54x96. The decisive part is not the resolution: stories
    carry text on top of the picture — captions, stickers, polls, song titles
    — and at that size, and at 112x200 in a maximized window too, that text is
    not ugly, it is **absent**. A browser that renders stories as ANSI art
    loses the actual content of a large share of them.
  - **A large share of stories are video, and no protocol plays video.** kitty's
    animation extension is implemented by kitty alone and explicitly declined by
    Ghostty. `commands::stories` already sniffs `ftyp` and writes `.mp4`. The
    best a terminal could do is one silent cover frame, which is strictly worse
    than what `opener` already does: the real file, in a real player, with
    sound and a scrub bar, on every platform, for no dependencies.

  If it is ever wanted anyway, the only defensible shape is `image` with
  `jpeg`+`png`, the kitty protocol written by hand with a DA1 probe and a hard
  timeout, **for photo stories only**, with `opener` for video and as the
  fallback everywhere. That avoids `viuer`, `ratatui-image` and `ratatui`
  entirely and still costs 3.9% of the binary.

## State

Every command works and has been exercised against the live API. What follows
is what meeting Instagram taught, including the parts that are still open:

- **`snob stories` works and has been run against Instagram.**
  `GET /api/v1/feed/reels_media/?reel_ids=` answers on **`www.instagram.com`**
  with a web session — which was the open question, because every write-up
  documents that path against `i.instagram.com` and the header of
  `client/mod.rs` says why this crate does not go there. Confirmed in August
  2026 against a real account with stories up: the items, their sizes,
  `taken_at` and `expiring_at` all arrive, and the largest candidate is the one
  taken.
- **`snob follow` and `snob unfollow` work, and have been run against
  Instagram.** The route is `POST /api/graphql` with a Relay operation name and
  a `doc_id`; the word "friendship" appears nowhere in what a browser sends.
  Two REST spellings were tried live first and both are dead —
  `POST /api/v1/friendships/create/{pk}/` answers 200 with the web app's HTML
  shell, and `POST /web/friendships/{pk}/follow/` answers 404 — which is
  recorded because somebody will read a Python client and propose them again.
  `x-web-session-id` was the one unexamined difference at the time, and the
  capture below settles it: a browser does not send it on `/api/graphql` at
  all, so the question is moot rather than answered.

  Run again in August 2026 after the Relay headers were added — one counted
  follow and one counted unfollow against a real account, each announcing its
  operation in `X-FB-Friendly-Name` and carrying `X-FB-LSD`. Both were accepted,
  and neither drew a push-back of any kind, so the load headers next to
  `Retry-After` are still unobserved under refusal.
- **A real browser session has been recorded and compared against this code**,
  August 2026: 251 seconds, 1890 requests, over the DevTools Protocol, driven
  by hand through login, profiles, stories, follow, unfollow, block, unblock
  and like. The transcript held a live session and was deleted; what it settled
  is spread through this document and through `pace.rs`. Three things are worth
  having in one place.

  It confirmed the two seeded `doc_id`s in `graphql.rs` unchanged, `ASBD_ID`,
  the app id, and that `client_hints::brands` reproduces a real Chrome 151
  byte for byte — GREASE spelling, GREASE version and entry order.

  It showed the web client now reaches almost everything through GraphQL:
  `/api/graphql` 151 times and `/graphql/query` 27 against 11 calls to
  `/api/v1/`. **That is not a reason to migrate**, and the reasoning is with
  the `show_many` decision above: every GraphQL operation is a POST, a POST is
  a write here, and one door with one guard is worth more than fewer requests.

  The first one was taken through a lens that reports the headers a page set
  rather than the ones that went on the wire, so nothing could be concluded
  from an absence in it. **A second capture, with the wire headers, settled
  four things the first could only suggest**, and the counts are kept because
  they are what makes them settled rather than argued:

  - `x-ig-www-claim`, `x-requested-with` and `x-web-session-id`: 0 of 171 on
    `/api/graphql`, 0 of 23 on `/graphql/query`, 97 of 97 on `/api/v1/`. Relay
    does not announce itself as an XHR and does not echo the claim, so the two
    this tool was sending there are not sent any more.
  - `Priority`: `u=0, i` on all six navigations, `u=1, i` on all 924 fetches
    and XHRs, which sources a constant written from the RFC.
  - **`/api/v1/friendships/{pk}/followers/` is alive and is this tool's own
    route** — twenty-seven calls in one session of scrolling the list, with no
    GraphQL operation near it. The read this program spends nearly every
    request on has nothing to migrate to.
  - The list payload carries `pk`, `username` and `full_name` and **no
    friendship status**, which is why `friendships/show_many/` exists at all.
    See the settled note about it, and the batch size recorded there.
- **The profile page was captured on 23 August 2026, across eleven accounts**,
  driven by hand: public and private, followed and not, with and without
  posts, with none to eighteen highlights, with nobody to thirty-three people
  in common, and the viewer's own. 91 navigations, 758 API calls, 6 HTML
  documents; the transcript carried a live session and was deleted once the
  findings below were written down. What it settled:
  - **The web client no longer calls `web_profile_info`.** The header comes
    from `PolarisProfilePageContentQuery` on `/api/graphql`, a POST. The REST
    endpoint still answers — verified with a counted request the same day —
    and carries everything `snob profile` prints: `edge_followed_by`,
    `edge_follow`, `edge_owner_to_timeline_media.count`, `biography`,
    `external_url`, `follows_viewer`, `has_requested_viewer`,
    `highlight_reel_count`, `category_name` (an empty string when there is
    none), and `edge_mutual_followed_by` with the count and the **three**
    names the page puts in its "Followed by" line. Nothing in it says whether
    a story is up; that is GraphQL-only (`latest_reel_media`), so `profile`
    spends the `reels_media` request to know.
  - **`GET /api/v1/highlights/{pk}/highlights_tray/` answers on `www`** with
    the whole tray in one page — eighteen highlights arrived in one, and
    `has_next_page` was false on every account — carrying id, title,
    `media_count`, `created_at`, `updated_timestamp` and a 150px cover. The
    web client reaches the same data through
    `PolarisProfileStoryHighlightsTrayContentQuery`, a POST, which this crate
    does not take.
  - **`reels_media` takes a prefixed id.** `highlight:<id>` returns the
    highlight as a reel that `ReelItem` parses unchanged, with `expiring_at`
    absent on every item; the browser itself uses the same endpoint with
    `archiveDay:<id>` for the viewer's story archive.
  - **`GET /api/v1/friendships/{pk}/mutual_followers/?page_size=12&max_id=N`**
    is the "mutual" tab of the followers dialog: the accounts the viewer
    follows that follow the account, twelve a page, the offset as the cursor,
    the same user shape as a followers page and no cursor on the last page.
    Thirty-three mutuals were three pages. A larger `page_size` has not been
    sent. This is what `profile` names the whole list with, at
    `count / 12` requests and nothing stored, where `engine::people::in_common`
    needs the account's followers walked.
  - **A private account the viewer does not follow** still answers the
    profile with counters, bio and the mutual line, and answers the tray with
    nothing. `profile` does not ask for the tray or the reel there, and says
    they are not visible rather than that there are none.
  - **The browser marks a highlight as seen through the very mutation it uses
    for a story**, with `reelId: "highlight:<id>"` — 42 times in this session.
    `no_seen.rs` already names it, and the prefix changes nothing: reading a
    highlight through this crate registers nothing.
  - **Two endpoints worth a command, not yet taken.**
    `GET /api/v1/friendships/pending/` is the follow requests waiting for the
    viewer's answer — 73 of them, one page, the followers shape — and
    `GET /api/v1/archive/reel/day_shells/` is the viewer's own story archive
    by day, each day a reel `reels_media` serves as `archiveDay:<id>`. Both
    are GETs about the viewer's own account.
  - The followers dialog asks `count=12` with `search_surface=follow_list_page`;
    this tool's `count=50` is a settled decision above and was not revisited.
- **Some accounts cannot be resolved at all, and it is Instagram's fault.**
  `web_profile_info` answers **400** for certain business accounts with
  `Asset asset://laser.provider/ig_business_category_subvertical has been
  deleted. You cannot use this schema` — Instagram failing to serialize its own
  reply. It breaks every command that takes a username, `pfp` and `scan`
  included, and it predates all of this. Confirmed live on `elrubiuswtf` in
  August 2026, reproducible. `instantgram` hit the same wall in July 2026 and
  worked around it with a search-endpoint fallback for username-to-id, which is
  the shape a fix here would take.

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
    `Schedule::room_for_jitter` bounds the grid, and it takes the interval as
    the step when there is one: `--every 2w --on mon` has a weekly grid and a
    fortnightly floor, so its room is zero. It also stops at the last moment of
    the day when the days are restricted, because past midnight is a day the
    calendar does not name. That is not the whole of it: the arithmetic is
    seconds-of-day, and a day a zone springs forward through is an hour shorter
    than 86400, so on that one day it allows an hour the day does not hold.
    `wake_at` is what the loop calls, and it asks the calendar in the zone from
    the moment actually due — the next moment less the floor, and no further
    than the local midnight after it. It only ever narrows, which is what keeps
    the banner honest about what it has already printed.
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

    Two other tables are swept in the same call. The run log goes at thirty
    days, **except the newest row of each account, whatever its age** — that is
    what `watch::status::health` reads, and without the exemption a monitor whose
    session expired would age out of "the last run ended in no_session" and exit
    1 into "it has not run yet" and exit 0, which is a probe going green while
    nothing was fixed. The outbox settles itself: `deliveries::expire_stale`
    gives up on a report that reached a day, and `forget_settled` forgets a
    delivered one after seven.

## Reproducible builds

Two builds of one tag produce the same bytes, and four things had to be true at
once for that to hold.

`rust-toolchain.toml` names an **exact** version rather than `stable`, because
a rebuild months later cannot match a release compiled by a different compiler,
and a mismatch that proves nothing is worse than no check. Bumping it is now a
commit somebody reviewed.

`-Clink-arg=/Brepro` is in both Windows tables in `.cargo/config.toml`. Two
release builds of one commit used to differ in exactly 24 bytes, all of them in
the PE header: the TimeDateStamp, which is the wall clock at link time, and the
CodeView GUID, which is random per link. `/Brepro` derives both from the input
instead. Verified here rather than assumed -- two release builds of this tree,
with `target/release` deleted in between, gave the same SHA-256; the same tree
without the flag gave a different one.

The absolute paths come out through a **second config file**, written by the
release workflow into `$CARGO_HOME/config.toml`. That shape is not decoration:
`RUSTFLAGS` *replaces* `.cargo/config.toml` rather than adding to it, so
exporting it there would silently drop `+crt-static` and both Windows binaries
would go back to needing the Visual C++ redistributable -- with nothing failing
until somebody could not launch one. Cargo joins the `rustflags` arrays across
config files, which is what makes the second file work, and
`[target.'cfg(all())']` merges with the per-triple tables rather than replacing
them. `CARGO_HOME` is moved inside the checkout in that workflow so a single
`--remap-path-prefix` covers the sources and the unpacked registry together.

The archives were the last of it, and an archive nobody can rebuild makes the
binary inside it unverifiable too. Both formats embed a modification time and
both are now given one -- the committer date of the tagged commit, derived from
the tag like everything else there. `tar` gets `--sort=name`, `--mtime`,
`--owner`/`--group` and `gzip -n`, that last for the timestamp gzip writes into
its own header and everybody forgets. `Compress-Archive` has no flag at all, so
the staged files' `LastWriteTime` is set before it runs, rounded down to an even
second because a zip's DOS timestamp has two-second resolution. Both recipes
were checked locally: same content on two different days, same bytes, where the
recipes they replace gave two different archives. The `.deb` needed nothing --
`cargo-deb` was already reproducible.

## What it costs, in bytes

Measured on `aarch64-pc-windows-msvc` in August 2026, which is the odd target —
it is the one on schannel rather than rustls. Recorded because these are
decisions somebody will otherwise re-open every year with no number to argue
against.

| | |
|---|---|
| Binary, aarch64-pc-windows-msvc | 5,885,952 B |
| Binary, x86_64-pc-windows-msvc | larger by roughly a third |
| Bundled SQLite | 532.6 KiB of `.text`, 9.4% |
| `rust_xlsxwriter` + `zopfli` | ~498 KiB, 6.6%, for one of five output formats — behind the `xlsx` feature, on by default. Measured on x86_64 Windows, August 2026: **656,384 B, 9.3%**, and six crates, between the default build and `--no-default-features`, which is what the feature lets a source build leave out |
| Static CRT on Windows | +126,976 B per binary |
| `tracing-subscriber`'s `env-filter`, replaced by `Targets` | **−327,680 B, 4.4%**, −3 crates (`matchers`, `regex-automata`, `regex-syntax`), measured on x86_64 Windows release either side of the change |
| Interactive story browser, rewritten | +18,432 B, 0.23%, 0 new crates on Windows |
| `ratatui` + `crossterm` for the same browser | +106,496 B, 1.32%, +27 crates — rejected |
| `image` with only `jpeg`+`png`, the floor under any terminal image | +312,320 B, 3.9%, +17 crates — rejected |
| Chromium profile after `snob login --browser` | 87.2 MB, 886 files |

The first two of those are the price of "one binary, no runtime", and they are
the right price. **The third is the largest single feature cost in the tree**
and nothing recorded that anybody had weighed it; it stays, but it is written
down now. The last is not a build cost at all and dwarfs all of them, which is
why `login` removes the profile when it is done with it.

The three Unix targets have never been measured — nothing here cross-links
them. Note that musl carries the Secret Service stack, some thirty crates a
Windows build does not, so it is not comparable.

**The three terminal-interface rows are measured on `x86_64-pc-windows-msvc`**,
not on ARM64 like the rest: they are deltas taken by building the same tree
twice, which is the only honest way to price a library, and the host is what it
is. The baseline they are against is 8,068,096 B, which is 1.37x the ARM64
figure above and agrees with the second row. Deltas of this kind do not
transfer between targets exactly, but they do not change order of magnitude
either.

Two of those numbers are much smaller than the same libraries cost on their own,
and the gap is the point. `ratatui` with `crossterm` on a hello-world is
+167,936 B; inside this binary it is +106,496, because LTO shares code with
everything already here. Quoting the standalone figure would overstate the cost
by 58%, and quoting a standalone figure for anything else here would too.

## Measured, decided, and not done yet

Found by an audit in August 2026, with numbers. Written down here rather than
left in a report nobody can find, and in the order they are worth doing.

- **The crate split is not a build-time optimization, and reopening it as one
  will disappoint.** Splitting `snob-store` out of `snob-core` was done for the
  dependency graph — `snob-ig` no longer compiles SQLite, three keyring
  backends, `directories` or a TOML parser — and the compile-time effect was
  measured before and after in this tree: `touch crates/snob-store/src/store/snapshots.rs`
  rebuilds all four crates in 6.22 s, against 5.08 s for `touch crates/snob-cli/src/report.rs`,
  which rebuilds one. `cargo check` after the same touch is 1.93 s. The
  difference the split can ever make is that ~1.1 s, because the dominant cost
  is `snob-cli` — 26,324 lines, half the workspace — and it depends on both
  halves whichever way they are arranged.

  What would make a further split worth it is a **second consumer**: another
  binary in this workspace, or publishing `snob-ig`. Neither exists.

- **`Retry-After` is measured, not acted on.** `classify` takes a status and a
  body and never sees a header, so whether these endpoints send this at all has
  never been answerable from a real run. `IgClient::note_push_back` now logs it
  at `debug` on every push-back — including the two a check on the status alone
  walks past, a 200 carrying `spam: true` and a push-back whose body died
  mid-read — and logs `<absent>` when there is none, which is the answer the
  logging is really after. **Nothing decides anything from it**, deliberately:
  inventing behavior on the assumption that the header arrives is guessing with
  somebody's account. When it is implemented it is a floor and never a ceiling,
  because a server naming thirty seconds is answering a different question from
  how long an account is left alone after Instagram has objected. That is
  written at `note_push_back`, where somebody adding it will be looking. The
  tests assert on the value `Answer` carries and that the recorded cooldown is
  unchanged.

  **The write path was not covered by any of that until August 2026**, which
  defeated the point: `post` neither read the header nor called
  `note_push_back`, so the one class of request that earns the twelve-hour
  cooldown was the one nobody was listening to. It goes through `decode` now,
  like a read, and `decode` is where the push-back is noted.

  **Alongside it, what Instagram volunteers about its own load.** A browser
  capture found `x-ig-capacity-level` and `x-ig-peak-time` on every API answer,
  and `Answer::load` now carries them into the same line. Also not acted on, and
  for a sharper reason than `Retry-After`: they describe a datacenter's
  headroom, which is identical for everyone served by that region, while what
  the pace is managing is a checkpoint on one account. Acting on them would also
  be a mechanism for going *faster*, and these numbers have only ever moved the
  other way.

  **What is still owed is a real run.** The logging is in so that somebody who
  gets throttled with `--verbose` on can say whether the header ever arrives and
  in which of its two forms — seconds or an HTTP date — and what the capacity
  level was when it did. The capture answered none of it: 426 answers, every one
  of them a 200. Until somebody has been refused, there is nothing here to
  implement, which is why it is parsed into nothing and kept as the string that
  was sent.

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
- **Instagram cannot serialize its own profile reply for some business
  accounts.** `GET /api/v1/users/web_profile_info/?username=elrubiuswtf` answers
  **400** with `Asset asset://laser.provider/ig_business_category_subvertical
  has been deleted. You cannot use this schema`. Reproducible, nothing to do
  with the request, and it took down every command that names an account,
  because they all begin by turning a username into an id. Confirmed against the
  live API in August 2026 — 400 for that account, 200 for an ordinary one in the
  same session, which is what scopes the answer to the failure.

  The answer is a **fallback, not a replacement**: `web_profile_info` tries the
  profile endpoint exactly as before and reaches
  `/web/search/topsearch/?context=blended&query=<name>` only after a failure
  that `IgError::worth_a_second_route` allows. An ordinary account still costs
  one request; the broken one costs two, both charged, and the wait between them
  is the ordinary pace. `instantgram` hit the same wall in July 2026 and solved
  it the same way.

  **Search answers with less, and the gap is not filled in.** It carries the id,
  the name, `is_private`, and — under `friendship_status` — the two flags the
  private-account refusal turns on, so that refusal still happens before a page
  is walked. It carries **no follower or following counters**, and those stay
  `None` rather than becoming zero: `pager::verify_completion` compares a walk
  against the declared size, so a declared zero would make every short walk look
  complete and the truncation wall would stop being detectable at all. Two
  things therefore quietly stop working for such an account, and both are said
  out loud rather than left to be discovered — `engine::target` warns that the
  run cannot tell a truncated list from a complete one nor judge a cached one,
  and `snob watch check` returns `Warned` with the same reason, because finding
  the truncation wall before six hours of walking is half of what that command
  is for. `WebProfileInfo::counters_are_knowable` is the question a caller asks;
  the reasoning is at `IgClient::web_profile_info`.

  Search matches loosely, so the hit is held to an exact, case-insensitive name
  match. Without that, a name Instagram would not serve hands back whatever the
  search box suggested instead, and the run walks a stranger's followers under
  the name that was typed. That is the one failure this route could introduce,
  and it has a test of its own.
- **Real behavior on a 429 has never been provoked on purpose.** The handling is
  verified against a recorded body. Everything downstream of it — the cooldown,
  the hard stop, the exit code — is tested; the classification of a live one is
  not. The cooldown half only became true in August 2026: every test that drove
  a throttling body through a real client hung `Pacer::unlimited()` off it,
  whose `start_cooldown` answers `Ok(0)` and forgets, so the recording could be
  deleted whole with the suite still green. There is a budget double that
  remembers now.
