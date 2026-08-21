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
follows and unfollows one account at a time, and shows and downloads the stories
an account has up. Single binary, no runtime. Windows and Linux on x86_64 and
ARM64, macOS on Apple Silicon.

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

- **Two write operations exist, and no third one may be added.** `snob` may
  follow an account and unfollow an account. That is the whole list. No block,
  no remove-follower, no like, no comment, no message, and **nothing that marks
  a story as seen** — which is a write dressed as a read, because it tells the
  other person you looked.
  This rule used to say that `snob` only reads, and it was right about the risk:
  writing is what gets an account actioned, and the tool spent its first two
  versions not doing any. The rule was lifted deliberately, for those two verbs
  and no others, and what replaced it is not permission but a regime:
  - **One account per invocation.** There is no bulk mode and there is no flag
    that makes one. The pattern Instagram acts on is not the daily total but
    the burst — a hundred unfollows in half an hour is actioned even when the
    day's count is unremarkable — and the follow-then-unfollow churn that
    `snob unfollowers` makes so easy to automate is the specific behavior its
    detection was built for. Piping a list of names into a loop is the user's
    business; handing them the loop is ours, and we do not.
  - **Every write is paid for out of its own budget**, the `writes` bucket in
    `rate_budget`, which is far slower than the one reads come out of. The
    numbers and their sources are in `crates/snob-ig/src/pace.rs`.
  - **Every write is confirmed before it is sent**, on standard error, with
    `-y` as the way to answer in advance. Nobody loses a follow to a typo.
  - **A `feedback_required` on a write is an action block**, not a throttle, and
    it earns the twelve-hour cooldown rather than the two-hour one.
  The enforcement is structural, not a promise: writing goes through
  `IgClient::post`, which cannot be reached without paying the write budget, and
  a test reads the source of all three crates to check that no `media/seen` call
  has appeared anywhere.
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

cargo test -p snob-cli --features testing            # the binary itself
```

Without the `--`, cargo keeps the flags instead of passing them through.

**The `testing` feature is what lets a test drive the program.** Everything else
in the suite drives a library: `watch_tick.rs` runs the comparison through
`engine::watch::tick`, `watch_webhook.rs` runs the outbox through
`WebhookClient`. Nothing ran `main` — not the dispatch, not `AppPaths::discover`,
not the secret store choosing a backend, not the exit code a timer reads. The
feature adds two flags for that and **a released binary contains neither them nor
the code they reach**, which `crates/snob-core/tests/sandbox.rs` reads the source
to hold down. `--sandbox-root` puts every file a run touches under one directory,
forces the file backend, **and gives the run a keyring service name derived from
that root** — `crates/snob-core/tests/keyring.rs`'s rule, applied to the binary.
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
| Every request is paid for | `Pacer::clear_to_send`, inside `IgClient::get` |
| A 429 puts the account in cooldown | `IgClient::classify_and_record` |
| The reported request count is what was really spent | `Pacer::spent`, read by `engine::list` |
| Consent before enumerating someone else, **before** resolving | `engine::ask_consent` |
| Only Instagram's CDN is ever downloaded from | `IgClient::check_downloadable` |
| A name is filtered before anything draws it, whoever it came from | `model::printable`, reached through `User::safe_username` / `safe_full_name`, `Viewer::safe_username`, `app::target_label`, `error::body_excerpt`, `error::missing_message`, `target::label` and `scan::summary_target` — and, for everything a failure prints, through `report::filtered`, which every branch of `print_error` goes through including the one that carries no label. Where a name came from decides whether it can be *trusted*, not whether a control character in it reaches a terminal — so the typed ones go through it too. `printable` covers the invisibles that are `Cf` **and** the four Hangul fillers, which are ordinary letters by category and blank by rendering |
| A name inside a URL or a header is encoded, never filtered | `model::in_a_path`, used by `User::profile_url` and by the `Referer` the Instagram client sends — filtering removes characters, and a name with one removed is the address of a different account. A header value cannot hold a byte below 0x20 at all, so an unencoded name there produces no request rather than a wrong one, and reqwest reports that as `Network`, which the pager retries |
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
| There is one spelling of each outcome token | `ExitCode::as_str`, with `from_token` derived from it over `ExitCode::ALL` rather than written as a second match. `watch_setup::health` matched the literals inline: respell one there and every recorded cooldown falls through to the failing arm, so `status` exits 1 for a monitor that will resume on its own — and the fixture those tests build their rows from spelled the same literals, so the suite would have moved with the defect |
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
| A walk in progress has exactly one writer | `snapshots::save_page` **and** `snapshots::close` both refuse a snapshot this process does not hold; `is_resumable` asks without claiming. `close` had no guard and releases the claim, so a process whose lease had gone stale killed the walk that adopted it |
| Two processes never walk into one capture | `snapshots::resumable`, which takes the claim in the statement that finds the row |
| A finished capture is never unfinished again | `snapshots::close`, whose `WHERE` carries `complete = 0` |
| No request is sent after the user asks it to stop | `Pacer::clear_to_send`, which reads the token before it reserves — it was read only inside the wait, so with nothing owed a canceled run kept sending |
| A push-back the body could not be read from is still a push-back | `IgClient::get` classifies from the status it already has when the body fails, rather than letting the read failure become a retryable network error |
| A credential is sent only to the address it was stored for | `plan`, for the token **and** the signing key, treating an absent or unparseable configured origin as a different destination |
| A calendar moment that went by is taken, not lost | `schedule::next_after` looks back from `now` to the floor before it looks forward |
| Jitter cannot cost the next run | `Schedule::room_for_jitter` bounds the grid — the gap less the floor between two runs, and no further than midnight when the days are restricted — applied to the default as well as to `--jitter`. `schedule::wake_at` narrows it again at each moment, in the zone, because seconds-of-day arithmetic is an hour too generous on the day a zone springs forward |
| One wall-clock moment is one run, in a zone that repeats an hour | `schedule::already_run_at_this_wall_clock`, asked in the direction ambiguity exists in |
| The rename cursor moves only over what this run could read | `engine::watch::compare` advances it only when every list the account has a capture of was accounted for, baselines included |
| A rename filed mid-comparison waits for the next window | `store::watch::renames_since` bounds above by the `head` the caller read first |
| Everything a scheduled run needs is checked while somebody is there | `snob watch check`, through `engine::check` — which takes `&App`, so it cannot record, and walks no list |
| Whether the monitor is working is an answer, not a reading | `watch_setup::health`, in `status`'s output and in its exit code |
| A write is paid for out of the write budget, and there is no other way to send one | `Pacer::clear_to_send_write`, inside `IgClient::post`, which is the only function in the workspace that sends a method other than GET to Instagram |
| A write is never replayed by a redirect | the POST client is built on `redirect::Policy::none()` — following a hop on a write means doing the thing twice, which is not what "follow the redirect" costs on a read |
| A write without a CSRF token is refused before it is sent | `IgClient::post` returns `IgError::NoCsrfToken` on an absent token rather than sending a request that will fail, so a `--paste` session cannot spend budget discovering it cannot write |
| Nothing tells anybody you looked at their story | `crates/snob-core/tests/no_seen.rs` reads the source of all three crates for `media/seen` and its spellings. There is no function to call, and the guard is there so that adding one is a test failure rather than a code review |

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
- **The wire signature is chosen for portability, and is not to be tuned to
  imitate anything** — the TLS handshake, the HTTP/2 SETTINGS and the header
  order alike. The reason written here used to be that there is nothing to
  imitate, because Chrome has randomized its ClientHello extension order since
  v110. That premise is true and the conclusion stopped holding in 2023: JA4
  sorts the extension list before hashing it, exactly so the shuffling changes
  nothing. The reasons that do hold are that copying a browser's cryptographic
  identity is detection evasion rather than honesty — and this tool exists to
  lower a real account's risk, not to be harder to recognize as a program — Trying would mean
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

Every command works and has been exercised against the live API **except the
three added with the write regime**, which are covered below. Two things are
deliberately unfinished:

- **`snob stories` works and has been run against Instagram.**
  `GET /api/v1/feed/reels_media/?reel_ids=` answers on **`www.instagram.com`**
  with a web session — which was the open question, because every write-up
  documents that path against `i.instagram.com` and the header of `client.rs`
  says why this crate does not go there. Confirmed in August 2026 against a real
  account with stories up: the items, their sizes, `taken_at` and `expiring_at`
  all arrive, and the largest candidate is the one taken.
- **`snob follow` and `snob unfollow` reach Instagram and do not work yet.**
  Everything around the request is exercised and sound — the write budget, the
  confirmation, the refusal without a CSRF token before anything is spent, the
  refusal to follow a redirect — and the request itself is refused. Two
  spellings were sent live and the relationship read back after each; neither
  changed anything:
  - `POST /api/v1/friendships/create/{pk}/` answers **200 with the web app's
    HTML shell**, which is what that edge serves for a path its API router did
    not accept. No status to classify and no message to read.
  - `POST /web/friendships/{pk}/follow/`, the older web route the Python
    clients use, answers **404**. It is gone.

  The one unexamined difference from what a browser sends is
  `x-web-session-id`. It has not been added on a guess: inventing a header a
  browser derives from its own session is the disguise the header rules exist
  to avoid, and each failed write is a request that looks like probing.
  **The next step is a capture of the real request from a logged-in browser**,
  not another attempt. `IgClient::friendship` carries the same record.
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
    what `watch_setup::health` reads, and without the exemption a monitor whose
    session expired would age out of "the last run ended in no_session" and exit
    1 into "it has not run yet" and exit 0, which is a probe going green while
    nothing was fixed. The outbox settles itself: `deliveries::expire_stale`
    gives up on a report that reached a day, and `forget_settled` forgets a
    delivered one after seven.

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
| `rust_xlsxwriter` + `zopfli` | ~498 KiB, 6.6%, for one of five output formats |
| Static CRT on Windows | +126,976 B per binary |
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

- **`login --browser` leaves an unauthenticated debugging port open**, on
  loopback, for up to `LOGIN_TIMEOUT`. Demonstrated end to end: a second local
  process read the port from `DevToolsActivePort`, called `/json/version` with
  no credential, and got the session cookie back from `Storage.getCookies`
  despite `httpOnly`. Loopback sockets have no per-user access control, so this
  is every account on the machine. `--remote-debugging-pipe` is the fix and it
  was demonstrated working on Windows — the platform is not the obstacle;
  `std::process::Command` is, because it does not expose the `lpReserved2`
  handle-inheritance blob. Roughly 250 lines of unsafe plus a Unix path, and it
  retires `tokio-tungstenite` and `futures-util`. Pair it with a job object
  carrying `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, which also covers the case no
  handler can catch: snob killed from outside, browser and port left running.
- **`keyring-core` and the platform stores.** The keyring project split in
  2026 and `keyring 4.x` now calls itself sample code, pointing applications at
  `keyring-core` plus a store. Two things arrive together:
  `windows-native-keyring-store` with `default-features = false` drops `regex`
  and `aho-corasick` — 863,744 bytes, 11.4% of both Windows binaries, for a
  search API nothing calls — and its `persistence` modifier gives
  `CRED_PERSIST_LOCAL_MACHINE` through a supported interface, retiring the
  roaming-credential exposure below. **One thing must be answered against a
  real Credential Manager before shipping it**: an entry written as Enterprise
  has to still be found by a lookup under Local on the same target name, or the
  first save after an upgrade silently logs every existing user out. That is
  why this did not go into 0.2.0.
- **Redirect hops are followed without being paced or charged.** Fixed: a
  *refused* redirect no longer retries. Still owed: the hops that are followed
  go out unpaid, against the standing rule that every request is paid for. The
  shape is `Policy::none()` plus a bounded loop in `IgClient::get` charging
  `clear_to_send()` per hop.
- **The Windows data directory has no DACL of its own.** `create_private_dir`
  chmods 0700 on Unix and does nothing on Windows, on the assumption that
  `%LOCALAPPDATA%` already limits access. On a machine with a non-default
  profile ACL it does not, and the database — the whole follower history, in
  the clear — is readable by other local accounts. `session.json` is
  DPAPI-sealed, so this is about the database and the browser profile.
  `SetNamedSecurityInfoW` with `PROTECTED_DACL_SECURITY_INFORMATION` was
  verified working unprivileged. The defect is that the code asserts a property
  it does not enforce.
- **Ctrl+C during an in-flight request waits for the server.** Measured: a
  stop during a budget wait takes 1.13 s, and about nine interrupts in ten land
  there — but during a request the exit tracks the server's hold, up to
  `REQUEST_TIMEOUT`, or 254 s on a black-holed connection because `Network`
  retries. It matters most under a service manager, where a stalled request can
  outlast the stop grace period and the process is killed before it closes its
  snapshot. Ten lines of `tokio::select!` in `get`, and the cancel branch must
  return `Canceled` rather than falling into `Network`.
- **`Retry-After` is never read.** `classify` receives no headers. Reading it
  would make snob the only tool of its class that does — but nobody has
  established whether these endpoints send it. Log the header at `debug` on
  every push-back first, so a real run answers the question. When implemented
  it is a floor and never a ceiling: a server-named 30 s must not shorten the
  local cooldown.
- **The pacing rate has no reference behind it.** The best public figure for
  this endpoint family is instaloader's field-report guess of 75 requests per
  660 s for non-GraphQL; snob walks at 172, because the cadence was copied from
  a project that walks GraphQL. Not a proposal — at 8.8 s per request a
  235-page walk takes 34 minutes against a 900-second resume window, and one
  list describing one moment is worth more than the rate. It is recorded
  because it is the one number in the design with nothing behind it. The
  ubiquitous "200 calls per user per hour" is Meta's Graph API platform limit
  for graph.facebook.com and has nothing to do with these endpoints.
- **`friendships/show_many`** would take a 1000/500 crossing from 52 requests
  to 14. It is a POST, which this project has never sent, and its page limit,
  response shape and throttle weighting are all unverified. Settle whether a
  non-mutating POST is inside the no-write rule before designing anything.
- **Reproducibility is 24 bytes away.** Two release builds differed only in the
  PE TimeDateStamp and the CodeView GUID; `-Clink-arg=/Brepro` plus
  `--remap-path-prefix` made them identical and took 46,080 bytes off. The gap
  is the archives: `tar -czf` and `Compress-Archive` both embed mtimes, while
  the `.deb` is already reproducible. Pin `rust-toolchain.toml` to an exact
  version first — it says `stable`, so a rebuild months later cannot match by
  construction. Remember that `RUSTFLAGS` replaces `.cargo/config.toml`.
- **Narrowing the trust store** is available and should be opt-in, not default.
  reqwest 0.13 made `rustls-platform-verifier` the default, so the four rustls
  targets now honor enterprise roots and a managed laptop with an inspection
  root can read the session in transit. `tls_certs_only(webpki_root_certs)`
  behind `--strict-roots`, with `--tls-extra-root` as the way out, and never on
  the webhook client, where a private CA is legitimate. Certificate **pinning**
  is separately rejected: Meta rotates leaves across issuers and there is no
  fast update channel behind this binary.

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
