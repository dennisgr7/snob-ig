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
                  somebody else both know.\n\n\
                  It only ever reads. snob never follows, unfollows, blocks or removes \
                  anyone.",
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

    #[command(subcommand)]
    pub command: Command,
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
  snob scan someone                   the full picture of another account
  snob pfp someone -o picture.jpg     their profile picture, at full size
  snob unfollowers --format csv -o unfollowers.csv

A username may be written with or without a leading @. If you write the @, quote
it (\"@someone\"): on PowerShell an unquoted one is eaten by the shell.

Exit codes:
  0   it worked; a list cut short by --limit or --max-pages is still a 0
  1   it failed, or a result was refused because a list came back incomplete
  3   no session, or the stored one no longer works -- run \"snob login\"
  4   Instagram wants the account verified -- open the address it prints
  5   Instagram is throttling, or the account is in cooldown -- wait
  130 stopped by you: Ctrl+C, or a confirmation not given -- including with
      no terminal to ask at, where -y confirms in advance";

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

    /// Summary of the whole account: followers, following, and how they cross
    Scan(ListArgs),

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

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Account to analyze. Defaults to your own.
    pub target: Option<String>,

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
    #[arg(long)]
    pub no_verified: bool,

    /// File of usernames to exclude from the result, one per line
    #[arg(long, value_name = "FILE")]
    pub exclude_list: Option<PathBuf>,

    /// Output format. Defaults to a table on a terminal and JSON in a pipe.
    #[arg(long, value_enum)]
    pub format: Option<Format>,

    /// Write the result to a file instead of standard output
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Trim the output to the first N accounts. Saves no requests: --max-pages
    /// is what does that.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

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
    #[arg(long)]
    pub no_resume: bool,

    /// Stop the walk after N pages, saving requests
    #[arg(long, value_name = "N")]
    pub max_pages: Option<u32>,

    /// Do not draw the progress bar
    #[arg(long)]
    pub no_progress: bool,

    /// Do not ask before enumerating someone else's account
    #[arg(short = 'y', long)]
    pub yes: bool,
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
#[derive(Args, Debug)]
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
    /// here. Sent as an X-Snob-Signature header.
    #[arg(long, value_name = "SECRET")]
    pub sign_with: Option<String>,

    /// Send a report even when nothing changed, so something watching for
    /// silence can tell "nothing happened" from "it stopped running"
    #[arg(long)]
    pub heartbeat: bool,
}

/// The monitor.
///
/// A subcommand is required for now. The scheduled run — `snob watch` on its
/// own, with the interval either on the command line or in its configuration
/// file — makes this optional when it lands, which is an addition rather than a
/// change: nothing written against these stops working.
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
                      Scheduler until the scheduled mode lands.\n\n\
                      It reads the account's counters and only walks a list if its counter moved, \
                      so a run with nothing to report costs a single request. Unlike \"diff\", \
                      this moves the monitor on: whatever it reports is not reported again.\n\n\
                      The first run on an account has nothing to compare against, so it reports \
                      nothing and says so."
    )]
    Once(WatchOnceArgs),
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
    // entry. Reading another account needs a terminal to ask at until the
    // configuration file lands, which is where a recorded answer will live.
    pub target: Option<String>,

    /// Return the data as JSON
    #[arg(long)]
    pub json: bool,

    /// Do not draw the progress bar
    #[arg(long)]
    pub no_progress: bool,
}

#[derive(Args, Debug)]
pub struct PfpArgs {
    /// Account whose profile picture to download
    pub target: String,

    /// Destination file
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,
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

    #[test]
    fn it_parses_the_usual_durations() {
        use std::time::Duration;
        assert_eq!(duration("30m").unwrap(), Duration::from_secs(1_800));
        assert_eq!(duration("6h").unwrap(), Duration::from_secs(21_600));
        assert_eq!(duration("2d").unwrap(), Duration::from_secs(172_800));
        assert_eq!(duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(duration(" 6h ").unwrap(), Duration::from_secs(21_600));
    }

    #[test]
    fn it_rejects_what_is_not_a_duration() {
        for bad in ["", "h", "six hours", "6x", "-3h", "6.5h"] {
            assert!(duration(bad).is_err(), "\"{bad}\" should be rejected");
        }
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
}
