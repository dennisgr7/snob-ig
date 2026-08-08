# snob

Instagram from the terminal.

It walks your followers and your following, crosses them, and answers the
questions the app will not: who does not follow you back, who you never followed
back, and who you and somebody else both know. It can also pull a profile
picture at full size.

One binary, no runtime, nothing to install alongside it. Windows and Linux on
x86_64 and ARM64, macOS on Apple Silicon.

**snob only ever reads.** It never follows, unfollows, blocks or removes anyone.

> **Early version.** Every command works and has been used against the real API,
> but this is the essentials and no more. Watching an account over time and
> reading Instagram's own data export are planned and not built yet.

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
cargo install --git https://github.com/dennisgr7/snob-ig snob-cli
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
mattfrs      Matt Frears
kellyjrd     Kelly J.         private
rosanieves   Rosa Nieves      verified
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

Every list takes `--format json|ndjson|csv|xlsx|md` and `-o file`, filters like
`--only private` or `--no-verified`, and `--limit`. Run `snob --help` for the
rest.

A username can be written with or without a leading `@`. If you write it on
PowerShell, quote it — `"@someone"` — because an unquoted `@` is swallowed by
the shell before snob ever sees it.

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
  same question twice does not cost twice the requests.

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

## Staying out of trouble

There is no official API for any of this — Meta removed the followers endpoint
in 2018 — so snob uses the private web API with your own session. That goes
against Instagram's Terms of Use, and the realistic consequence for an
individual is a verification checkpoint on their account.

Most of the design exists to make that unlikely:

- **It never writes.** No follow, unfollow, block or remove-follower, ever. The
  operations that get accounts banned are not in the tool at all.
- **Requests are paced**, with the timings borrowed from
  [InstagramUnfollowers][iu], which has years of real use behind it, and only
  ever adjusted downwards. Nothing in snob can send a request without paying for
  it first.
- **The first sign of trouble stops the run.** A 429, a `feedback_required` or a
  challenge ends it immediately and puts the account in cooldown; there is no
  retry loop, because a retry loop is how an account gets flagged.
- **Nothing is asked twice.** A recent list is reused from storage instead of
  walked again, and an interrupted walk resumes rather than starting over.
- **The headers match a browser** that is actually installed on the machine,
  rather than announcing something no browser sends.

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
| 3 | No session stored, or the one there no longer works. Run `snob login`. |
| 4 | Instagram wants the account verified. Open the address it prints. |
| 5 | Instagram is throttling, or the account is in cooldown. Wait. |
| 130 | Interrupted with Ctrl+C. |

`followers` and `following` print what they got and exit 0 even when the walk
was cut short, because a partial list is still true as far as it goes.
`unfollowers`, `fans`, `friends` and `scan` cross two lists, so an incomplete
one there makes the answer wrong rather than short — those refuse, and exit
with the code of whatever stopped them.

## Inspiration

The idea comes from [InstagramUnfollowers][iu] by David Arroyo (MIT), which does
the same thing from the browser console. snob shares no code with it.

[iu]: https://github.com/davidarroyo1234/InstagramUnfollowers

## License

MIT.
