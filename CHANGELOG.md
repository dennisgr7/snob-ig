# Changelog

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
