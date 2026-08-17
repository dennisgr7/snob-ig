//! The monitor's configuration file.
//!
//! The first thing this project writes to the configuration directory, which
//! until now was a path with a comment saying nothing creates it. `paths.rs`
//! says the directory is created by whatever writes there and not before, so
//! that is what [`write`] does.
//!
//! **Read with serde, written from a template by hand.** A serializer produces
//! a correct file and a useless one: it cannot put the reason for a value next
//! to the value, and this project puts the reason next to the thing it governs
//! everywhere else. A file somebody is meant to edit has to explain itself, so
//! the template is written out and the parser is what has to keep up with it —
//! and a test writes the template and reads it back, so it cannot drift.
//!
//! **No secrets live here.** The webhook's token and signing key go to the
//! keyring beside the session, because this file is plain text at a guessable
//! path and a token in it is a token in every backup of the home directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::duration;
use crate::paths::AppPaths;

/// What the file says.
///
/// `deny_unknown_fields` throughout, and that is not pedantry: this file drives
/// something that runs unattended for months, and a mistyped key that is
/// silently ignored is a schedule nobody is running or a consent nobody gave.
/// Better to refuse at startup, where somebody is watching.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    /// Which shape of this file it is. Read before anything else, so a future
    /// version can recognise an older one instead of failing at a field.
    #[serde(default = "one")]
    pub schema: u32,

    /// How often to run: `6h`, `2d`, `2w`.
    #[serde(default, deserialize_with = "duration_opt")]
    pub every: Option<Duration>,
    /// Times of day, `09:00`.
    #[serde(default)]
    pub at: Vec<String>,
    /// Days of the week, `mon`.
    #[serde(default)]
    pub on: Vec<String>,
    /// A five-field cron expression.
    #[serde(default)]
    pub cron: Option<String>,
    /// How far a run may be pushed later.
    #[serde(default, deserialize_with = "duration_opt")]
    pub jitter: Option<Duration>,

    #[serde(default)]
    pub webhook: Option<WebhookConfig>,

    /// The accounts to watch. Empty means your own.
    #[serde(default, rename = "account")]
    pub accounts: Vec<AccountConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    pub url: String,
    /// Extra headers, as a table. The values here are **not** secrets: a token
    /// belongs in the keyring, and `snob watch setup` puts it there.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub heartbeat: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    /// The username, or `self` for the account the session belongs to.
    pub target: String,
    /// The recorded answer to "may this walk somebody else's lists?".
    ///
    /// A table rather than a bool, because what has to be on record is that
    /// somebody answered and when — a `true` is something any editor can type
    /// without having been asked anything.
    #[serde(default)]
    pub consent: Option<ConsentConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConsentConfig {
    /// When it was given, as an epoch in seconds.
    pub agreed_at: i64,
}

impl AccountConfig {
    /// Whether this line names the account the session belongs to.
    pub fn is_own(&self) -> bool {
        self.target.eq_ignore_ascii_case("self")
    }
}

fn one() -> u32 {
    1
}

/// The shape this version writes and understands.
pub const SCHEMA: u32 = 1;

/// Durations are written the way a person types them, so they are parsed the
/// same way rather than as a number of seconds.
fn duration_opt<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let text = Option::<String>::deserialize(deserializer)?;
    text.map(|t| duration::parse(&t).map_err(D::Error::custom))
        .transpose()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} could not be read: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error(
        "{path} says it is schema {found}, and this version of snob understands {SCHEMA}.\n\
         It was probably written by a newer snob; update, or move that file aside."
    )]
    Unknown { path: PathBuf, found: u32 },
    #[error(transparent)]
    Paths(#[from] crate::paths::PathError),
}

/// Where the file lives.
pub fn path(paths: &AppPaths) -> PathBuf {
    paths.config_dir().join("watch.toml")
}

/// Reads it, if it is there.
pub fn load(paths: &AppPaths) -> Result<Option<WatchConfig>, ConfigError> {
    let path = path(paths);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    parse(&text, &path).map(Some)
}

/// Parses one, naming the file in anything it complains about.
pub fn parse(text: &str, path: &Path) -> Result<WatchConfig, ConfigError> {
    let config: WatchConfig = toml::from_str(text).map_err(|e| ConfigError::Invalid {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    // Checked before any field is used, so a file from a newer version is
    // refused as what it is rather than as a missing key.
    if config.schema != SCHEMA {
        return Err(ConfigError::Unknown {
            path: path.to_path_buf(),
            found: config.schema,
        });
    }
    Ok(config)
}

/// Writes the file, creating the configuration directory if it is not there.
///
/// `paths::ensure_dirs` deliberately does not create that directory — an empty
/// folder in every user's roaming profile is litter, and this tool left one on
/// every machine it ran on once already. So the thing that writes there creates
/// it, which is what the comment on `AppPaths::config_dir` asks for.
pub fn write(paths: &AppPaths, contents: &str) -> Result<PathBuf, ConfigError> {
    let path = path(paths);
    if let Some(parent) = path.parent() {
        // `create_private_dir` rather than `create_dir_all`, which is what the
        // data directory already uses. This holds the webhook address — which
        // for an n8n, Slack or Discord hook *is* the credential — the accounts
        // being watched and when consent was given. 0755 was letting every
        // other account on the machine read all three.
        crate::paths::create_private_dir(parent)?;
    }

    // Written whole and then moved into place, and readable only by its owner.
    //
    // The temporary file is not tidiness: `fs::write` truncates first, so a
    // process that dies mid-write leaves a half-written schedule that the next
    // start refuses to parse. Renaming is atomic on both platforms, so the file
    // is either the old one or the new one.
    let temporary = path.with_extension("toml.new");
    write_private(&temporary, contents)?;
    std::fs::rename(&temporary, &path).map_err(|source| ConfigError::Write {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Writes a file only its owner can read.
fn write_private(path: &Path, contents: &str) -> Result<(), ConfigError> {
    let failed = |source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    };

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        // The mode goes on at creation rather than afterwards, so there is no
        // window in which the file exists and is world-readable.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(failed)?;
        file.write_all(contents.as_bytes()).map_err(failed)?;
    }

    #[cfg(not(unix))]
    {
        // Windows inherits the directory's ACL, and the directory is under the
        // user's own profile.
        std::fs::write(path, contents).map_err(failed)?;
    }

    Ok(())
}

/// Renders a configuration file a person can read and edit.
///
/// Written out rather than serialized, so every value can carry the sentence
/// that explains it. What comes back has to parse — [`parse`] is what reads it
/// — and a test writes one of these and reads it back for exactly that reason.
pub fn template(
    schedule_line: &str,
    jitter: Option<Duration>,
    webhook: Option<&str>,
    heartbeat: bool,
    headers: &[(String, String)],
    accounts: &[(String, Option<i64>)],
    signed: bool,
) -> String {
    let mut out = String::new();
    out.push_str(
        "# snob watch. Written by \"snob watch setup\", and safe to edit by hand.\n\
         #\n\
         # Times are your local ones. Durations are written the way you would say\n\
         # them: 30m, 6h, 2d, 2w.\n\n",
    );
    out.push_str(&format!("schema = {SCHEMA}\n\n"));

    out.push_str("# When to run.\n");
    out.push_str(schedule_line);
    out.push('\n');

    if let Some(jitter) = jitter {
        out.push_str(
            "\n# How far each run may be pushed past its due moment, so the walks do\n\
             # not start on the same second every day. \"0\" turns it off.\n",
        );
        out.push_str(&format!("jitter = \"{}\"\n", duration::format(jitter)));
    }

    if let Some(url) = webhook {
        out.push_str("\n[webhook]\n");
        out.push_str(&format!("url = {}\n", quote(url)));
        out.push_str(
            "# Send a report even when nothing changed, so something watching for\n\
             # silence can tell \"nothing happened\" from \"it stopped running\".\n",
        );
        out.push_str(&format!("heartbeat = {heartbeat}\n"));
        if signed {
            out.push_str(
                "# The body is signed: the key is in the system keyring, not here.\n\
                 # So is any token below that you gave to \"snob watch setup\".\n",
            );
        }
        if !headers.is_empty() {
            out.push_str("\n[webhook.headers]\n");
            for (name, value) in headers {
                out.push_str(&format!("{} = {}\n", quote(name), quote(value)));
            }
        }
    }

    for (target, consented_at) in accounts {
        out.push_str("\n[[account]]\n");
        out.push_str(&format!("target = {}\n", quote(target)));
        if let Some(at) = consented_at {
            out.push_str(
                "# You were asked whether this may read that account's lists, and you\n\
                 # said yes. A scheduled run cannot ask, so it reads this instead.\n",
            );
            out.push_str("[account.consent]\n");
            out.push_str(&format!("agreed_at = {at}\n"));
        }
    }
    out
}

/// A TOML basic string. The values here come from a person, so a quote or a
/// backslash in one has to survive being written and read back.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Result<WatchConfig, ConfigError> {
        parse(text, Path::new("watch.toml"))
    }

    #[test]
    fn it_reads_an_interval_and_a_webhook() {
        let config = at(r#"
schema = 1
every = "6h"
jitter = "15m"

[webhook]
url = "https://n8n.local/webhook/snob"
heartbeat = true

[webhook.headers]
"X-Source" = "homelab"
"#)
        .unwrap();

        assert_eq!(config.every, Some(Duration::from_secs(21_600)));
        assert_eq!(config.jitter, Some(Duration::from_secs(900)));
        let webhook = config.webhook.unwrap();
        assert_eq!(webhook.url, "https://n8n.local/webhook/snob");
        assert!(webhook.heartbeat);
        assert_eq!(webhook.headers["X-Source"], "homelab");
    }

    #[test]
    fn it_reads_a_calendar() {
        let config = at(r#"
schema = 1
on = ["mon", "thu"]
at = ["09:00", "21:00"]
"#)
        .unwrap();
        assert_eq!(config.on, vec!["mon", "thu"]);
        assert_eq!(config.at, vec!["09:00", "21:00"]);
    }

    /// A typo in a file that drives something unattended for months would
    /// otherwise be a schedule nobody is running, discovered weeks later.
    #[test]
    fn a_key_that_is_not_a_key_is_refused_rather_than_ignored() {
        let error = at("schema = 1\nevry = \"6h\"\n").unwrap_err();
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error}");
        assert!(error.to_string().contains("evry"), "{error}");
    }

    /// Checked before any field is read, so a newer file is refused as what it
    /// is rather than as a missing key.
    #[test]
    fn a_file_from_a_newer_version_says_so() {
        let error = at("schema = 99\n").unwrap_err();
        assert!(
            matches!(error, ConfigError::Unknown { found: 99, .. }),
            "{error}"
        );
        assert!(error.to_string().contains("update"), "{error}");
    }

    #[test]
    fn a_duration_that_is_not_one_is_refused_where_it_is_written() {
        let error = at("schema = 1\nevery = \"six hours\"\n").unwrap_err();
        assert!(
            error.to_string().contains("not a valid duration"),
            "{error}"
        );
    }

    /// The record has to say somebody answered, not merely that the answer is
    /// yes — which is a thing any editor can type without having been asked.
    #[test]
    fn a_third_party_carries_when_it_was_agreed_to() {
        let config = at(r#"
schema = 1
every = "6h"

[[account]]
target = "self"

[[account]]
target = "someone"
[account.consent]
agreed_at = 1786925176
"#)
        .unwrap();

        assert_eq!(config.accounts.len(), 2);
        assert!(config.accounts[0].is_own());
        assert!(config.accounts[0].consent.is_none());
        assert_eq!(config.accounts[1].consent.unwrap().agreed_at, 1_786_925_176);
    }

    /// The template is written by hand so it can explain itself, which means
    /// nothing but a test stops it drifting away from the parser.
    #[test]
    fn what_the_template_writes_is_what_the_parser_reads() {
        let text = template(
            "every = \"6h\"",
            Some(Duration::from_secs(900)),
            Some("https://n8n.local/webhook/snob"),
            true,
            &[("X-Source".into(), "homelab".into())],
            &[("self".into(), None), ("someone".into(), Some(1_700))],
            true,
        );

        let config = at(&text).expect("the template has to parse");
        assert_eq!(config.every, Some(Duration::from_secs(21_600)));
        assert_eq!(config.jitter, Some(Duration::from_secs(900)));
        assert!(config.webhook.as_ref().unwrap().heartbeat);
        assert_eq!(config.accounts.len(), 2);
        assert_eq!(config.accounts[1].consent.unwrap().agreed_at, 1_700);
    }

    #[test]
    fn a_calendar_template_parses_too() {
        let text = template(
            "on = [\"mon\", \"thu\"]\nat = [\"09:00\"]",
            None,
            None,
            false,
            &[],
            &[("self".into(), None)],
            false,
        );
        let config = at(&text).unwrap();
        assert_eq!(config.on, vec!["mon", "thu"]);
        assert_eq!(config.at, vec!["09:00"]);
    }

    /// A URL or a header value with a quote in it has to survive the round
    /// trip, or the file the tool wrote is one it cannot read.
    #[test]
    fn a_value_with_a_quote_in_it_survives_being_written() {
        let text = template(
            "every = \"6h\"",
            None,
            Some(r#"https://example.com/a"b\c"#),
            false,
            &[("X-Odd".into(), "a\"b".into())],
            &[],
            false,
        );
        let config = at(&text).unwrap();
        let webhook = config.webhook.unwrap();
        assert_eq!(webhook.url, r#"https://example.com/a"b\c"#);
        assert_eq!(webhook.headers["X-Odd"], "a\"b");
    }

    /// A file with only a schedule is complete. Everything else is optional,
    /// and a parser that demanded a webhook would make the no-webhook mode --
    /// `snob watch --json >> events.ndjson` -- unconfigurable.
    #[test]
    fn a_schedule_on_its_own_is_a_whole_file() {
        let config = at("schema = 1\nevery = \"6h\"\n").unwrap();
        assert!(config.webhook.is_none());
        assert!(config.accounts.is_empty());
    }
}
