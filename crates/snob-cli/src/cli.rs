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

/// Parses durations written like `30m`, `6h`, `2d`.
fn duration(text: &str) -> Result<std::time::Duration, String> {
    let text = text.trim();
    let (number, factor) = match text.chars().last() {
        Some('s') => (&text[..text.len() - 1], 1),
        Some('m') => (&text[..text.len() - 1], 60),
        Some('h') => (&text[..text.len() - 1], 3600),
        Some('d') => (&text[..text.len() - 1], 86400),
        // With no suffix, seconds are assumed.
        Some(c) if c.is_ascii_digit() => (text, 1),
        _ => {
            return Err(format!(
                "\"{text}\" is not a valid duration (try 30m, 6h or 2d)"
            ));
        }
    };

    let value: u64 = number
        .trim()
        .parse()
        .map_err(|_| format!("\"{text}\" is not a valid duration (try 30m, 6h or 2d)"))?;

    // A number long enough to overflow is not a duration anyone means, and
    // wrapping it would silently turn "never expire" into "expire at once".
    let seconds = value
        .checked_mul(factor)
        .ok_or_else(|| format!("\"{text}\" is too long to be a duration"))?;

    Ok(std::time::Duration::from_secs(seconds))
}

#[derive(Args, Debug)]
pub struct WatchArgs {
    #[command(subcommand)]
    pub command: WatchCommand,
}

/// The monitor.
///
/// A subcommand is required for now. The scheduled run — `snob watch` on its
/// own, with the interval either on the command line or in its configuration
/// file — makes this optional when it lands, which is an addition rather than a
/// change: nothing written against `snob watch diff` stops working.
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
