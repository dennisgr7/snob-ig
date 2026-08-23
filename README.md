# snob

Instagram from the terminal.

It walks your followers and your following, crosses them, and answers the
questions the app will not: who does not follow you back, who you never followed
back, and who you and somebody else both know. It can also pull a profile
picture at full size, and show or save the stories an account has up.

All of it is work you can already do by hand in the app, for as long as you have
the patience: scroll two lists, compare them, long-press a picture. snob is the
same work done in a terminal — quicker, scriptable, and with the answer in a
format you can keep. It is the housekeeping the app has never put a button on,
at the size one person's account actually is.

One binary, no runtime, nothing to install alongside it. Windows and Linux on
x86_64 and ARM64, macOS on Apple Silicon.

**Almost all of snob reads.** It changes exactly two things, one account per
command and after asking: `snob follow` and `snob unfollow`. It never blocks,
never removes a follower, never likes, comments or messages, and never marks a
story as seen.

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
# And, with GitHub CLI, that it came out of this repository's release pipeline:
gh attestation verify snob-v<version>-x86_64-unknown-linux-musl.deb --repo dennisgr7/snob-ig
sudo apt install ./snob-v<version>-x86_64-unknown-linux-musl.deb
```

The sums say a file is the one the release page lists; the attestation says
who built it, which the sums cannot, because they are published on the same
page. The install scripts below run the same check when `gh` is installed.

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
snob profile someone
```

```
@someone  Some One  (private)
  a line of bio

Followers:    244
Following:    319
Posts:        0

You follow them, they follow you
Followed by @ana, @luis, @eva and 30 others:
  @ana @luis @eva @pat ...

Highlights:   2
   1  trip   5 items, updated Jul 13 at 10:53
   2  home   6 items, updated Feb 9 at 10:08
Stories up:   none
profile of @someone - 6 requests
```

What you would see opening the profile, and nothing walked: the counters, the
bio, whether you follow each other, the accounts you follow that follow them,
the highlights and whether anything is up right now. Three or four requests
for most accounts, one more per twelve people you have in common. Without a
name it is your own page. `--format json` for a script, `--format md` for a
note.

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

Everything about somebody else is what their profile already shows to anyone
signed in — snob only reads it faster. It is still their account rather than
yours, so snob asks before it starts on one. `-y` answers in advance.

```bash
snob pfp someone -o picture.jpg
```

Their profile picture at 1080x1080, which is not the size the web page serves.

```bash
snob stories someone
```

What they have up right now, numbered, with what each one is and how long it
has left. `--download 2` saves the second one, `--download 1,3` or
`--download 2-4` a set, `--download all` the whole tray, and `-i` opens a list
you move through with the arrow keys — Enter opens the story in whatever you
already open pictures and videos with, `D` keeps a copy.

**Saving a story does not mark it as seen.** Instagram registers a view with a
separate request, and that request is a write — so it falls under the two-write
rule below, and there is no code here that could send it. A test reads the whole
source on every build to keep it that way. It follows from snob being a
downloader rather than a viewer: nothing you do here lands in somebody's viewer
list, in either direction.

```bash
snob unfollow someone
```

One of the two things snob changes, and it asks first. The other is
`snob follow`. One account per command — see [staying a light client](#staying-a-light-client)
for why there is no bulk mode — and they need a session with a CSRF token,
which `snob login --browser` picks up on its own. If you logged in by pasting,
`snob login --paste --csrftoken <token>` is how to add it.

Every list takes `--format json|ndjson|csv|xlsx|md` and `-o file`, filters like
`--only private` or `--hide verified`, and `--limit`. Run `snob --help` for the
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

## Staying a light client

There is no official API for any of this — Meta removed the followers endpoint
in 2018 — so snob asks the same web API the instagram.com page in your browser
asks, signed in as you. Automating that is outside Instagram's Terms of Use, as
it is for every tool in this category, and the realistic consequence for one
person reading their own lists is that Instagram asks the account to verify
itself.

So most of the design goes into being an unremarkable client — one that asks for
what it needs, at a rate the service can absorb, and stops the moment it is told
to:

- **It writes two things, and nothing else.** `snob follow` and `snob unfollow`,
  one account per command. No block, no remove-follower, no like, no comment,
  no message, and nothing that marks a story as seen. Both ask before they send,
  both come out of a budget of their own that allows one action every fifteen
  minutes and at most three in a row, and there is **no bulk mode and no flag
  that makes one**. That is deliberate rather than unfinished. What strains a
  service is not the day's total but the burst — and the follow-then-unfollow
  churn that automating a list makes easy is a growth-hacking trick, not
  housekeeping, and not what this is for. Writing your own loop around it is
  your business; shipping you the loop is not something snob will do.
- **Requests are paced**, with the timings borrowed from
  [InstagramUnfollowers][iu], which has years of real use behind it, and only
  ever adjusted downwards. Nothing in snob can send a request without paying for
  it first.
- **The first refusal stops the run.** A 429, a `feedback_required` or a
  challenge ends it immediately and puts the account in cooldown. There is no
  retry loop: when a service says no, the answer is to stop asking, and pushing
  on is also how a momentary limit becomes a lasting one.
- **Nothing is asked twice.** A recent list is reused from storage instead of
  walked again, and an interrupted walk resumes rather than starting over. The
  cheapest request is the one that is never sent.
- **The requests are well-formed.** The headers are derived from a browser
  actually installed on the machine, so they agree with each other instead of
  describing something that does not exist. snob does not dress itself up as a
  browser; it just does not send a self-contradictory request.

One thing here is not snob's to control, and it decides more than any of the
above: **where the requests come from**. Instagram serves a home connection and
a datacenter one very differently, and the same endpoint that answers a laptop
normally can answer 429 on the first request from a cloud address. That is a
practical limit on where this runs usefully, so:

- Run it from the connection you normally browse from, and it behaves.
- On a VPS, behind a VPN or through a public proxy, expect more throttling and
  shorter-lived sessions. A homelab is supported and works well; a rented cloud
  box often does not, and snob has no way around that and does not go looking
  for one.
- Keep one session in one place. Instagram treats an account that appears from
  two networks at once as worth a second look, and it is not wrong to.

None of that is a guarantee, and it is not offered as one. Walking a list of
several thousand costs hundreds of requests however carefully they are spaced,
and that is real load on somebody else's service. Ask for it when you want the
answer, not on a loop.

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
