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

use crate::paths::AppPaths;
use snob_core::duration;

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
    /// version can recognize an older one instead of failing at a field.
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
    ///
    /// The at sign comes off first, because everywhere else in the tool it does:
    /// `@self` is what somebody writes who has just written `@friend` on the
    /// line above. Without it that line is read as a stranger named `self`, and
    /// a scheduled run refuses to start asking for confirmation to read an
    /// account it owns.
    pub fn is_own(&self) -> bool {
        self.target
            .trim_start_matches('@')
            .eq_ignore_ascii_case("self")
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
    // **The schema really is read first**, which is what the comment below used
    // to claim while the code did the opposite.
    //
    // `WatchConfig` carries `deny_unknown_fields`, and a full parse ran before
    // the check — so a file from a newer version, which by definition is a file
    // with keys this build has never heard of, was refused as a typo:
    //
    //     schema = 99                        -> "says it is schema 99"      (right)
    //     schema = 99, something_new = true  -> "unknown field `something_new`"
    //     schema = 99, [webhook] retries = 3 -> "unknown field `retries`"
    //
    // The one diagnostic written to survive a version skew did not survive it,
    // and what the user reads is a parse error about a file they have no reason
    // to think is broken. Nothing here is reachable yet — `SCHEMA` has only ever
    // been 1 — but the fix only helps anybody if it ships *before* schema 2
    // does, which is now.
    //
    // Two lines and no `deny_unknown_fields`, so every other key is ignored;
    // every top-level field of `WatchConfig` carries `serde(default)`, so a
    // *missing* schema key still reaches the full parse and is refused there.
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(default)]
        schema: u32,
    }
    if let Ok(Version { schema }) = toml::from_str::<Version>(text)
        && schema != SCHEMA
        && schema != 0
    {
        return Err(ConfigError::Unknown {
            path: path.to_path_buf(),
            found: schema,
        });
    }

    let config: WatchConfig = toml::from_str(text).map_err(|e| ConfigError::Invalid {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    if config.schema != SCHEMA {
        return Err(ConfigError::Unknown {
            path: path.to_path_buf(),
            found: config.schema,
        });
    }

    // `cron` and the `at`/`on` pair are two syntaxes for the same thing, and
    // only one of them can win. `schedule_from` takes `cron` first while the
    // sentence the monitor prints is built from every field that is populated,
    // so a file with both said "Running at 21:00, on the schedule \"0 9 * * 1\"."
    // and then ran only on Mondays — the banner describing a schedule nobody was
    // on. The flags cannot reach this state, `cli.rs` already declares them as
    // conflicting; the file could, and `deny_unknown_fields` is on this struct
    // for exactly the reason that applies here, that a schedule nobody is
    // running must not be accepted in silence.
    //
    // `every` alongside a calendar stays legal: those two combine rather than
    // compete, which is what `--every 2w --on mon` means.
    if config.cron.is_some() && !(config.at.is_empty() && config.on.is_empty()) {
        let other = if config.at.is_empty() { "on" } else { "at" };
        return Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            message: format!(
                "\"cron\" and \"{other}\" are two ways of saying the same thing, and only \
                 \"cron\" would be used. Keep whichever one you meant."
            ),
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
///
/// The same recipe as `secrets::write_private`, and for the reasons written out
/// there. It was `create(true).truncate(true)` here, which differs in two ways
/// that matter: an existing file at this predictable name is **opened with
/// whatever permissions it already had**, since `mode` only applies at creation,
/// and the open follows a symlink — which the rename then moves into place.
/// `create_private_dir` chmods the directory to 0700 but removes nothing already
/// inside it, so a link planted while it was lax outlives the tightening.
fn write_private(path: &Path, contents: &str) -> Result<(), ConfigError> {
    use std::io::Write;

    let failed = |source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    };

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // At creation, not afterwards: a later chmod leaves a window in which
        // the file is readable by others.
        options.mode(0o600);
    }

    // Anything left over from a failed run is cleared rather than reused, which
    // is what makes `create_new` usable at a fixed name.
    let _ = std::fs::remove_file(path);
    let mut file = options.open(path).map_err(failed)?;
    // Flushed before the caller renames it. A temporary that is renamed into
    // place with its contents still in the page cache is not the protection
    // against a half-written file that writing to a temporary is for.
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(failed)?;

    Ok(())
}

/// Renders a configuration file a person can read and edit.
///
/// Written out rather than serialized, so every value can carry the sentence
/// that explains it. What comes back has to parse — [`parse`] is what reads it
/// — and a test writes one of these and reads it back for exactly that reason.
/// **It takes the type [`parse`] produces, so the round trip is a comparison
/// rather than a description of one.** This was eight positional parameters: a
/// schedule already rendered as text, a jitter, a URL, a bool, a slice of header
/// pairs, a slice of account pairs and a bool. That is [`WatchConfig`] written
/// out again in a shape it cannot be compared against, so the tests standing in
/// for the round trip could each only assert that some field or other survived,
/// and between them they never looked at most of the file — while `WatchConfig`
/// has derived `Eq` the whole time.
///
/// Two of those parameters were also strictly weaker than the fields they stood
/// for, and each weakness was reachable. `&[(String, String)]` of headers can
/// hold one name twice; `[webhook.headers]` is a TOML table and cannot, so
/// correcting a mistyped `X-Api-Key` produced a file the wizard could not read
/// back, after every question had been answered and both secrets typed.
/// `&[(String, Option<i64>)]` is [`AccountConfig`] with the meaning of the
/// second element left to whoever is calling. A `&WatchConfig` can express
/// neither.
///
/// `signed` stays a parameter, because it is not in the file and must not be: it
/// says a key went into the keyring, and all this writes is the sentence telling
/// the reader to look there.
///
/// `schema` is what this build writes rather than what the argument holds. That
/// is also the only value [`parse`] ever returns, so the round trip is an
/// equality for every configuration that can have come out of it.
pub fn template(config: &WatchConfig, signed: bool) -> String {
    let mut out = String::new();
    out.push_str(
        "# snob watch. Written by \"snob watch setup\", and safe to edit by hand.\n\
         #\n\
         # Times are your local ones. Durations are written the way you would say\n\
         # them: 30m, 6h, 2d, 2w.\n\n",
    );
    out.push_str(&format!("schema = {SCHEMA}\n\n"));

    out.push_str("# When to run.\n");
    if let Some(every) = config.every {
        out.push_str(&format!("every = \"{}\"\n", duration::format(every)));
    }
    if !config.on.is_empty() {
        out.push_str(&format!("on = [{}]\n", quoted_list(&config.on)));
    }
    if !config.at.is_empty() {
        out.push_str(&format!("at = [{}]\n", quoted_list(&config.at)));
    }
    if let Some(cron) = &config.cron {
        out.push_str(&format!("cron = {}\n", quote(cron)));
    }

    if let Some(jitter) = config.jitter {
        out.push_str(
            "\n# How far each run may be pushed past its due moment, so the walks do\n\
             # not start on the same second every day. \"0\" turns it off.\n",
        );
        out.push_str(&format!("jitter = \"{}\"\n", duration::format(jitter)));
    }

    if let Some(webhook) = &config.webhook {
        out.push_str("\n[webhook]\n");
        out.push_str(&format!("url = {}\n", quote(&webhook.url)));
        out.push_str(
            "# Send a report even when nothing changed, so something watching for\n\
             # silence can tell \"nothing happened\" from \"it stopped running\".\n",
        );
        out.push_str(&format!("heartbeat = {}\n", webhook.heartbeat));
        if signed {
            out.push_str(
                "# The body is signed: the key is in the system keyring, not here.\n\
                 # So is any token below that you gave to \"snob watch setup\".\n",
            );
        }
        if !webhook.headers.is_empty() {
            out.push_str("\n[webhook.headers]\n");
            for (name, value) in &webhook.headers {
                out.push_str(&format!("{} = {}\n", quote(name), quote(value)));
            }
        }
    }

    for account in &config.accounts {
        out.push_str("\n[[account]]\n");
        out.push_str(&format!("target = {}\n", quote(&account.target)));
        if let Some(consent) = account.consent {
            out.push_str(
                "# You were asked whether this may read that account's lists, and you\n\
                 # said yes. A scheduled run cannot ask, so it reads this instead.\n",
            );
            out.push_str("[account.consent]\n");
            out.push_str(&format!("agreed_at = {}\n", consent.agreed_at));
        }
    }
    out
}

/// A TOML array of basic strings.
///
/// Through [`quote`] rather than a bare `"{v}"`, which is how the wizard wrote
/// these. Every day and time in one is validated before it can reach here, so
/// nothing can carry a quote today — and the file's own first line invites
/// hand-editing, which is the route by which "nothing can" stops being true.
fn quoted_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| quote(value))
        .collect::<Vec<_>>()
        .join(", ")
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

    /// Two syntaxes for the same thing, and only one of them would be used.
    ///
    /// `schedule_from` takes `cron` first while the sentence the monitor prints
    /// is built from every populated field, so a file with both announced
    /// "Running at 21:00, on the schedule "0 9 * * 1"." and then ran on Mondays
    /// alone. `deny_unknown_fields` is on this struct because a schedule nobody
    /// is running must not be accepted in silence, and this was one.
    #[test]
    fn a_file_cannot_name_a_schedule_twice() {
        let both = at(r#"
schema = 1
cron = "0 9 * * 1"
at = ["21:00"]
"#)
        .unwrap_err();
        let message = both.to_string();
        assert!(message.contains("cron"), "{message}");
        assert!(message.contains("at"), "{message}");

        assert!(
            at(r#"
schema = 1
cron = "0 9 * * 1"
on = ["thu"]
"#)
            .is_err(),
            "the day half names it twice just as much as the time half"
        );

        // An interval alongside a calendar is not the same thing: those combine,
        // which is what `--every 2w --on mon` means.
        assert!(
            at(r#"
schema = 1
cron = "0 9 * * 1"
every = "2w"
"#)
            .is_ok()
        );
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

    /// And it is still refused for its schema when it carries a key this
    /// version has never had — which is what a newer file actually looks like.
    ///
    /// The full parse ran first and `WatchConfig` denies unknown fields, so the
    /// one diagnostic written to survive a version skew did not survive it: the
    /// user got "unknown field `something_new`" about a file they have no
    /// reason to think is broken. The test above cannot tell the two orders
    /// apart, because `schema = 99` alone parses cleanly either way.
    #[test]
    fn a_newer_file_is_refused_for_its_schema_even_when_it_carries_a_key_this_version_never_had() {
        for text in [
            "schema = 99\nsomething_new = true\n",
            "schema = 99\n[webhook]\nurl = \"https://n8n.local/hook\"\nretries = 3\n",
        ] {
            let error = at(text).unwrap_err();
            assert!(
                matches!(error, ConfigError::Unknown { found: 99, .. }),
                "{text:?} was refused as a typo rather than as a newer file: {error}"
            );
        }

        // A key this version does not know, on a file that claims *this*
        // schema, is still a typo — which is what `deny_unknown_fields` is for.
        let typo = at("schema = 1\nsomething_new = true\n").unwrap_err();
        assert!(matches!(typo, ConfigError::Invalid { .. }), "{typo}");
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

    /// What the template writes is what the parser reads -- the whole of it, as
    /// one comparison.
    ///
    /// The template is written by hand so it can explain itself, so nothing but
    /// a test stops it drifting away from the parser. What stopped that test
    /// being the obvious one was `template`'s shape: eight positional parameters
    /// standing in for a `WatchConfig`, which meant the round trip could only
    /// ever be described field by field, and between the three tests that did
    /// the describing most of the file was never looked at. `parse` returns the
    /// type `template` now takes, and `WatchConfig` has derived `Eq` the whole
    /// time.
    ///
    /// Two configurations rather than one, because `cron` beside `at` or `on` is
    /// a file `parse` deliberately refuses -- so the two syntaxes cannot be
    /// covered by the same round trip and each has to have its own.
    #[test]
    fn what_the_template_writes_is_what_the_parser_reads() {
        let calendar = WatchConfig {
            schema: SCHEMA,
            every: Some(Duration::from_secs(1_209_600)),
            at: vec!["09:00".to_string(), "21:30".to_string()],
            on: vec!["mon".to_string(), "thu".to_string()],
            cron: None,
            jitter: Some(Duration::from_secs(900)),
            webhook: Some(WebhookConfig {
                url: r#"https://n8n.local/webhook/a"b\c"#.to_string(),
                headers: [
                    ("X-Source".to_string(), "homelab".to_string()),
                    ("X-Odd".to_string(), "a\"b".to_string()),
                ]
                .into_iter()
                .collect(),
                heartbeat: true,
            }),
            accounts: vec![
                AccountConfig {
                    target: "self".to_string(),
                    consent: None,
                },
                AccountConfig {
                    target: "someone".to_string(),
                    consent: Some(ConsentConfig {
                        agreed_at: 1_700_000_000,
                    }),
                },
            ],
        };
        assert_eq!(
            at(&template(&calendar, true)).expect("the template has to parse"),
            calendar,
            "a field the template does not write is a setting that disappears"
        );

        // The other syntax, and the smallest whole file: no jitter, no webhook,
        // no accounts. Each of those is an `if` in the template, and an `if`
        // with no test is a branch that can write anything.
        let cron = WatchConfig {
            schema: SCHEMA,
            every: None,
            at: vec![],
            on: vec![],
            cron: Some("0 9 * * 1,4".to_string()),
            jitter: None,
            webhook: None,
            accounts: vec![],
        };
        assert_eq!(at(&template(&cron, false)).unwrap(), cron);

        // And `signed` is not in the file: it changes a sentence for the reader
        // and nothing the parser sees.
        assert_eq!(
            at(&template(&calendar, false)).unwrap(),
            at(&template(&calendar, true)).unwrap(),
            "the signing note is prose, not configuration"
        );
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
