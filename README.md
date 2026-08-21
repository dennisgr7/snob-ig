# snob

Instagram from the terminal.

It walks your followers and your following, crosses them, and answers the
questions the app will not: who does not follow you back, who you never followed
back, and who you and somebody else both know. It can also pull a profile
picture at full size.

One binary, no runtime, nothing to install alongside it. Windows and Linux on
x86_64 and ARM64, macOS on Apple Silicon.

**snob only ever reads.** It never follows, unfollows, blocks or removes anyone.

> **Early version.** Every command works and has been used against the real API.
> Reading Instagram's own data export is planned and not built yet.

## Install

**Windows**, with [Scoop][scoop]:

```bash
scoop bucket add snob https://github.com/dennisgr7/snob-ig
scoop install snob
```

**macOS and Linux**, with [Homebrew][brew]:

```bash
brew tap dennisgr7/snob https://github.com/dennisgr7/snob-ig
brew install snob
```

**Debian and Ubuntu** — download the `.deb` for your architecture from the
[releases page][releases] and:

```bash
# Check it against the published sums first, as every other channel here does.
curl -fsSLO https://github.com/dennisgr7/snob-ig/releases/latest/download/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
sudo apt install ./snob-v<version>-x86_64-unknown-linux-musl.deb
```

The Linux builds are statically linked, so they carry no glibc version
requirement and run on any distribution.

**Without a package manager.** These download the release for your platform and
check it against the published SHA256. The PowerShell one adds the install
directory to your user `PATH`; the shell one prints the line to add to your
profile, because which file that is depends on the shell you use:

```bash
curl -fsSL https://raw.githubusercontent.com/dennisgr7/snob-ig/main/packaging/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/dennisgr7/snob-ig/main/packaging/install.ps1 | iex
```

Piping a script from the internet into a shell deserves the suspicion it gets:
both are short, and reading them first is the right instinct.

**From source**, with Rust installed:

```bash
cargo install --locked --git https://github.com/dennisgr7/snob-ig snob-cli
```

**By hand** — every release has an archive per platform on the [releases
page][releases]. Unpack it, put `snob` anywhere on your `PATH`, done. There is
no runtime to install.

### Updating

`scoop update snob`, `brew upgrade snob`, or run the install script again.
Installed by hand, there is nothing watching for you: check the [releases
page][releases].

### Uninstalling

Run **`snob purge`** first. The session, the database and the browser profile
live in your user directories, and no package manager can reach them — removing
the binary leaves an Instagram session cookie behind. Then `scoop uninstall
snob`, `brew uninstall snob`, `sudo apt remove snob`, or delete the file.

[releases]: https://github.com/dennisgr7/snob-ig/releases
[scoop]: https://scoop.sh
[brew]: https://brew.sh

## First run

```bash
snob login
```

**Your password is never involved.** Instagram identifies a browser by a cookie
called `sessionid`, and that cookie is all snob needs. There are two ways to
hand it over, and `snob login` offers both:

- **A browser it opens itself.** It launches Chrome, Edge or Brave with a
  profile of its own — never yours — you log in there as usual, and it takes the
  cookie the browser hands over through its debugging protocol.
- **Paste it.** You copy `sessionid` out of your browser's developer tools and
  paste it in. `snob login --paste` goes straight here, which is what a server
  with no desktop needs.

Check it worked with `snob whoami`. The session lasts until you log out of
Instagram from that browser, or Instagram expires it.

## What it does

```bash
snob unfollowers
```

```
Username     Full name        Attributes
someone      Some One
a.private    Private Account  private
a.verified   Verified Account verified
...
33 accounts you follow that do not follow you back - 33 of 139 - 12 requests
```

In a terminal that is a real table, with the usernames clickable where the
terminal supports it. Down a pipe the output turns into JSON on its own, so
something else can read it without being told to.

The other crossings are `snob fans` (they follow you, you do not follow them)
and `snob friends` (you follow each other). `snob followers` and
`snob following` print a list on its own.

```bash
snob scan
```

The whole picture in one go: both counts, all three crossings, and how much of
it came out of storage rather than off the network. Point it at somebody else
and it opens with the people you both know, worked out from what is already
stored rather than from a request:

```bash
snob scan someone
```

Reading somebody else's lists costs their account nothing, but it is still
somebody else's, so snob asks before it starts. `-y` answers in advance.

```bash
snob pfp someone -o picture.jpg
```

Their profile picture at 1080x1080, which is not the size the web page serves.

```bash
snob stories someone
```

What they have up right now, numbered, with what each one is and how long it
has left. `--download 2` saves the second one, `--all` saves all of them, and
`-i` opens a list you move through with the arrow keys — Enter opens the story
in whatever you already open pictures and videos with, `D` keeps a copy.

**None of that tells them you looked.** Instagram registers a view through a
separate request; snob does not make it, has no code that could, and a test
reads the whole source on every build to keep it that way.

```bash
snob unfollow someone
```

One of the two things snob changes, and it asks first. The other is
`snob follow`. One account per command — see [the risk](#the-risk-and-what-the-design-does-about-it)
for why there is no bulk mode — and they need a session with a CSRF token,
which `snob login --browser` picks up on its own. If you logged in by pasting,
`snob login --paste --csrftoken <token>` is how to add it.

Every list takes `--format json|ndjson|csv|xlsx|md` and `-o file`, filters like
`--only private` or `--no-verified`, and `--limit`. Run `snob --help` for the
rest.

A username can be written with or without a leading `@`. If you write it on
PowerShell, quote it — `"@someone"` — because an unquoted `@` is swallowed by
the shell before snob ever sees it.

## Watching over time

```bash
snob watch diff
```

What has changed since the last time the monitor looked: who started following
you and who stopped, who you followed and unfollowed, and who now goes by a
different name. It reads what is already stored, so it costs no requests and
running it twice gives the same answer.

The first time there is nothing to compare against, so it says so rather than
announcing your whole follower list as new arrivals. Walk a list once and it has
something to say from then on.

```bash
snob watch setup
```

Asks how often to look, how far each run may be pushed past its moment, and
where to send the reports, then writes a file you can edit. It finishes by trying all of it — the schedule, the session, that each
account resolves and can be read, and the webhook, by posting one message to it
— so a typo or an expired token turns up while you are still there rather than
in an unattended run at three in the morning. Then it offers to take the first
capture, telling you what that costs, because the first scheduled run otherwise
lays one down and reports nothing.

```bash
snob watch          # stays up and runs on the schedule
snob watch once     # one run, for cron or a systemd timer
snob watch check    # would a scheduled run work? exits non-zero if not
snob watch status   # what is configured, when it last ran, and whether it is healthy
```

`check` writes nothing and walks no list, so it is safe to repeat — but poll it
hourly rather than by the minute. It costs one request for the session, one per
watched account, and one more until the session has learned its own account's
name, and all of them come out of the same daily budget the walks draw on: a
probe that drains it causes the condition it is watching for. `--no-webhook`
leaves the receiver alone and checks everything else. Both it and `status` exit
non-zero when something would stop the monitor doing its job, which is what
makes them usable from a monitoring system rather than only readable.

Or say it directly: `--every 6h`, `--on mon,thu --at 09:00`, or
`--cron "0 9 * * 1,4"` if you already have one written. The two combine, so
`--every 2w --on mon` is one Monday in every two. Times are your local ones.

**A run with nothing to report costs one request.** It reads your counters and
only walks a list if its counter moved, which is what makes running it every few
hours reasonable. Each run is nudged a little past its due moment so the walks
do not start on the same second every day; `--jitter` says how far and
`--jitter 0` turns it off. A schedule whose runs are already as close together
as the tool allows has no room to be nudged, and none is taken.

### Sending it somewhere

```bash
snob watch --every 6h --webhook https://n8n.local/webhook/snob
```

Each report goes out as one JSON object — built so an automation can branch on
it without digging through arrays:

```json
{
  "schema": 1,
  "event": "watch.changes",
  "run": {
    "id": "1755612000-9f3c1a04",
    "at": 1755612000,
    "looked": true,
    "requests": 14,
    "lists": [
      { "kind": "followers", "skipped": null },
      { "kind": "following", "skipped": null }
    ],
    "tool": { "name": "snob", "version": "0.1.1" }
  },
  "account": { "pk": 1234567, "username": "you", "is_self": true },
  "counts": {
    "followers_gained": 1,
    "followers_lost": 2,
    "following_gained": 0,
    "following_lost": 0,
    "renamed": 1,
    "total": 4
  },
  "events": {
    "followers_lost": [
      { "pk": 7654321, "username": "someone", "profile_url": "https://www.instagram.com/someone/" }
    ]
  }
}
```

Each account in `events` is abbreviated above. What is actually sent is
everything snob knows about them — `pk`, `username`, `full_name`,
`is_private`, `is_verified` and `pfp_url` — plus the `profile_url` snob builds.
Worth knowing before you point this at a third-party automation service: the
report names real people, and `pfp_url` is a signed CDN address that anything
holding it can fetch until it expires.

`schema` is the version of this shape. It moves when a field is removed or
changes meaning and never when one is added, so a workflow written against 1
keeps working; every message carries it, including the one `snob watch check`
posts and every line of the `--json` stream.

`run.id` is the value to deduplicate on: delivery is at-least-once and a retry
carries the same one. `run.at` is when the run concluded, in epoch seconds, and
`run.lists` says which of the two lists it actually read — a `skipped` of
`not_verified` or `incomplete` means that list was not compared, so its zeros in
`counts` mean "not looked at" rather than "nothing happened". `events` holds one
array per kind of change and is shown here with one of them; `lists.followers`
and `lists.following`, left out above, carry each list's basis, how many accounts
it holds, and two moments, or `null` for a list this run did not compare. The two
moments are not the ends of a window and one is not always before the other:
`since` is when this list was last *reported* on and `until` is when the capture
being reported was *taken*. A run that walked the list has `since` before
`until`; a run that found the counters unmoved and served the list out of storage
has it the other way round, because the receipt was written after the capture it
points at.

`--header "Authorization: Bearer …"` for an endpoint that wants one, and
`--sign-with` to have the body signed with HMAC-SHA256 in an `X-Snob-Signature`
header, so the receiver can check it came from you. `snob watch setup` puts both
in the keyring instead, which is what lets a systemd unit hold neither.

Three more headers travel with every POST, and a receiver can route on them
without parsing the body at all:

| Header | What it is |
|---|---|
| `X-Snob-Event` | `watch.changes` or `watch.preflight` — the same value as `event` in the body. |
| `X-Snob-Delivery` | The delivery id: the same across every retry of one report. Convenient for dropping a duplicate before you parse anything — but **deduplicate on `run.id` in the body**, which is the same value and is the one the signature covers. A header is not signed and anything on the path can set it. |
| `X-Snob-Attempt` | Which try this is, counting from 1. |

Because those four are the protocol, snob refuses to send a `[webhook.headers]`
entry or a `--header` that sets any name beginning `X-Snob-`: a configured copy
would make the value ambiguous, and most frameworks join duplicates with `, `.

Two more things a receiver should do, both using values already inside the
signature: reject a report whose `run.at` is more than a day old, since nothing
older than that is ever sent, and treat `event` in the body rather than
`X-Snob-Event` as the authority on what arrived.

Nothing is sent when nothing changed, so every message that arrives means
something; `--heartbeat` sends one anyway, for when silence is the signal you
are watching. A report that cannot be delivered is queued and retried, and it is
queued *before* the monitor moves on — a receiver that was restarting does not
cost you the change. It remembers the address it was made for, so pointing a run
somewhere else with `--webhook` does not flush your backlog, or your stored
token, to that address. Plain `http://` is refused unless the address is on your
own network, because the report carries account names and any token travels with
it.

No webhook at all is a complete way to use this:

```bash
snob watch --every 6h --json >> events.ndjson
```

## Where your data goes

Nowhere. There is no server, no account and no telemetry: snob talks to
Instagram and to nothing else.

On your machine it keeps two things, both under your user profile and both
per-user rather than per-folder, so it does not matter which directory you run
it from:

- **The session**, in the operating system's keyring — Credential Manager,
  Keychain or Secret Service. With `--no-keyring` it goes to a file instead,
  encrypted with DPAPI on Windows and readable only by you on Unix. On a machine
  with no keyring at all, like a server or a container, `snob login` notices and
  uses the file, telling you it did.
- **A local SQLite database** of the lists it has walked, so that asking the
  same question twice does not cost twice the requests. The monitor expires
  captures older than a month; the newest of each list, and whatever it last
  reported against, are always kept.
- **The monitor's settings**, if you ran `snob watch setup` — a `watch.toml` you
  can read and edit. Any token or signing key it needs goes to the keyring
  rather than into that file.

**Your browser's own cookie store is never read, copied or decrypted.** The
browser `snob login` opens is a separate one with a profile of its own. That
does mean the session then also exists inside that profile;
`snob logout --purge-profile` removes it along with the stored one.

To take everything off the machine, including the keyring entry that no package
manager can reach:

```bash
snob purge
```

It shows you the list and asks before deleting anything. Then remove the binary
however you installed it.

## The risk, and what the design does about it

There is no official API for any of this — Meta removed the followers endpoint
in 2018 — so snob uses the private web API with your own session. That goes
against Instagram's Terms of Use, and the realistic consequence for an
individual is a verification checkpoint on their account.

Most of the design exists to make that unlikely:

- **It writes two things, and nothing else.** `snob follow` and `snob unfollow`,
  one account per command. No block, no remove-follower, no like, no comment,
  no message, and nothing that marks a story as seen. Both ask before they send,
  both come out of a budget of their own that allows one action every fifteen
  minutes and at most three in a row, and there is **no bulk mode and no flag
  that makes one**. That is deliberate rather than unfinished: what Instagram
  acts on is not the day's total but the burst, and the follow-then-unfollow
  churn a tool like this makes easy to automate is the specific pattern its
  detection was built for. Writing your own loop around it is your business;
  shipping you the loop is not something snob will do.
- **Requests are paced**, with the timings borrowed from
  [InstagramUnfollowers][iu], which has years of real use behind it, and only
  ever adjusted downwards. Nothing in snob can send a request without paying for
  it first.
- **The first refusal stops the run.** A 429, a `feedback_required` or a
  challenge ends it immediately and puts the account in cooldown. There is no
  retry loop: when a service says no, the answer is to stop asking, and pushing
  on is also how a momentary limit becomes a lasting one.
- **Nothing is asked twice.** A recent list is reused from storage instead of
  walked again, and an interrupted walk resumes rather than starting over.
- **The requests are well-formed.** The headers are derived from a browser
  actually installed on the machine, so they agree with each other instead of
  describing something contradictory.

One part of this is not snob's to control, and it is the part that matters
most. The single strongest signal Instagram has is **where the requests come
from**: a home connection is treated very differently from a datacenter one,
and the same endpoint that answers normally from a laptop can answer 429 on the
very first request from a cloud address. So:

- Run it from the connection you normally browse from.
- A VPS, a VPN or a public proxy raises the odds of a checkpoint and can
  shorten the life of the session. Running it in a homelab is supported and
  works; it is not risk-free in the way running it on your own desktop is.
- Try not to have the session in two places at once — snob on a server while
  you browse Instagram at home is the kind of split Instagram notices.

None of that is a guarantee, and it is not offered as one. Walking a list of
several thousand costs hundreds of requests however carefully they are spaced.
Use it knowing that.

## Exit codes

Stable, and meant for scripts: the point of them is to tell "log in again"
apart from "wait a while" without reading the message text.

| Code | Meaning |
|---|---|
| 0 | It worked. A list cut short by `--limit` or `--max-pages` is still a 0. |
| 1 | It failed, with nothing more specific to say — including a result refused because a list came back incomplete. |
| 2 | The command line could not be parsed. Nothing was done, and running it again unchanged will not help. |
| 3 | No session stored, or the one there no longer works. Run `snob login`. |
| 4 | Instagram wants the account verified. Open the address it prints. |
| 5 | Instagram is throttling, or the account is in cooldown. Wait. |
| 130 | Stopped by you: Ctrl+C, or a confirmation that was not given — including with no terminal to ask at, where `-y` confirms in advance. |

`followers` and `following` print what they got even when the walk was cut
short, because a partial list is still true as far as it goes — but they still
exit with the code of whatever stopped them. Only a cap you asked for,
`--limit` or `--max-pages`, is a 0; Instagram refusing to serve the rest of a
list is a 1, and throttling is a 5. Something has to be able to tell those
apart, and the printed names cannot.

`unfollowers`, `fans` and `friends` cross two lists, and the two halves are not
the same question. The list being crossed **against** has to be whole: an
account missing from it shows up in the answer without deserving to, which is
wrong rather than short, so that one refuses outright and exits with whatever
stopped it. The list the results come **out of** is the ordinary case — the
answer is short but every name in it is true — so it prints with a warning and
follows the rule above, cap you asked for included. `scan` needs both lists
whole, because each of its five numbers leans on both, and refuses either way.

## Inspiration

The idea comes from [InstagramUnfollowers][iu] by David Arroyo (MIT), which does
the same thing from the browser console. snob shares no code with it.

[iu]: https://github.com/davidarroyo1234/InstagramUnfollowers

## License

MIT.
