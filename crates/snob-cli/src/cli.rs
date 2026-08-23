use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "snob",
    // Without this, the help on Windows announces "snob.exe", which is not how
    // the command is typed.
    bin_name = "snob",
    version,
    about = "Instagram from the terminal",
    long_about = "Instagram from the terminal.\n\n\
                  Walks your followers and your following, crosses them, and answers who \
                  does not follow you back, who you never followed back, and who you and \
                  somebody else both know. It also shows and downloads the stories an \
                  account has up.\n\n\
                  It changes exactly two things and asks first about both: \"follow\" and \
                  \"unfollow\", one account at a time. It never blocks, never removes a \
                  follower, and never tells anybody you looked at their story.",
    after_help = EXAMPLES
)]
pub struct Cli {
    /// Store the session in a protected file instead of the system keyring.
    /// For environments without a desktop session.
    // Ordered last, with `verbose`. Both are global, so without this they are
    // propagated into every subcommand and land in the middle of its own
    // options, splitting a list that reads in a deliberate order.
    #[arg(long, global = true, display_order = 900)]
    pub no_keyring: bool,

    /// Show diagnostic traces
    #[arg(long, global = true, display_order = 901)]
    pub verbose: bool,

    /// Check Instagram's certificate against Mozilla's roots only
    ///
    /// Off by default, and that default is deliberate rather than lazy.
    /// reqwest 0.13 made the platform verifier the default, so snob honors
    /// whatever roots an administrator has installed — which is what makes it
    /// work on a managed machine, and is also how a laptop carrying a
    /// TLS-inspecting root lets that middlebox read the session in transit.
    /// This ends the second at the cost of the first, which is a trade only the
    /// person running it can make.
    ///
    /// It refuses on Windows for ARM64, where the TLS backend is schannel and
    /// has no way to express "these roots and no others". A security flag that
    /// silently does nothing is worse than one that is not offered.
    ///
    /// **Never applied to the webhook.** A private CA in front of somebody's
    /// own receiver is legitimate, and `snob_ig::http::plain` has no argument
    /// for this — the same shape that stops the session reaching a webhook.
    #[arg(long, global = true, display_order = 904)]
    pub strict_roots: bool,

    /// Also trust the certificates in this PEM file
    ///
    /// The way back out of `--strict-roots`, and it requires it: on the
    /// platform store there is nothing to add to, because whatever an
    /// administrator installed is already trusted. Repeatable.
    #[arg(
        long,
        global = true,
        display_order = 905,
        requires = "strict_roots",
        value_name = "PEM"
    )]
    pub tls_extra_root: Vec<PathBuf>,

    /// Keep every file this run reads or writes under this directory
    ///
    /// A testing build only. Replaces the discovered data and configuration
    /// directories, puts the session in a file inside it rather than in the
    /// system keyring, **and gives the run a keyring namespace of its own** —
    /// so a sandbox run cannot read, write or delete the real one. That is the
    /// property [`Cli::ig_base_url`] leans on.
    ///
    /// All three, and the third is not decoration. The store reaches the
    /// keyring whatever backend it is on, so with the real service name a
    /// sandbox `login` deleted the developer's session and a sandbox that had
    /// not logged in yet loaded the real cookie — which is the exact thing the
    /// pairing below exists to prevent. `main::wiring` is where the namespace
    /// is assigned and says the rest.
    #[cfg(feature = "testing")]
    #[arg(long, global = true, hide = true, display_order = 902)]
    pub sandbox_root: Option<std::path::PathBuf>,

    /// Ask this server instead of Instagram
    ///
    /// A testing build only, and it **requires `--sandbox-root`**. That is the
    /// whole safety argument, and it is enforced by clap rather than described:
    /// a redirected client can only ever carry a session out of a store inside
    /// the sandbox root, so the session belonging to the person running this is
    /// not reachable from a redirected run. Without that pairing the flag would
    /// be a way to send a real session cookie to somebody else's server.
    ///
    /// Nothing about it is loopback-only, deliberately. `IgClient::is_live`
    /// decides whether the pace is real by address, so a proxy on `127.0.0.1`
    /// forwarding to Instagram would be a test server by address and Instagram
    /// by content — a real account walked with no waits between pages. A
    /// loopback restriction would look like the safe option and be the
    /// dangerous one; an empty sandbox store is the thing that actually helps.
    #[cfg(feature = "testing")]
    #[arg(
        long,
        global = true,
        hide = true,
        display_order = 903,
        requires = "sandbox_root",
        value_name = "URL"
    )]
    pub ig_base_url: Option<url::Url>,

    #[command(subcommand)]
    pub command: Command,
}

/// A shared secret long enough to be worth having.
///
/// `--sign-with a` was accepted and produced a well-formed signature that
/// anybody could reproduce. The reason that matters is already written down at
/// `delivery.rs`: handing a third party a body and its MAC gives them
/// everything they need to guess a human-chosen secret offline, at whatever
/// rate their hardware allows. HMAC-SHA256 accepts a key of any length, so
/// nothing below this would have complained.
///
/// Thirty-two characters is the shortest that is not a guessing target. It is
/// counted in characters rather than bytes because the person typing it is
/// counting characters.
fn signing_secret(value: &str) -> Result<String, String> {
    const FLOOR: usize = 32;
    let length = value.chars().count();
    if length < FLOOR {
        return Err(format!(
            concat!(
                "a signing secret has to be at least {FLOOR} characters and this one is ",
                "{length}. A short one can be guessed offline by anybody who has been ",
                "sent one signed report. Generate one instead -- ",
                "\"openssl rand -hex 32\", or ",
                "\"python -c \\\"import secrets; print(secrets.token_hex(32))\\\"\"." // Named rather than captured: a `format!` cannot reach an identifier
                                                                                      // through a `concat!`, and `concat!` is what keeps `cargo fmt` from
                                                                                      // rejoining these lines and leaving the indentation inside the string.
            ),
            FLOOR = FLOOR,
            length = length
        ));
    }
    Ok(value.to_string())
}

/// Shown under the option list.
///
/// The note about quoting is not decoration. On PowerShell `@` is the splatting
/// operator, so an unquoted `@name` is consumed by the shell and never reaches
/// this program, which then answers about the user's own account instead. It
/// cannot be detected from here, so the only place to say it is the help, and
/// none of the examples above it are written that way.
const EXAMPLES: &str = "\
Examples:
  snob login                          store your session, once
  snob unfollowers                    who does not follow you back
  snob profile someone                their page: counts, bio, who you both know
  snob scan someone                   the full picture of another account
  snob pfp someone -o picture.jpg     their profile picture, at full size
  snob stories someone                what they have up right now
  snob stories someone -i             move through it with the arrow keys
  snob unfollow someone               the one thing snob changes, after asking
  snob unfollowers --format csv -o unfollowers.csv

A username may be written with or without a leading @. If you write the @, quote
it (\"@someone\"): on PowerShell an unquoted one is eaten by the shell.

Exit codes:
  0   it worked; a list cut short by --limit or --max-pages is still a 0
  1   it failed, or a result was refused because a list came back incomplete
  2   the command line could not be parsed; nothing was done, and running it
      again unchanged will not help
  3   no session, or the stored one no longer works -- run \"snob login\"
  4   Instagram wants the account verified -- open the address it prints
  5   Instagram is throttling, or the account is in cooldown -- wait
  130 stopped by you: Ctrl+C, or a confirmation not given -- including with
      no terminal to ask at, where -y confirms in advance

Environment:
  NO_COLOR          no styling, whatever the terminal supports
  CLICOLOR_FORCE    styling even where stdout is not a terminal
  FORCE_HYPERLINK   OSC 8 hyperlinks even where they were not detected
  SNOB_LOG          what --verbose shows, as target=level pairs
  SNOB_CSRFTOKEN, SNOB_SIGNING_KEY
                    the two secrets a command line would otherwise carry;
                    see \"snob login --help\" and \"snob watch --help\"";

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Store your Instagram session in the system keyring
    Login(LoginArgs),

    /// Show which account you are authenticated as
    Whoami(WhoamiArgs),

    /// Delete the stored session
    #[command(
        after_help = "This removes the session and nothing else. To clear everything snob \
                      has stored on this computer, use \"snob purge\"."
    )]
    Logout(LogoutArgs),

    /// Delete everything snob has stored on this computer, before uninstalling
    #[command(
        after_help = "The session, the database and the browser profile. The binary itself \
                      is left alone: uninstall it with whatever installed it."
    )]
    Purge(PurgeArgs),

    /// An account as its page shows it: counts, bio, who you both know, highlights
    #[command(
        after_help = "What you would see opening the profile, for three or four requests: the \
                      counters, the bio, whether you follow each other, the accounts you follow \
                      that follow them, the highlights and whether anything is up right now. It \
                      walks no list and stores nothing. \"snob scan\" is the crossing, and \
                      costs both lists."
    )]
    Profile(ProfileArgs),

    /// Summary of the whole account: followers, following, and how they cross
    Scan(ScanArgs),

    /// Accounts you follow that do not follow you back
    Unfollowers(ListArgs),

    /// Accounts that follow you and you do not follow
    Fans(ListArgs),

    /// Accounts you and they follow each other
    // `mutuals` was the name until the rename; kept hidden so anything written
    // against the earlier name still runs rather than failing at the shell.
    #[command(alias = "mutuals")]
    Friends(ListArgs),

    /// Your followers
    Followers(ListArgs),

    /// The accounts you follow
    Following(ListArgs),

    /// Download a profile picture in high resolution
    Pfp(PfpArgs),

    /// Show the stories an account has up, and download them
    #[command(
        after_help = "Listing and downloading a story does not tell the account you looked. \
                      snob has no way of doing that and a test keeps it that way."
    )]
    Stories(StoriesArgs),

    /// Follow an account
    #[command(
        after_help = "One account per run, on purpose: what Instagram acts on is a burst of \
                      follows rather than the day's total. Needs a session with a CSRF token, \
                      which \"snob login --browser\" captures."
    )]
    Follow(FollowArgs),

    /// Unfollow an account
    #[command(
        after_help = "One account per run, on purpose: what Instagram acts on is a burst of \
                      unfollows rather than the day's total. Needs a session with a CSRF token, \
                      which \"snob login --browser\" captures."
    )]
    Unfollow(FollowArgs),

    /// Track an account over time and report what changed
    Watch(WatchArgs),
    // `import dyi` is written and tested but not wired up here on purpose: the
    // reader works, and what is unfinished is the question of what an import
    // should be able to do once it is in. Leaving it out of the CLI keeps the
    // published surface to what has been settled. See `commands::import`.
}

#[derive(Args, Debug)]
#[command(group = clap::ArgGroup::new("method").multiple(false))]
pub struct LoginArgs {
    /// Paste the sessionid copied from the developer tools
    #[arg(long, group = "method")]
    pub paste: bool,

    /// Open a browser, wait for you to log in, and capture the session
    #[arg(long, group = "method")]
    pub browser: bool,

    /// User-Agent of the session. If omitted, it is worked out from the
    /// installed browser. It has to match or Instagram will reject the session.
    #[arg(long, value_name = "STRING")]
    pub user_agent: Option<String>,

    /// The csrftoken cookie, alongside the pasted sessionid
    ///
    /// Only "follow" and "unfollow" need it; everything else reads, and reads
    /// do not. "--browser" picks it up on its own, so this is for the machine
    /// with no browser to launch — the headless case this tool supports on
    /// purpose — where the two cookies have to be copied by hand.
    ///
    /// A value typed here lands in the shell history and in "ps"; the
    /// SNOB_CSRFTOKEN environment variable is read instead when it is set.
    #[arg(
        long,
        value_name = "TOKEN",
        requires = "paste",
        env = "SNOB_CSRFTOKEN",
        hide_env_values = true
    )]
    pub csrftoken: Option<String>,

    /// Keep the browser profile "--browser" creates, so a later login skips
    /// the Instagram form
    ///
    /// Off by default, and the default is the point. That profile is 87 MB and
    /// holds a second copy of the live session, at rest, indefinitely — more
    /// than the binary and a year of the database together. It exists so the
    /// login does not happen in the user's everyday browser, not so it
    /// survives the login.
    #[arg(long, requires = "browser")]
    pub keep_profile: bool,
}

#[derive(Args, Debug)]
pub struct WhoamiArgs {
    /// Do not check with Instagram whether the session is still alive
    #[arg(long)]
    pub offline: bool,

    /// Return the data as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct LogoutArgs {
    /// Also delete the browser profile used by "snob login"
    #[arg(long)]
    pub purge_profile: bool,
}

#[derive(Args, Debug)]
pub struct PurgeArgs {
    /// Delete without asking. Needed when there is no terminal to ask at.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// List what would be deleted and delete nothing
    #[arg(long, conflicts_with = "yes")]
    pub dry_run: bool,
}

/// The options every command that prints a list of accounts takes.
///
/// Composed from three groups rather than written as one struct, and the
/// reason is `scan`: it walks the same two lists with the same filter and
/// writes to the same destinations, but it prints counts, so `--limit` had
/// nothing to trim and was accepted with a warning. One flat struct meant
/// every command took every flag whether it meant anything or not, and the
/// warning was the cheapest way to say so. A command now takes the groups it
/// acts on — [`ScanArgs`] is this without the cap — and a flag it would ignore
/// is one clap refuses.
///
/// The order of the fields is the order of the help, so a reader meets the
/// target, then what to show, then where, then how the walk is made.
#[derive(Args, Debug)]
pub struct ListArgs {
    /// Account to analyze. Defaults to your own.
    pub target: Option<String>,

    #[command(flatten)]
    pub filter: FilterArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    /// Trim the output to the first N accounts. Saves no requests: --max-pages
    /// is what does that.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    #[command(flatten)]
    pub walk: WalkArgs,
}

/// `snob scan`: [`ListArgs`] without `--limit`, because a summary of five
/// counts has no rows to cut.
#[derive(Args, Debug)]
pub struct ScanArgs {
    /// Account to summarize. Defaults to your own.
    pub target: Option<String>,

    #[command(flatten)]
    pub filter: FilterArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    #[command(flatten)]
    pub walk: WalkArgs,
}

/// Which accounts a list keeps.
#[derive(Args, Debug, Clone, Default)]
pub struct FilterArgs {
    /// Hide accounts with any of these attributes
    #[arg(long, value_delimiter = ',', value_name = "ATTR")]
    pub hide: Vec<Attr>,

    /// Show only accounts with all of these attributes
    // Written out because the two combine in opposite ways and used to be
    // described as a symmetric pair: `--only verified,private` reads as "the
    // verified ones and the private ones" and returns neither — it means
    // verified AND private. See `Filter::allows` for why that is the useful
    // reading of `only`.
    #[arg(long, value_delimiter = ',', value_name = "ATTR")]
    pub only: Vec<Attr>,

    /// Shorthand for --hide verified
    // Hidden rather than removed: it is the spelling the first release
    // documented, so a script written against it still runs. It is kept out of
    // the help because `--hide` is the general form and a second way to say
    // one thing is the beginning of one per attribute.
    #[arg(long, hide = true)]
    pub no_verified: bool,

    /// File of usernames to exclude from the result, one per line
    #[arg(long, value_name = "FILE")]
    pub exclude_list: Option<PathBuf>,
}

/// Where a list goes and in what shape.
#[derive(Args, Debug, Clone, Default)]
pub struct OutputArgs {
    /// Output format. Defaults to a table on a terminal and JSON in a pipe.
    #[arg(long, value_enum)]
    pub format: Option<Format>,

    /// Write the result to a file instead of standard output
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub path: Option<PathBuf>,
}

/// How a walk is made: whether to make one at all, how far, and whether
/// anybody has to be asked first.
///
/// `--cache` answers out of storage and spends nothing, so the two flags that
/// shape a walk conflict with it rather than being accepted and ignored.
#[derive(Args, Debug, Clone)]
pub struct WalkArgs {
    /// Walk the list again even if there is a recent snapshot
    #[arg(long, conflicts_with = "cache")]
    pub refresh: bool,

    /// Use the last stored snapshot without touching the network
    #[arg(long, conflicts_with = "refresh")]
    pub cache: bool,

    /// Maximum age of a reusable snapshot (30m, 6h, 2d)
    #[arg(long, value_name = "DURATION", default_value = "6h", value_parser = duration)]
    pub max_age: std::time::Duration,

    /// Start from scratch instead of continuing an interrupted walk
    #[arg(long, conflicts_with = "cache")]
    pub no_resume: bool,

    /// Stop the walk after N pages, saving requests
    #[arg(long, value_name = "N", conflicts_with = "cache")]
    pub max_pages: Option<u32>,

    /// Do not draw the progress bar
    #[arg(long)]
    pub no_progress: bool,

    /// Do not ask before enumerating someone else's account
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// What no flags mean. Written by hand because a derived `Default` would put
/// `--max-age` at zero seconds, and a test building arguments with
/// `..Default::default()` would then find every snapshot stale.
impl Default for WalkArgs {
    fn default() -> Self {
        Self {
            refresh: false,
            cache: false,
            max_age: std::time::Duration::from_secs(6 * 3600),
            no_resume: false,
            max_pages: None,
            no_progress: false,
            yes: false,
        }
    }
}

/// Parses durations written like `30m`, `6h`, `2d`, `2w`.
///
/// A thin wrapper because clap wants this exact signature. The parser itself
/// lives in `snob-core`: the monitor's schedule and its configuration file read
/// the same durations, and a second copy that understood `w` while this one did
/// not is how `--max-age 2w` comes to mean two seconds.
fn duration(text: &str) -> Result<std::time::Duration, String> {
    snob_core::duration::parse(text)
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct WatchArgs {
    /// Absent means "run on a schedule".
    #[command(subcommand)]
    pub command: Option<WatchCommand>,

    #[command(flatten)]
    pub run: WatchRunArgs,
}

/// `snob watch` with no subcommand: stay up and run on a schedule.
#[derive(Args, Debug, Default)]
pub struct WatchRunArgs {
    /// Account to watch. Defaults to your own.
    pub target: Option<String>,

    /// How often to run: 6h, 2d, 2w
    #[arg(long, value_name = "DURATION", value_parser = duration)]
    pub every: Option<std::time::Duration>,

    /// Times of day to run at: 09:00,21:00
    #[arg(long, value_delimiter = ',', value_name = "HH:MM")]
    pub at: Vec<String>,

    /// Days to run on: mon,thu. Defaults to every day.
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "DAYS",
        conflicts_with = "cron"
    )]
    pub on: Vec<String>,

    /// A five-field cron expression, for a schedule you already have written
    #[arg(long, value_name = "EXPR", conflicts_with = "at")]
    pub cron: Option<String>,

    /// How far a run may be pushed later, so it does not land on the same
    /// second every day. Worked out from the interval if not given; 0 turns it
    /// off.
    #[arg(long, value_name = "DURATION", value_parser = duration)]
    pub jitter: Option<std::time::Duration>,

    /// Run once at start, then follow the schedule
    #[arg(long)]
    pub now: bool,

    #[command(flatten)]
    pub delivery: WebhookArgs,

    /// Emit one JSON object per run, on standard output
    #[arg(long)]
    pub json: bool,

    /// Do not draw the progress bar
    #[arg(long)]
    pub no_progress: bool,
}

/// Where a report goes, shared by the scheduled run and `once`.
#[derive(Args, Debug, Clone, Default)]
pub struct WebhookArgs {
    /// POST each report to this address, as JSON
    #[arg(long, value_name = "URL")]
    pub webhook: Option<String>,

    /// Header to send with it, repeatable: --header "Authorization: Bearer x"
    // A literal token here ends up in the shell history and in `ps`. The help
    // says so rather than the code refusing it: this is the shape that works in
    // a systemd unit, where the value comes from an environment file.
    #[arg(long, value_name = "NAME: VALUE")]
    pub header: Vec<String>,

    /// Sign the body with this secret, so the receiver can check it came from
    /// here. Sent as an X-Snob-Signature header. At least 32 characters.
    ///
    /// The floor is checked here rather than at the point of sending, and that
    /// is deliberate: a report is queued before it is delivered, so refusing a
    /// weak key at send time would strand one that had already been made.
    /// Here, nothing has been queued yet.
    ///
    /// A value typed here lands in the shell history and in "ps", where on
    /// Linux every local user can read it for as long as the run lasts; the
    /// SNOB_SIGNING_KEY environment variable is read instead when it is set,
    /// which is also the shape a systemd unit wants. "snob watch setup" puts
    /// the key in the keyring, and then neither is needed.
    #[arg(
        long,
        value_name = "SECRET",
        value_parser = signing_secret,
        env = "SNOB_SIGNING_KEY",
        hide_env_values = true
    )]
    pub sign_with: Option<String>,

    /// Send a report even when nothing changed, so something watching for
    /// silence can tell "nothing happened" from "it stopped running"
    #[arg(long)]
    pub heartbeat: bool,
}

/// The monitor.
///
/// Optional: `snob watch` with no subcommand is the scheduled run, taking its
/// interval from the command line or from `watch.toml`. The subcommands are the
/// things a person does by hand — look once, read the last diff, configure it,
/// ask what it has been doing.
#[derive(Subcommand, Debug)]
pub enum WatchCommand {
    /// What has changed since the last time the monitor reported
    #[command(
        after_help = "Reads what is already stored and spends no requests, so it costs nothing \
                      to run as often as you like and it never moves the monitor on: ask twice \
                      and you get the same answer.\n\n\
                      With nothing walked yet there is nothing to compare against. Run \
                      \"snob followers\" or \"snob following\" once first."
    )]
    Diff(WatchDiffArgs),

    /// Look now, report what changed, and remember having reported it
    #[command(
        after_help = "One run of the monitor. Meant for cron, a systemd timer or Windows Task \
                      Scheduler; \"snob watch\" with no subcommand schedules \
                      itself instead.\n\n\
                      It reads the account's counters and only walks a list if its counter moved, \
                      so a run with nothing to report costs a single request. Unlike \"diff\", \
                      this moves the monitor on: whatever it reports is not reported again.\n\n\
                      The first run on an account has nothing to compare against, so it reports \
                      nothing and says so."
    )]
    Once(WatchOnceArgs),

    /// Write the configuration file, step by step
    #[command(
        after_help = "Asks how often to run, where to send the reports, and which accounts to \
                      watch, then writes a file you can edit afterwards.\n\n\
                      A token or a signing key goes into the system keyring, never into the \
                      file: it sits at a guessable path and would end up in every backup of \
                      your home directory. \"snob purge\" removes both."
    )]
    Setup(WatchSetupArgs),

    /// Check the configuration would work, before it runs unattended
    #[command(
        after_help = "Everything a scheduled run needs, checked while somebody is still here to \
                      fix it: the schedule through the evaluator that actually decides it, the \
                      session, that each configured account resolves and may be read, and the \
                      webhook — by posting one \"watch.preflight\" message to it.\n\n\
                      It writes nothing and walks no list, and it exits non-zero when something \
                      would stop a run, which is what makes it usable as a monitoring probe. \
                      Poll it hourly rather than by the minute: every invocation is charged to \
                      the same daily budget the walks draw on, and a probe that drains it \
                      causes the condition it is watching for.\n\n\
                      Cost: one request to check the session, one per configured account, and \
                      one more while the session has not learned its own account's name — \
                      which it does the first time \"snob whoami\" runs."
    )]
    Check(WatchCheckArgs),

    /// What is configured, when it last ran, and what is still owed
    Status(WatchStatusArgs),
}

#[derive(Args, Debug, Default)]
pub struct WatchCheckArgs {
    /// Return the data as JSON
    #[arg(long)]
    pub json: bool,

    /// Do not post anything to the webhook
    #[arg(long)]
    pub no_webhook: bool,
}

#[derive(Args, Debug)]
pub struct WatchSetupArgs {
    /// Print what would be written and write nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct WatchStatusArgs {
    /// Return the data as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct WatchDiffArgs {
    /// Account to report on. Defaults to your own.
    pub target: Option<String>,

    /// Return the data as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct WatchOnceArgs {
    #[command(flatten)]
    pub delivery: WebhookArgs,

    /// Account to watch. Defaults to your own.
    // No -y here, and deliberately. Consent to enumerate somebody else's lists
    // is a thing a person gives, and an unattended run that could be handed one
    // on the command line is one whose consent came from whoever wrote the cron
    // entry. Reading another account needs a terminal to ask at, or an
    // `[[account]]` in `watch.toml` carrying the answer somebody gave once,
    // which is the only thing an unattended run accepts.
    pub target: Option<String>,

    /// Return the data as JSON
    #[arg(long)]
    pub json: bool,

    /// Do not draw the progress bar
    #[arg(long)]
    pub no_progress: bool,
}

#[derive(Args, Debug)]
pub struct ProfileArgs {
    /// Account to show. Defaults to your own.
    pub target: Option<String>,

    /// Output format. Defaults to a table on a terminal and JSON in a pipe.
    #[arg(long, value_enum)]
    pub format: Option<ProfileFormat>,

    /// Write the result to a file instead of standard output
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct PfpArgs {
    /// Account whose profile picture to download
    pub target: String,

    /// Destination file
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct StoriesArgs {
    /// Account whose stories to show. Defaults to your own.
    pub target: Option<String>,

    /// Download the story with this number, as printed by the listing
    #[arg(short = 'd', long, value_name = "N", conflicts_with_all = ["all", "interactive"])]
    pub download: Option<usize>,

    /// Download every story
    #[arg(long, conflicts_with = "interactive")]
    pub all: bool,

    /// Move through the stories with the arrow keys
    #[arg(short = 'i', long)]
    pub interactive: bool,

    /// Where a download goes. A directory with --all, a file otherwise.
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Output format. Defaults to a table on a terminal and JSON in a pipe.
    // Only the listing has a format. A download writes a file and the browser
    // draws a screen, so `--format json -d 3` used to be accepted and then
    // ignored, which reads as a format that did not work.
    #[arg(long, value_enum, conflicts_with_all = ["download", "all", "interactive"])]
    pub format: Option<StoryFormat>,
}

#[derive(Args, Debug)]
pub struct FollowArgs {
    /// The one account to follow or unfollow
    pub target: String,

    /// Do not ask for confirmation. Needed when there is no terminal to ask at.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Not reachable from the CLI yet; see the note in [`Command`].
#[derive(Subcommand, Debug)]
pub enum ImportCommand {
    /// Import Instagram's "Download your information" archive
    Dyi {
        /// Path to the downloaded archive
        path: PathBuf,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attr {
    Verified,
    Private,
    NoPfp,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Table,
    Json,
    Ndjson,
    Csv,
    Xlsx,
    Md,
}

/// The formats a story listing actually has.
///
/// A narrower enum rather than [`Format`] with three values quietly ignored.
/// `--format xlsx` on a list of five stories would have been accepted, printed
/// a table, and left the user believing they had a spreadsheet somewhere. The
/// three that are missing are the ones that exist to hand a **list of accounts**
/// to something else — a column of usernames — and a story has no username in
/// it. Adding them would mean deciding what a spreadsheet of five expiring
/// links is for, and nobody has asked.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoryFormat {
    Table,
    Json,
    Ndjson,
}

/// The formats a profile has: a card to read, an object to parse, a
/// document to keep. The row formats are for a list of accounts, and a
/// profile is not one — the same reasoning as [`StoryFormat`].
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileFormat {
    Table,
    Json,
    Md,
}

impl From<StoryFormat> for Format {
    fn from(story: StoryFormat) -> Self {
        match story {
            StoryFormat::Table => Self::Table,
            StoryFormat::Json => Self::Json,
            StoryFormat::Ndjson => Self::Ndjson,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Catches `conflicts_with` pointing at a name that does not exist, and
    /// other definition slips that would otherwise only show at runtime.
    #[test]
    fn the_cli_definition_is_coherent() {
        Cli::command().debug_assert();
    }

    /// The sandbox flag cannot be given without the sandbox.
    ///
    /// This is the whole safety argument for `--ig-base-url` existing at all,
    /// and it is enforced by clap rather than described in a comment: a
    /// redirected client can only ever carry a session out of a store inside
    /// the sandbox root, so the session belonging to the person running this is
    /// not reachable from a redirected run. Alone, the flag would be a way to
    /// send a live session cookie to somebody else's server.
    ///
    /// A testing build only. In a released one neither flag exists, which
    /// `crates/snob-core/tests/sandbox.rs` reads the source to hold down.
    #[cfg(feature = "testing")]
    #[test]
    fn a_redirected_run_cannot_reach_the_real_session() {
        let refused =
            Cli::try_parse_from(["snob", "--ig-base-url", "http://127.0.0.1:9/", "whoami"]);
        assert!(
            refused.is_err(),
            "a base URL with no sandbox root would carry the stored session there"
        );

        let tmp = std::env::temp_dir();
        let paired = Cli::try_parse_from([
            "snob",
            "--sandbox-root",
            tmp.to_str().expect("the temporary directory has a name"),
            "--ig-base-url",
            "http://127.0.0.1:9/",
            "whoami",
        ])
        .expect("the pair is what a sandbox run is");
        assert_eq!(
            paired.ig_base_url.map(|u| u.to_string()).as_deref(),
            Some("http://127.0.0.1:9/")
        );
        assert_eq!(paired.sandbox_root.as_deref(), Some(tmp.as_path()));

        // And a sandbox root on its own is fine: it is what drives everything
        // that does not need Instagram at all.
        assert!(
            Cli::try_parse_from([
                "snob",
                "--sandbox-root",
                tmp.to_str().expect("the temporary directory has a name"),
                "watch",
                "status",
            ])
            .is_ok()
        );
    }

    /// The one claim in the help that was not true, kept out.
    ///
    /// `snob watch check` charges 1 + N against the same GCRA budget the walks
    /// draw on -- and `clear_to_send` charges at **reservation, before** the
    /// owed sleep, so a probe that times out and is killed has already spent a
    /// slot for a request that never went out. A one-minute blackbox probe on
    /// two accounts is 4320 requests a day against a sustained ceiling of 2000;
    /// once that is drained the budget owes about 43 seconds a request, which
    /// is past every probe timeout. So the probe reports the monitor broken
    /// while it is fine, and the budget it drained is the one the walk needed:
    /// a probe that causes the condition it detects.
    ///
    /// The per-invocation cost was always stated. It was the safety claim above
    /// it that was not, and this is what keeps it from coming back the next time
    /// somebody tidies the paragraph.
    #[test]
    fn check_does_not_advertise_itself_as_free_to_poll() {
        let watch = Cli::command()
            .find_subcommand("watch")
            .expect("watch is a subcommand")
            .clone();
        let help = watch
            .find_subcommand("check")
            .expect("check is a subcommand of watch")
            .get_after_help()
            .expect("check has an after_help")
            .to_string();

        assert!(
            !help.contains("as often as you like"),
            "every invocation is charged to the budget the walks need: {help}"
        );
        assert!(
            help.contains("hourly"),
            "and the help has to name an interval instead of taking it back: {help}"
        );
    }

    /// What this wrapper actually adds: the error carries the text somebody
    /// typed, so clap can say which value it was complaining about.
    ///
    /// The parsing itself moved to `snob_core::duration` and its tests went with
    /// it; a verbatim copy of them stayed here for a while, testing the same
    /// function twice and quietly implying there were two.
    #[test]
    fn a_duration_that_will_not_parse_is_refused_by_name() {
        assert_eq!(
            duration("6h").unwrap(),
            std::time::Duration::from_secs(21_600)
        );
        assert!(duration("six hours").is_err());
    }

    #[test]
    fn unfollowers_takes_the_shared_options() {
        let cli =
            Cli::try_parse_from(["snob", "unfollowers", "@someone", "--no-verified"]).unwrap();
        assert!(matches!(cli.command, Command::Unfollowers(_)));
    }

    #[test]
    fn cache_and_refresh_are_mutually_exclusive() {
        let result = Cli::try_parse_from(["snob", "followers", "--cache", "--refresh"]);
        assert!(result.is_err());
    }

    /// `--cache` makes no walk, so a flag that shapes one is refused rather
    /// than accepted and ignored.
    #[test]
    fn a_flag_that_shapes_a_walk_is_refused_with_cache() {
        for flag in [["--max-pages", "2"], ["--no-resume", ""]] {
            let mut line = vec!["snob", "followers", "--cache", flag[0]];
            if !flag[1].is_empty() {
                line.push(flag[1]);
            }
            assert!(
                Cli::try_parse_from(&line).is_err(),
                "{} was accepted alongside --cache",
                flag[0]
            );
        }
    }

    /// `scan` prints counts, so it has no rows for `--limit` to cut and does
    /// not take it. It used to, with a warning, because it shared the list
    /// commands' struct.
    #[test]
    fn scan_takes_the_list_options_but_not_the_cap() {
        assert!(Cli::try_parse_from(["snob", "scan", "--limit", "5"]).is_err());
        let cli = Cli::try_parse_from([
            "snob", "scan", "someone", "--hide", "verified", "--format", "json", "--cache", "-y",
        ])
        .unwrap();
        let Command::Scan(args) = cli.command else {
            panic!("scan");
        };
        assert_eq!(args.target.as_deref(), Some("someone"));
        assert_eq!(args.filter.hide, vec![Attr::Verified]);
        assert_eq!(args.output.format, Some(Format::Json));
        assert!(args.walk.cache && args.walk.yes);
    }

    /// The environment variables the program answers to are announced in one
    /// place, under the examples, where exit codes already live.
    ///
    /// Four of them used to be documented nowhere at all: three belong to the
    /// crates behind the styling (`console`, `supports-hyperlinks`) and one to
    /// `main::init_tracing`, so no flag's help ever mentioned them. This pins
    /// the list. `SNOB_IGNORE_COOLDOWN` is deliberately absent -- its own
    /// doc-comment in `rate_budget` says why it is not advertised -- and the
    /// assertion holds that down too.
    #[test]
    fn the_environment_variables_are_announced_together() {
        for name in [
            "NO_COLOR",
            "CLICOLOR_FORCE",
            "FORCE_HYPERLINK",
            "SNOB_LOG",
            "SNOB_CSRFTOKEN",
            "SNOB_SIGNING_KEY",
        ] {
            assert!(EXAMPLES.contains(name), "{name} is read but not announced");
        }
        assert!(
            !EXAMPLES.contains("SNOB_IGNORE_COOLDOWN"),
            "the escape hatch is deliberately not advertised"
        );
    }

    /// Hidden from the help, still accepted: the first release documented it.
    #[test]
    fn no_verified_still_parses_and_is_not_advertised() {
        let cli = Cli::try_parse_from(["snob", "unfollowers", "--no-verified"]).unwrap();
        let Command::Unfollowers(args) = cli.command else {
            panic!("unfollowers");
        };
        assert!(args.filter.no_verified);

        let help = Cli::command()
            .find_subcommand("unfollowers")
            .expect("unfollowers is a subcommand")
            .clone()
            .render_long_help()
            .to_string();
        assert!(!help.contains("--no-verified"), "{help}");
        assert!(help.contains("--hide"), "{help}");
    }

    /// A story listing's format has nothing to say about a download or the
    /// browser, and was accepted and ignored next to both.
    #[test]
    fn a_story_format_only_goes_with_the_listing() {
        for extra in [["-d", "1"], ["--all", ""], ["-i", ""]] {
            let mut line = vec!["snob", "stories", "someone", "--format", "json", extra[0]];
            if !extra[1].is_empty() {
                line.push(extra[1]);
            }
            assert!(
                Cli::try_parse_from(&line).is_err(),
                "--format was accepted alongside {}",
                extra[0]
            );
        }
        assert!(Cli::try_parse_from(["snob", "stories", "someone", "--format", "json"]).is_ok());
    }
    /// `--tls-extra-root` only means something alongside `--strict-roots`.
    ///
    /// On the platform store there is nothing to add to: whatever an
    /// administrator installed is already trusted, so a lone `--tls-extra-root`
    /// would be a flag that reads a file and changes nothing. Enforced by clap
    /// rather than described, the same way `--ig-base-url` is paired with
    /// `--sandbox-root`.
    #[test]
    fn an_extra_root_needs_the_narrowing_it_widens() {
        let alone = Cli::try_parse_from(["snob", "--tls-extra-root", "ca.pem", "whoami"]);
        assert!(alone.is_err(), "it was accepted on its own");

        let paired = Cli::try_parse_from([
            "snob",
            "--strict-roots",
            "--tls-extra-root",
            "ca.pem",
            "whoami",
        ])
        .expect("together they are the way out of the narrowing");
        assert!(paired.strict_roots);
        assert_eq!(paired.tls_extra_root.len(), 1);

        // And narrowing on its own is the ordinary case.
        let narrow = Cli::try_parse_from(["snob", "--strict-roots", "whoami"]).unwrap();
        assert!(narrow.strict_roots);
        assert!(narrow.tls_extra_root.is_empty());

        // Off unless asked, which is the default the audit settled on.
        let plain = Cli::try_parse_from(["snob", "whoami"]).unwrap();
        assert!(!plain.strict_roots);
    }
}
