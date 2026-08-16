# Changelog

## Unreleased

**`snob watch diff` says what has changed since the last time it was asked.**
The first half of the monitor. It reads what is already stored, so it spends no
requests and costs nothing to run: who has started following you and who has
stopped, who you have followed and unfollowed, and who now goes by a different
name. That last one comes out of a history the tool has been keeping on every
walk since the first release and had never shown anybody.

It needs something to compare against, so the first run on an account reports
nothing and says so rather than announcing your whole follower list as new
arrivals. Walk a list once — `snob followers`, `snob unfollowers`, anything —
and it has something to say from then on. `--json` for a script; down a pipe,
`snob watch diff --json | jq`. Looking does not count as reporting: ask twice
and you get the same answer.

The scheduled run and the webhook are not built yet.

Four things here change what a script sees, so they come first:

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

And the corrections worth knowing about:

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
