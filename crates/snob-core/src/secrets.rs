//! Session storage.
//!
//! By default it goes to the system keyring (Credential Manager, Keychain or
//! Secret Service). The file fallback exists for environments without a desktop
//! session, where the keyring simply is not there.
//!
//! # What this protects against, and what it does not
//!
//! Worth stating plainly, because the shape of the module suggests a hierarchy
//! that does not exist:
//!
//! - **Other users of the machine**: yes. The keyring is per account, and the
//!   file is `0600` inside a `0700` directory.
//! - **The file or the credential travelling to another machine**: yes. Every
//!   backend ties the secret to this user on this computer — DPAPI on Windows,
//!   the login keychain on macOS, the Secret Service collection on Linux.
//! - **Code running as the user themselves**: **no. Nowhere. By any backend.**
//!   Windows documents no read restriction on `CRED_TYPE_GENERIC`, and any
//!   process of the same logon can call `CredRead` or `CryptUnprotectData` and
//!   get the plaintext with no prompt. That is not a gap in this tool; it is
//!   what every credential store on a desktop offers, and it is the same
//!   boundary `gh`, `aws`, `docker` and `flyctl` settle for.
//!
//! Two consequences follow. **On Windows the keyring and the file fallback have
//! the same cryptographic protection** — both are DPAPI under the user's master
//! key — so choosing between them is about management, not strength. And
//! **adding secondary entropy to `CryptProtectData` would buy nothing**: it
//! would have to live in the binary, where it is public, and the only attacker
//! it could stop is one that already runs as the user and can simply read it.
//! It would also become a value that can never change without invalidating
//! every stored session. It has been considered and rejected.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use zeroize::{Zeroize, Zeroizing};

use crate::paths::{AppPaths, PathError};
use crate::secret::Secret;
use crate::session::{MAX_KEYRING_SECRET_BYTES, Session, keyring_bytes};

const KEYRING_SERVICE: &str = "snob-ig";
const KEYRING_USER: &str = "session";
/// Separate keyring entry used only to check that writing works.
const KEYRING_PROBE_USER: &str = "write-probe";

/// What the credential store said when it was asked for one secret.
///
/// **Three answers, not two.** This used to be an `Option`, and `None` meant
/// both "the store holds nothing under that name" and "this process cannot
/// reach the store at all". Those are not the same fact and they do not deserve
/// the same reaction: the first is a configuration the user chose, the second
/// is a machine that cannot honor the one they did choose.
///
/// What that cost is in `commands::watch::plan`. Both of its warning arms sit
/// inside `if let Some(...)`, so two absent secrets produced **no** warnings and
/// a webhook client with no `Authorization` and no signing key. On a box where
/// the session is in the file fallback and the keyring is not reachable — a
/// `login --no-keyring`, then `setup`, then cron with no session bus — the
/// monitor posted a document naming the user's followers unauthenticated and
/// unsigned, and the only trace was a `debug!` line nobody sees at the default
/// level. `WatchConfig` records nothing about what `setup` stored, so no later
/// run could notice either.
///
/// An enum rather than a second predicate the caller has to remember to ask:
/// this project has the rule that a guard living in a doc-comment is not a
/// guard.
#[derive(Debug)]
pub enum Stored {
    Found(Secret),
    /// The store answered, and holds nothing under this name.
    Nothing,
    /// The store could not be opened. Whether anything is in it is unknown.
    Unreachable,
}

impl Stored {
    /// The secret, for callers that genuinely do not care why there is none.
    pub fn found(self) -> Option<Secret> {
        match self {
            Self::Found(secret) => Some(secret),
            Self::Nothing | Self::Unreachable => None,
        }
    }

    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable)
    }
}

/// Everything this tool may keep in the keyring.
///
/// An enum with an `ALL` rather than a set of loose strings, and the reason is
/// [`SecretStore::delete`]: it walks this list, so a secret added later is
/// deleted by `snob purge` without anybody having to remember to add it there
/// too. A test walks the variants for the same reason.
///
/// The probe entry is deliberately not here. It is written and removed by the
/// write check itself and never holds anything, so listing it would only mean
/// `delete` reporting a failure about a value nobody stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The Instagram session. What every command needs.
    Session,
    /// A token the monitor sends as a header to the user's webhook.
    WatchToken,
    /// The key the monitor signs the webhook body with.
    WatchSigningKey,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Session, Kind::WatchToken, Kind::WatchSigningKey];

    /// The keyring entry's user name. Stable: changing one of these strands
    /// whatever is already stored under the old one, where `purge` will no
    /// longer find it either.
    pub fn entry_name(self) -> &'static str {
        match self {
            Self::Session => KEYRING_USER,
            Self::WatchToken => "watch-token",
            Self::WatchSigningKey => "watch-signing-key",
        }
    }
}

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("the system keyring is unavailable: {0}")]
    KeyringUnavailable(String),
    /// The keyring answered, and would not give the entry up.
    ///
    /// Deliberately not [`SecretsError::KeyringUnavailable`], which means there
    /// is no backend to talk to at all — ordinary on a server, a container or
    /// WSL, and never a reason to say a session survived. Here there **is** one,
    /// it is holding the session, and it refused. That is the one case where
    /// "the session was deleted" is false, so it needs a sentence of its own.
    #[error("the system keyring would not delete the stored session: {0}")]
    KeyringRefused(String),
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the stored session is corrupt: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Paths(#[from] PathError),
    #[cfg(windows)]
    #[error("could not {operation} the session with DPAPI (code {code})")]
    Dpapi { operation: &'static str, code: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The operating system keyring.
    Keyring,
    /// A file in the data directory. On Windows, protected with DPAPI.
    File,
}

impl Backend {
    /// Stable token for machine-readable output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keyring => "keyring",
            Self::File => "file",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Protection {
    Dpapi,
    Plain,
}

#[derive(Serialize, Deserialize)]
struct StoredSession {
    protection: Protection,
    payload: String,
}

pub struct SecretStore {
    backend: Backend,
    paths: AppPaths,
    /// Keyring service name.
    ///
    /// A field rather than a constant because tests **have** to point
    /// elsewhere: the keyring belongs to the operating system, not the process,
    /// and a test calling `delete()` under the real name wipes the session of
    /// whoever is running the suite. It happened once already.
    service: String,
}

impl SecretStore {
    pub fn new(paths: AppPaths, prefer_file: bool) -> Self {
        let backend = if prefer_file {
            Backend::File
        } else {
            Backend::Keyring
        };
        Self {
            backend,
            paths,
            service: KEYRING_SERVICE.to_string(),
        }
    }

    /// Points at a different keyring entry. **Tests only.**
    #[doc(hidden)]
    pub fn with_service(mut self, service: &str) -> Self {
        self.service = service.to_string();
        self
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Where the session lives, for the user to read.
    pub fn describe(&self) -> String {
        match self.backend {
            Backend::Keyring => "system keyring".to_string(),
            Backend::File => format!("file {}", self.paths.session_file().display()),
        }
    }

    /// The file path, when the backend is one. Kept apart from `describe` so
    /// that machine-readable output does not have to dig it out of a sentence.
    pub fn storage_path(&self) -> Option<String> {
        match self.backend {
            Backend::Keyring => None,
            Backend::File => Some(self.paths.session_file().display().to_string()),
        }
    }

    /// Checks that writing works, and reports **where** it would work.
    ///
    /// This runs before anything is asked of the user. Having someone complete
    /// a two-factor login and then lose the result because there was no D-Bus
    /// is the worst possible failure of this command.
    ///
    /// On a machine with no keyring — a server, a container, WSL, any headless
    /// Linux — this falls back to the file rather than refusing. That is a
    /// deliberate reversal: asking the user to run the whole login again with
    /// `--no-keyring` teaches them a flag to fix a situation the tool could
    /// have recognized itself. What it does **not** do is fall back silently:
    /// the answer says which backend it landed on, and the caller says so out
    /// loud, because a session quietly stored somewhere less protected than the
    /// user expected is its own kind of failure.
    pub fn probe_writable(&self) -> Result<Backend, SecretsError> {
        if self.backend == Backend::Keyring {
            match self.probe_keyring() {
                Ok(()) => return Ok(Backend::Keyring),
                Err(e) => {
                    tracing::debug!(error = %e, "no usable keyring; trying the file instead");
                }
            }
        }

        self.probe_file()?;
        Ok(Backend::File)
    }

    fn probe_keyring(&self) -> Result<(), SecretsError> {
        let entry = self.probe_entry()?;
        entry
            .set_password("probe")
            .map_err(|e| SecretsError::KeyringUnavailable(e.to_string()))?;
        let _ = entry.delete_credential();
        Ok(())
    }

    fn probe_file(&self) -> Result<(), SecretsError> {
        self.paths.ensure_dirs()?;
        let probe = self.paths.data_dir().join(".write-probe");
        write_private(&probe, b"probe")?;
        let _ = std::fs::remove_file(&probe);
        Ok(())
    }

    /// Fixes the backend to the one that was found to work.
    ///
    /// Called after [`SecretStore::probe_writable`] so that the store saves
    /// where it proved it could, rather than where it was first asked to.
    pub fn using(mut self, backend: Backend) -> Self {
        self.backend = backend;
        self
    }

    pub fn save(&self, session: &Session) -> Result<(), SecretsError> {
        // `Zeroizing` throughout: the serialized session is the cookie in
        // plaintext, and it would otherwise be left in freed memory for a core
        // dump or a swap file to pick up.
        let json = Zeroizing::new(
            serde_json::to_string(session)
                .map_err(|e| SecretsError::Corrupt(format!("while serializing: {e}")))?,
        );

        match self.backend {
            Backend::Keyring => {
                // The Windows keyring has a size ceiling. If we go over it,
                // store the essentials rather than failing.
                //
                // **Inside this arm**, because it is a property of this
                // backend. Applied before the match, a 0600 file — which has no
                // size limit at all — was written without `username`,
                // `csrftoken`, `mid` and `ig_did`, and the warning named a
                // keyring that was not the destination. `IgClient::get` then
                // omits `X-CSRFToken`, which `session.rs` records as having
                // already cost one debugging session.
                let json =
                    if keyring_bytes(&json) > MAX_KEYRING_SECRET_BYTES {
                        tracing::warn!(
                            bytes = keyring_bytes(&json),
                            "the session does not fit the keyring whole; storing only the \
                         essential fields"
                        );
                        Zeroizing::new(serde_json::to_string(&session.minimal()).map_err(|e| {
                            SecretsError::Corrupt(format!("while serializing: {e}"))
                        })?)
                    } else {
                        json
                    };

                self.entry()?
                    .set_password(&json)
                    .map_err(|e| SecretsError::KeyringUnavailable(e.to_string()))?;
            }
            Backend::File => {
                self.write_file(&json)?;
                // Not a reason to fail the save: the backend the user asked for
                // has the session. But not something to pass over in silence
                // either — `load` reads the keyring **first**, so an entry that
                // will not go is the session every later run picks up, and the
                // file just written is never reached.
                if let Ok(entry) = self.entry()
                    && let Err(e) = entry.delete_credential()
                    && !matches!(e, keyring::Error::NoEntry)
                {
                    tracing::warn!(
                        error = %e,
                        "the session was written to the file, but the keyring would not give up \
                         its own copy; that copy is the one later runs will read"
                    );
                }
            }
        }

        // Do not leave two different sessions lying around. Every location, not
        // just the current one, and **on both backends** — `session_files`'s own
        // doc says `save` and `delete` both walk it, and only the keyring arm
        // did.
        //
        // `load` checks the keyring first, so once an entry exists the rescue
        // path that would have found and removed a legacy file is never reached
        // again. On the file backend nothing reached it either: a `--no-keyring`
        // save wrote the new file, deleted the keyring entry, and left an older
        // install's `session.json` in the **roaming** profile — where it roams,
        // and into every backup of the home directory — until somebody happened
        // to run `logout` or `purge`.
        //
        // The file just written is skipped, which is what makes this safe to run
        // after the `Backend::File` arm.
        let just_written = (self.backend == Backend::File).then(|| self.paths.session_file());
        for stale in self.paths.session_files() {
            if Some(&stale) == just_written.as_ref() {
                continue;
            }
            let _ = std::fs::remove_file(stale);
        }
        Ok(())
    }

    pub fn load(&self) -> Result<Option<Session>, SecretsError> {
        // Both places are checked whatever the preferred backend: if the user
        // saved with --no-keyring and then runs without the flag, the session
        // still has to show up.
        if let Some(json) = self.load_from_keyring()? {
            return parse_session(&json).map(Some);
        }
        if let Some(json) = self.load_from_file()? {
            return parse_session(&json).map(Some);
        }
        Ok(None)
    }

    /// Stores one of the monitor's secrets.
    ///
    /// Keyring only, unlike the session — and that is a deliberate difference
    /// rather than an omission. The session has a fallback because without one
    /// the tool does not work at all on a machine with no keyring, which
    /// `paths.rs` documents as normal for a server or a container. A webhook
    /// token is not in that position: without it the monitor still runs, still
    /// reports, and still writes to standard output. So rather than invent a
    /// second protected file, this says it cannot keep the secret and the user
    /// passes `--sign-with` or `--header` on the command line, where a systemd
    /// unit can supply it from an environment file.
    pub fn save_secret(&self, kind: Kind, value: &Secret) -> Result<(), SecretsError> {
        debug_assert_ne!(kind, Kind::Session, "the session is saved by `save`");
        self.entry_for(kind.entry_name())?
            .set_password(value.expose())
            .map_err(|e| SecretsError::KeyringRefused(e.to_string()))
    }

    /// Reads one back, if it is there.
    pub fn load_secret(&self, kind: Kind) -> Result<Stored, SecretsError> {
        match self.entry_for(kind.entry_name()) {
            Ok(entry) => match entry.get_password() {
                Ok(value) => Ok(Stored::Found(Secret::new(value))),
                Err(keyring::Error::NoEntry) => Ok(Stored::Nothing),
                Err(e) => Err(SecretsError::KeyringRefused(e.to_string())),
            },
            // Not an error — the tool works without one — but not "nothing is
            // stored" either, which is what this used to answer.
            Err(e) => {
                tracing::debug!(error = %e, "there is no keyring to read from");
                Ok(Stored::Unreachable)
            }
        }
    }

    /// Removes one, for a `setup` that is being run again with no token this
    /// time. Silent about one that was not there.
    pub fn forget_secret(&self, kind: Kind) -> Result<(), SecretsError> {
        match self.entry_for(kind.entry_name()) {
            Ok(entry) => match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(SecretsError::KeyringRefused(e.to_string())),
            },
            Err(_) => Ok(()),
        }
    }

    /// Whether there is a credential on this machine at all.
    ///
    /// **Anything but a clean "nothing there" counts as one.** A stored session
    /// too corrupt to parse is still a session on the disk, so treating the parse
    /// failure as absence would let `logout` print "there was no session stored"
    /// while deleting one — and skip the line about it still being live on
    /// Instagram, which is exactly the case where the user needs it.
    ///
    /// It lives here rather than in the two commands that ask because the reading
    /// is the non-obvious part: `!matches!(load(), Ok(None))` written out at a
    /// call site looks like an oversight, and the obvious `load().is_ok()` is
    /// wrong in the one way that matters.
    pub fn something_is_stored(&self) -> bool {
        !matches!(self.load(), Ok(None))
    }

    /// Which of the monitor's secrets are on this machine.
    ///
    /// The session is not among them: [`something_is_stored`] answers for that
    /// one, and it has to, because a session too corrupt to parse still counts
    /// and `load_secret` would call it absent.
    ///
    /// This exists so `purge` can **name** what it is about to remove. What it
    /// removes is `Kind::ALL` unconditionally, so nothing depends on this
    /// answer being complete — a keyring that refuses to be read leaves the
    /// listing short and the deletion whole, which is the right way round.
    ///
    /// Walking `Kind::ALL` rather than naming the two, so a secret added later
    /// appears here without anybody remembering to come back.
    ///
    /// [`something_is_stored`]: SecretStore::something_is_stored
    pub fn monitor_secrets_stored(&self) -> Vec<Kind> {
        Kind::ALL
            .into_iter()
            .filter(|kind| *kind != Kind::Session)
            .filter(|kind| matches!(self.load_secret(*kind), Ok(Stored::Found(_))))
            .collect()
    }

    /// Removes the session from everywhere it can be, and says so only if it
    /// went.
    ///
    /// **Every location is attempted even after one of them refuses.** Stopping
    /// at the first failure would leave the copies behind it alive, and this
    /// call does not promise to have tried — it promises that no live cookie
    /// survives it.
    ///
    /// The keyring's answer used to be discarded, so a refusal there was
    /// indistinguishable from success: `logout` printed "Session deleted." and
    /// `purge` printed "snob's files are gone from this computer" over a
    /// credential that was still in the store, which is the outcome `purge`
    /// exists to prevent. `NoEntry` is not a refusal — it means there was
    /// nothing to take away, which is the result being asked for — and neither
    /// is having no keyring at all, for the same reason: there is no copy there
    /// to leave behind.
    ///
    /// One error comes back where two can happen, and the file's wins. Both
    /// give the same exit code and both withhold the same claim, so the choice
    /// only decides which sentence is printed — and the file's names a path
    /// somebody can go and delete by hand.
    pub fn delete(&self) -> Result<(), SecretsError> {
        self.remove(&[Kind::Session])
    }

    /// Removes every secret this tool has ever written, and says so only if they
    /// all went.
    ///
    /// What [`SecretStore::delete`] used to be, and the split is the whole
    /// point. `Kind::ALL` is the one list, for the same reason
    /// `AppPaths::session_files` and `owned_dirs` are: a secret added later must
    /// not be forgotten by the one command whose entire job is to leave nothing
    /// behind, and a webhook token still in the keyring after `snob purge` is
    /// exactly the failure that command exists to prevent.
    ///
    /// But `delete` had a second caller, and for `logout` the same list was
    /// wrong: `snob logout` says "This removes the session and nothing else",
    /// and it was taking the monitor's webhook token and signing key with it.
    /// The monitor then went on running from `watch.toml`, found nothing in the
    /// keyring, and posted reports with neither `Authorization` nor
    /// `X-Snob-Signature` — where a receiver that requires the token answers
    /// 401, a 401 is a refusal, and the change in that report is gone.
    pub fn delete_all(&self) -> Result<(), SecretsError> {
        self.remove(&Kind::ALL)
    }

    fn remove(&self, kinds: &[Kind]) -> Result<(), SecretsError> {
        let mut keyring_refused = None;
        for &kind in kinds {
            match self.entry_for(kind.entry_name()) {
                Ok(entry) => match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(e) => {
                        keyring_refused.get_or_insert(SecretsError::KeyringRefused(e.to_string()));
                    }
                },
                Err(e) => tracing::debug!(error = %e, "there is no keyring to delete from"),
            }
        }

        // The legacy copy is not housekeeping: it is a working session in the
        // directory that roams with the profile, so a failure to remove it is
        // a live cookie left behind exactly like the current one.
        let mut file_refused = None;
        for file in self.paths.session_files() {
            if file.exists()
                && let Err(source) = std::fs::remove_file(&file)
                && file_refused.is_none()
            {
                file_refused = Some(SecretsError::Write {
                    path: file.display().to_string(),
                    source,
                });
            }
        }

        match file_refused.or(keyring_refused) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, SecretsError> {
        self.entry_for(KEYRING_USER)
    }

    /// Separate entry for the write check.
    ///
    /// It must not be the session's: checking by writing and deleting over the
    /// real entry would destroy a working session whenever the login that
    /// follows ends up failing.
    fn probe_entry(&self) -> Result<keyring::Entry, SecretsError> {
        self.entry_for(KEYRING_PROBE_USER)
    }

    /// Builds a keyring entry.
    ///
    /// **Known limitation, Windows.** The backend writes the credential with
    /// `CRED_PERSIST_ENTERPRISE`, which Microsoft documents as visible "to
    /// logon sessions for this user **on other computers**" — so on a machine
    /// with a roaming profile, or with Credential Roaming enabled, the session
    /// cookie follows the user around the network. `CRED_PERSIST_LOCAL_MACHINE`
    /// would keep it where it was created, which is the same reasoning that put
    /// the database in the local data directory rather than the roaming one.
    ///
    /// It is not set here because `keyring`'s own `Entry` does not expose the
    /// modifier: reaching it means depending on `keyring-core` directly and
    /// keeping that version in step with whatever `keyring` pulls in, or the
    /// default store the two of them share stops being the same one and every
    /// read fails at run time. Microsoft also notes that the value degrades to
    /// local storage on accounts with no roamable state, which is every
    /// ordinary machine — so the exposure is real but narrow.
    fn entry_for(&self, user: &str) -> Result<keyring::Entry, SecretsError> {
        keyring::Entry::new(&self.service, user)
            .map_err(|e| SecretsError::KeyringUnavailable(e.to_string()))
    }

    fn load_from_keyring(&self) -> Result<Option<Zeroizing<String>>, SecretsError> {
        let entry = match self.entry() {
            Ok(e) => e,
            // No keyring backend at all. Ordinary on a server, a container or
            // WSL, and the session may still be in the fallback file.
            Err(e) => {
                tracing::debug!(error = %e, "there is no keyring to read from");
                return Ok(None);
            }
        };
        match entry.get_password() {
            Ok(json) => Ok(Some(Zeroizing::new(json))),
            // The entry is simply not there.
            Err(keyring::Error::NoEntry) => Ok(None),
            // The keyring is there and refused. Every error used to land here
            // silently alongside `NoEntry`, and the two are not the same
            // claim: a credential that cannot be read this once became "no
            // session stored", which sends the user to log in again over a
            // session that is still there — and `save` removes the file
            // fallback once a keyring entry exists, so there is nothing behind
            // it to catch them.
            //
            // It still falls through to the file, because that is the right
            // thing to try next. What it no longer does is fall through in
            // silence.
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "the keyring refused to hand the session over; trying the file instead"
                );
                Ok(None)
            }
        }
    }

    fn load_from_file(&self) -> Result<Option<Zeroizing<String>>, SecretsError> {
        let file = self.paths.session_file();
        if file.exists() {
            return self.read_file(&file).map(Some);
        }

        // Earlier versions stored this in the data directory that roams with
        // the profile. Rescue whatever is there and move it across.
        let Some(previous) = self.paths.legacy_session_file() else {
            return Ok(None);
        };
        if !previous.exists() {
            return Ok(None);
        }

        tracing::info!(
            from = %previous.display(),
            to = %file.display(),
            "moving the session to the current data directory"
        );
        let json = self.read_file(&previous)?;

        // If the move fails nothing is lost: the session has been read and the
        // old file is still where it was.
        match self.write_file(&json) {
            Ok(()) => {
                let _ = std::fs::remove_file(&previous);
            }
            Err(e) => tracing::warn!(error = %e, "could not move the session"),
        }

        Ok(Some(json))
    }

    /// Reads a stored session, clearing what it read on the way out.
    ///
    /// On Unix the payload **is** the session in the clear — the protection
    /// there is the file's 0600 permissions, not encryption — so the bytes read
    /// off the disk and the payload parsed out of them are both the cookie.
    /// This runs on every command, so an unzeroized copy of each is left in
    /// freed memory every time the tool starts.
    fn read_file(&self, path: &std::path::Path) -> Result<Zeroizing<String>, SecretsError> {
        let raw = Zeroizing::new(std::fs::read(path).map_err(|source| SecretsError::Read {
            path: path.display().to_string(),
            source,
        })?);
        let mut stored: StoredSession = serde_json::from_slice(&raw)
            .map_err(|e| SecretsError::Corrupt(format!("the session file is not valid: {e}")))?;
        let session = unprotect(&stored);
        stored.payload.zeroize();
        session
    }

    fn write_file(&self, json: &str) -> Result<(), SecretsError> {
        self.paths.ensure_dirs()?;
        let stored = protect(json)?;
        let serialized = serde_json::to_vec_pretty(&stored)
            .map_err(|e| SecretsError::Corrupt(format!("while wrapping: {e}")))?;
        write_private(&self.paths.session_file(), &serialized)
    }
}

fn parse_session(json: &str) -> Result<Session, SecretsError> {
    let session: Session = serde_json::from_str(json)
        .map_err(|e| SecretsError::Corrupt(format!("could not parse: {e}")))?;
    session
        .check_schema()
        .map_err(|e| SecretsError::Corrupt(e.to_string()))?;
    Ok(session)
}

/// Writes with restricted permissions and atomically: first to a temporary in
/// the same directory, then rename.
fn write_private(path: &std::path::Path, contents: &[u8]) -> Result<(), SecretsError> {
    use std::io::Write;

    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let temporary = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));

    let mut options = std::fs::OpenOptions::new();
    // `create_new`, not `create`: the temporary's name is predictable, and with
    // `create` an existing one would be opened and written **with whatever
    // permissions it already had**, which is the one thing the mode below is
    // here to prevent. It would follow a symlink, too. Anything left over from
    // a failed run is cleared first rather than reused.
    options.write(true).create_new(true);
    // Permissions are set at creation, not afterwards: a later chmod leaves a
    // window in which the file is readable by others.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let _ = std::fs::remove_file(&temporary);
    let mut f = options
        .open(&temporary)
        .map_err(|source| SecretsError::Write {
            path: temporary.display().to_string(),
            source,
        })?;
    f.write_all(contents)
        .and_then(|()| f.sync_all())
        .map_err(|source| SecretsError::Write {
            path: temporary.display().to_string(),
            source,
        })?;
    drop(f);

    std::fs::rename(&temporary, path).map_err(|source| SecretsError::Write {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(windows)]
fn protect(json: &str) -> Result<StoredSession, SecretsError> {
    let encrypted = dpapi::protect(json.as_bytes())?;
    Ok(StoredSession {
        protection: Protection::Dpapi,
        payload: b64(&encrypted),
    })
}

#[cfg(not(windows))]
fn protect(json: &str) -> Result<StoredSession, SecretsError> {
    // On Unix the protection is the file's 0600 permissions.
    Ok(StoredSession {
        protection: Protection::Plain,
        payload: json.to_string(),
    })
}

fn unprotect(stored: &StoredSession) -> Result<Zeroizing<String>, SecretsError> {
    match stored.protection {
        Protection::Plain => Ok(Zeroizing::new(stored.payload.clone())),
        Protection::Dpapi => {
            #[cfg(windows)]
            {
                let bytes = unb64(&stored.payload)?;
                let plain = Zeroizing::new(dpapi::unprotect(&bytes)?);
                String::from_utf8(plain.to_vec())
                    .map(Zeroizing::new)
                    .map_err(|e| SecretsError::Corrupt(format!("not valid UTF-8: {e}")))
            }
            #[cfg(not(windows))]
            {
                Err(SecretsError::Corrupt(
                    "the session is DPAPI-protected and this system is not Windows".into(),
                ))
            }
        }
    }
}

#[cfg(windows)]
fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(windows)]
fn unb64(s: &str) -> Result<Vec<u8>, SecretsError> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| SecretsError::Corrupt(format!("invalid base64: {e}")))
}

/// Encryption tied to the Windows user account. A file protected this way is
/// useless copied to another machine or another account.
#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    use super::SecretsError;

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, SecretsError> {
        transform(plain, true)
    }

    pub fn unprotect(encrypted: &[u8]) -> Result<Vec<u8>, SecretsError> {
        transform(encrypted, false)
    }

    fn transform(input: &[u8], encrypt: bool) -> Result<Vec<u8>, SecretsError> {
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_ptr() as *mut u8,
        };
        let mut out_blob = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };

        // SAFETY: both blobs are valid for the duration of the call; the output
        // buffer is allocated by Windows and freed with LocalFree below.
        let ok = unsafe {
            if encrypt {
                CryptProtectData(
                    &in_blob,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out_blob,
                )
            } else {
                CryptUnprotectData(
                    &in_blob,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out_blob,
                )
            }
        };

        if ok == 0 {
            let code = unsafe { GetLastError() };
            return Err(SecretsError::Dpapi {
                operation: if encrypt { "encrypt" } else { "decrypt" },
                code,
            });
        }

        // SAFETY: on success Windows guarantees pbData is valid for cbData bytes.
        let output = unsafe {
            std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec()
        };
        // On the decrypt path this buffer is the session in the clear, and it
        // is Windows's memory rather than ours: nothing else will wipe it, and
        // `LocalFree` only returns it to the heap with the cookie still in it.
        // The copy above is what the caller wraps in `Zeroizing`; this is the
        // original.
        //
        // SAFETY: same pointer and length the read above used, still owned by
        // this function and not yet freed.
        unsafe {
            std::ptr::write_bytes(out_blob.pbData, 0, out_blob.cbData as usize);
            LocalFree(out_blob.pbData as *mut core::ffi::c_void)
        };

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionOrigin;

    const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
    const SID: &str = "71234567890%3AAbCdEfGhIjKl%3A20";

    /// A keyring service name of its own for each test.
    ///
    /// The keyring belongs to the operating system, not the process: a test
    /// using the real name wipes the session of whoever is developing. This
    /// actually happened, so it is not a theoretical precaution.
    fn test_service() -> String {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("snob-ig-test-{}-{n}", std::process::id())
    }

    /// Serializes the tests that reach the keyring.
    ///
    /// The credential store belongs to the operating system, and touching it
    /// from several threads at once is not reliable here: an entry written by
    /// one test came back missing to another, roughly one run in ten, in
    /// whichever test happened to be running at the time. Not a collision
    /// between the tests — each already has a service name of its own — so the
    /// race is below this code and cannot be fixed from here. Running them one
    /// at a time is the whole fix, and it costs milliseconds.
    ///
    /// The guard is handed back by `file_store` so a test that goes through it
    /// cannot forget to take it. One test does not go through it --
    /// `the_session_lands_where_the_probe_said_it_would` builds a
    /// keyring-backed store on purpose -- and it takes the lock by hand.
    ///
    /// **It does not reach across test binaries**, which a `static` cannot do,
    /// and `snob-cli` runs its own in parallel. That is why this is a reduction
    /// in a failure rate rather than a fix: what is left is one operating
    /// system credential store being written by two processes at once, which
    /// nothing in this repository can serialize.
    fn keyring_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn file_store() -> (
        tempfile::TempDir,
        SecretStore,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let held = keyring_lock();
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        (
            tmp,
            SecretStore::new(paths, true).with_service(&test_service()),
            held,
        )
    }

    /// The real service name must not appear in any test.
    #[test]
    fn tests_never_point_at_the_real_keyring() {
        let (_tmp, store, _keyring) = file_store();
        assert_ne!(store.service, KEYRING_SERVICE);
        assert!(store.service.starts_with("snob-ig-test-"));
    }

    /// Every kind has an entry name, and no two share one.
    ///
    /// Two kinds pointing at one entry would have the second silently overwrite
    /// the first — the signing key landing on top of the token, with nothing
    /// failing anywhere.
    #[test]
    fn every_kind_has_a_name_of_its_own() {
        // `ALL` is the list `purge` walks, and every test of it -- including
        // this one -- walks the same list, so shrinking `ALL` used to be
        // invisible: drop `WatchSigningKey` from it and `snob purge` leaves the
        // signing key in the user's keyring forever, with nothing failing.
        //
        // Two guards, because they catch opposite mistakes. The match is
        // exhaustive, so a variant added later stops this compiling until
        // somebody looks at `ALL`; the count catches a variant taken out of
        // `ALL` while the type keeps it.
        fn is_a_kind(kind: Kind) -> bool {
            match kind {
                Kind::Session | Kind::WatchToken | Kind::WatchSigningKey => true,
            }
        }
        assert!(Kind::ALL.into_iter().all(is_a_kind));
        assert_eq!(
            Kind::ALL.len(),
            3,
            "a kind left `ALL`, so `purge` no longer removes it"
        );

        let names: Vec<&str> = Kind::ALL.iter().map(|k| k.entry_name()).collect();
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "two kinds share an entry: {names:?}"
        );
        assert!(!names.contains(&KEYRING_PROBE_USER));
    }

    /// The guard `snob purge` rests on. Its whole promise is that afterwards
    /// there is nothing of this tool left on the machine, and a webhook token
    /// forgotten in the keyring is precisely the failure it exists to prevent.
    ///
    /// Walking `Kind::ALL` rather than naming the three, so a secret added
    /// later is covered by this test the moment it joins the list — the same
    /// shape as the test that walks every `StopReason`.
    #[test]
    fn purging_takes_every_kind_of_secret_with_it() {
        let (_tmp, store, _keyring) = file_store();
        store
            .save(&Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap())
            .unwrap();

        if !save_the_monitors_secrets(&store) {
            return;
        }

        store.delete_all().unwrap();

        assert!(
            store.load().unwrap().is_none(),
            "the session is still there"
        );
        for kind in Kind::ALL {
            assert!(
                store.load_secret(kind).unwrap().found().is_none(),
                "{kind:?} survived a purge"
            );
        }
    }

    /// Stores one secret for every kind but the session. Returns false when
    /// there is no keyring to store them in, which `save_secret` documents and
    /// is not what any of these tests are about.
    fn save_the_monitors_secrets(store: &SecretStore) -> bool {
        for kind in Kind::ALL {
            if kind == Kind::Session {
                continue;
            }
            if store.save_secret(kind, &Secret::new("a secret")).is_err() {
                return false;
            }
        }
        true
    }

    /// Saving to the file clears an earlier version's copy too.
    ///
    /// `AppPaths::session_files` says in its own doc that both `save` and
    /// `delete` walk it, and only the keyring arm did. A `--no-keyring` save
    /// wrote the new file, deleted the keyring entry, and left an older
    /// install's `session.json` in the **roaming** profile — a live Instagram
    /// cookie that roams, and lands in every backup of the home directory.
    /// `load` reads the keyring first, so the rescue-and-delete path that would
    /// have found it never ran again either.
    #[test]
    fn saving_to_the_file_clears_an_earlier_versions_copy() {
        let (_tmp, store, _keyring) = file_store();
        let store = store.using(Backend::File);

        let legacy = store
            .paths
            .legacy_session_file()
            .expect("the fixture has one");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"an older install's live session").unwrap();

        store
            .save(&Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap())
            .unwrap();

        assert!(
            !legacy.exists(),
            "the roaming copy is a live credential and has to go"
        );
        assert!(
            store.paths.session_file().exists(),
            "and the one just written stays"
        );
    }

    /// The keyring's size ceiling is the keyring's, not the file's.
    ///
    /// The reduction to `session.minimal()` sat before the backend match, so a
    /// 0600 file — which has no size limit — was written without `username`,
    /// `csrftoken`, `mid` and `ig_did`, and the warning named a keyring that was
    /// not the destination. `IgClient::get` then omits `X-CSRFToken`, the silent
    /// state `session.rs` records as having already cost a debugging session.
    #[test]
    fn the_file_backend_does_not_shrink_to_fit_a_keyring() {
        let (_tmp, store, _keyring) = file_store();
        let store = store.using(Backend::File);

        let mut session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        session.csrftoken = Some(Secret::new("a-csrf-token"));
        // Long enough that the whole thing is past the keyring ceiling.
        session.ig_did = Some("x".repeat(MAX_KEYRING_SECRET_BYTES));

        store.save(&session).unwrap();

        let back = store.load().unwrap().expect("it was just saved");
        assert!(
            back.csrftoken.is_some(),
            "a file has no size ceiling, so nothing may be dropped to fit one"
        );
    }

    /// `snob logout` takes the session and nothing else, which is what its help
    /// says in those words.
    ///
    /// It shared one function with `purge`, so logging out silently took the
    /// monitor's webhook token and signing key. The monitor kept running from
    /// `watch.toml`, found nothing in the keyring, and posted reports with
    /// neither `Authorization` nor `X-Snob-Signature`; a receiver that requires
    /// the token answers 401, a 401 is a refusal, and the change in that report
    /// is lost with no retry.
    #[test]
    fn logging_out_leaves_the_monitors_secrets_alone() {
        let (_tmp, store, _keyring) = file_store();
        store
            .save(&Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap())
            .unwrap();

        if !save_the_monitors_secrets(&store) {
            return;
        }

        store.delete().unwrap();

        assert!(
            store.load().unwrap().is_none(),
            "the session should be gone"
        );
        for kind in Kind::ALL {
            if kind == Kind::Session {
                continue;
            }
            assert!(
                store.load_secret(kind).unwrap().found().is_some(),
                "logout took {kind:?} with it"
            );
        }
    }

    #[test]
    fn a_stored_secret_reads_back_and_can_be_forgotten() {
        let (_tmp, store, _keyring) = file_store();
        if store
            .save_secret(Kind::WatchToken, &Secret::new("Bearer abc"))
            .is_err()
        {
            return; // no keyring on this machine; see `save_secret`
        }

        assert_eq!(
            store
                .load_secret(Kind::WatchToken)
                .unwrap()
                .found()
                .unwrap()
                .expose(),
            "Bearer abc"
        );
        store.forget_secret(Kind::WatchToken).unwrap();
        assert!(
            store
                .load_secret(Kind::WatchToken)
                .unwrap()
                .found()
                .is_none()
        );
        // Forgetting one that is not there is not an error: `setup` run again
        // with no token has to be able to clear whatever was there before.
        store.forget_secret(Kind::WatchToken).unwrap();
    }

    #[test]
    fn file_round_trip() {
        let (_tmp, store, _keyring) = file_store();
        let original = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();

        assert_eq!(store.probe_writable().unwrap(), Backend::File);
        store.save(&original).unwrap();

        let recovered = store.load().unwrap().expect("there should be a session");
        assert_eq!(recovered.sessionid, original.sessionid);
        assert_eq!(recovered.ds_user_id, original.ds_user_id);
        assert_eq!(recovered.user_agent, original.user_agent);
    }

    /// The promise `probe_writable` makes: whatever backend it names is where
    /// the session actually lands. A machine with no keyring — a server, a
    /// container, WSL — gets the file instead of a refusal, and the caller is
    /// told so rather than left to discover it.
    ///
    /// Which branch runs here depends on the machine: with a working keyring it
    /// proves the keyring path, without one it proves the fallback. The
    /// assertion is the same either way, which is the point — the answer and
    /// the destination cannot disagree.
    #[test]
    fn the_session_lands_where_the_probe_said_it_would() {
        // The lock, taken by hand because this is the one test that builds a
        // keyring-backed store rather than going through `file_store` -- and
        // the only one in the workspace that *writes* to the operating
        // system's credential store.
        let _keyring = keyring_lock();
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let store = SecretStore::new(paths, false).with_service(&test_service());

        let landed = store.probe_writable().unwrap();
        let store = store.using(landed);

        let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        store.save(&session).unwrap();

        assert_eq!(
            store.paths.session_file().exists(),
            landed == Backend::File,
            "the file should exist exactly when the probe named the file"
        );
        let read_back = load_settled(&store);
        assert_eq!(
            read_back
                .expect("the session was just written and has to read back")
                .sessionid
                .expose(),
            session.sessionid.expose()
        );

        store.delete().unwrap();
    }

    /// Reads the session back, waiting out the credential store if it needs it.
    ///
    /// Not a retry bolted on to make a red test green. What it waits for was
    /// measured: this suite creates and deletes dozens of credentials in
    /// parallel, and two or three runs in fifty ended with `save` reporting the
    /// write to the Windows Credential Manager as successful and the read
    /// immediately after it answering `NoEntry` — the credential is not
    /// missing, it is not visible yet.
    ///
    /// The delay is the whole mechanism, which is why an immediate second
    /// attempt did not help: both landed inside the same window, microseconds
    /// apart. Adding any tracing to the path made it stop reproducing, which is
    /// the other reason to believe it is a timing window rather than logic.
    ///
    /// The real store is deliberately kept rather than faked. What this test is
    /// for is that the answer `probe_writable` gave and the place `save` put it
    /// cannot disagree, and against an in-memory double that proves nothing
    /// about the platform it is asserting.
    fn load_settled(store: &SecretStore) -> Option<Session> {
        for attempt in 0..5 {
            if let Some(session) = store.load().unwrap() {
                return Some(session);
            }
            std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
        }
        store.load().unwrap()
    }

    #[test]
    fn with_no_session_stored_it_returns_none() {
        let (_tmp, store, _keyring) = file_store();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn delete_removes_the_session() {
        let (_tmp, store, _keyring) = file_store();
        let s = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        store.save(&s).unwrap();
        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    /// Having nothing to delete is the result being asked for, not a refusal.
    ///
    /// The guard against the honesty fix going too far: `delete` now reports
    /// what would not go, and the keyring's way of saying "there was nothing
    /// here" is `keyring::Error::NoEntry` — an answer, not a failure. Reading
    /// it as one would make every `logout` on a clean machine exit non-zero.
    #[test]
    fn nothing_stored_is_not_a_refusal() {
        let (_tmp, store, _keyring) = file_store();
        store.delete().unwrap();
    }

    #[test]
    fn a_session_in_the_legacy_location_is_rescued() {
        let (_tmp, store, _keyring) = file_store();
        let s = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();

        // Simulate an earlier install: the session only exists in the legacy
        // path.
        store.save(&s).unwrap();
        let previous = store.paths.legacy_session_file().unwrap();
        std::fs::create_dir_all(previous.parent().unwrap()).unwrap();
        std::fs::rename(store.paths.session_file(), &previous).unwrap();
        assert!(!store.paths.session_file().exists());

        let recovered = store.load().unwrap().expect("it should be rescued");
        assert_eq!(recovered.sessionid, s.sessionid);

        // And it is moved, not duplicated.
        assert!(store.paths.session_file().exists());
        assert!(!previous.exists());
    }

    #[test]
    fn delete_also_clears_the_legacy_location() {
        let (_tmp, store, _keyring) = file_store();
        let previous = store.paths.legacy_session_file().unwrap();
        std::fs::create_dir_all(previous.parent().unwrap()).unwrap();
        std::fs::write(&previous, b"{}").unwrap();

        store.delete().unwrap();
        assert!(!previous.exists());
    }

    /// One copy refusing must not spare the others, and must still be reported.
    ///
    /// The keyring branch cannot be driven from a test — reaching a fake store
    /// means depending on `keyring-core` directly, which `entry_for` documents
    /// as the thing not to do, and no test may touch the real one. So the shape
    /// is pinned through the filesystem, which the keyring branch shares: every
    /// location is attempted, and the refusal comes back at the end rather than
    /// short-circuiting. Returning early was the tempting fix and the wrong
    /// one: it would leave the copies after the failure alive.
    ///
    /// A directory standing where the file goes is how the refusal is arranged:
    /// `remove_file` fails on one on every platform, which a permission bit
    /// does not — Windows governs deletion by the file's read-only attribute
    /// and Unix by the parent's write bit. What is being tested is the
    /// reporting, not the reason the operating system said no.
    #[test]
    fn a_copy_that_will_not_go_is_reported_and_does_not_stop_the_others() {
        let (_tmp, store, _keyring) = file_store();
        let s = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        store.save(&s).unwrap();

        // An earlier install's copy, in the other directory. It is the one
        // after the refusal, and it still has to go.
        let previous = store.paths.legacy_session_file().unwrap();
        std::fs::create_dir_all(previous.parent().unwrap()).unwrap();
        std::fs::write(&previous, b"{}").unwrap();

        let holding = store.paths.session_file();
        std::fs::remove_file(&holding).unwrap();
        std::fs::create_dir(&holding).unwrap();

        let result = store.delete();

        assert!(
            matches!(result, Err(SecretsError::Write { .. })),
            "the refusal has to reach the caller, or purge claims the session is gone"
        );
        assert!(
            holding.exists(),
            "the test did not arrange what it meant to"
        );
        assert!(
            !previous.exists(),
            "the copy after the refusal was skipped, which is the failure the loop exists to avoid"
        );
    }

    #[test]
    fn a_corrupt_file_gives_a_clear_error() {
        let (_tmp, store, _keyring) = file_store();
        store.paths.ensure_dirs().unwrap();
        std::fs::write(store.paths.session_file(), b"this is not json").unwrap();
        assert!(matches!(store.load(), Err(SecretsError::Corrupt(_))));
    }

    #[test]
    fn the_backend_token_is_stable() {
        assert_eq!(Backend::Keyring.as_str(), "keyring");
        assert_eq!(Backend::File.as_str(), "file");
    }

    #[test]
    #[cfg(windows)]
    fn on_windows_the_file_does_not_hold_the_credential_in_the_clear() {
        let (_tmp, store, _keyring) = file_store();
        let s = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        store.save(&s).unwrap();
        let raw = std::fs::read_to_string(store.paths.session_file()).unwrap();
        assert!(
            !raw.contains("AbCdEfGhIjKl"),
            "the sessionid appears in the clear in the file"
        );
    }

    #[test]
    #[cfg(unix)]
    fn on_unix_the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, store, _keyring) = file_store();
        let s = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        store.save(&s).unwrap();
        let mode = std::fs::metadata(store.paths.session_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "actual mode: {:o}", mode & 0o777);
    }
}
