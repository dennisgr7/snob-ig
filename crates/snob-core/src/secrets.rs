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
use crate::session::{MAX_KEYRING_SECRET_BYTES, Session, keyring_bytes};

const KEYRING_SERVICE: &str = "snob-ig";
const KEYRING_USER: &str = "session";
/// Separate keyring entry used only to check that writing works.
const KEYRING_PROBE_USER: &str = "write-probe";

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("the system keyring is unavailable: {0}")]
    KeyringUnavailable(String),
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

        // The Windows keyring has a size ceiling. If we go over it, store the
        // essentials rather than failing.
        let json = if keyring_bytes(&json) > MAX_KEYRING_SECRET_BYTES {
            tracing::warn!(
                bytes = keyring_bytes(&json),
                "the session does not fit the keyring whole; storing only the essential fields"
            );
            Zeroizing::new(
                serde_json::to_string(&session.minimal())
                    .map_err(|e| SecretsError::Corrupt(format!("while serializing: {e}")))?,
            )
        } else {
            json
        };

        match self.backend {
            Backend::Keyring => {
                self.entry()?
                    .set_password(&json)
                    .map_err(|e| SecretsError::KeyringUnavailable(e.to_string()))?;
                // Do not leave two different sessions lying around. Both
                // locations, not just the current one: `load` checks the
                // keyring first, so once an entry exists the rescue path that
                // would have found and removed the legacy file is never
                // reached again, and an older account's live cookie stays in
                // the roaming profile — where it roams — until someone happens
                // to run `logout` or `purge`.
                for stale in [
                    Some(self.paths.session_file()),
                    self.paths.legacy_session_file(),
                ]
                .into_iter()
                .flatten()
                {
                    let _ = std::fs::remove_file(stale);
                }
            }
            Backend::File => {
                self.write_file(&json)?;
                if let Ok(entry) = self.entry() {
                    let _ = entry.delete_credential();
                }
            }
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

    pub fn delete(&self) -> Result<(), SecretsError> {
        if let Ok(entry) = self.entry() {
            let _ = entry.delete_credential();
        }
        let file = self.paths.session_file();
        if file.exists()
            && let Err(source) = std::fs::remove_file(&file)
        {
            return Err(SecretsError::Write {
                path: file.display().to_string(),
                source,
            });
        }
        // Make sure a delete does not leave the legacy copy alive.
        if let Some(previous) = self.paths.legacy_session_file() {
            let _ = std::fs::remove_file(previous);
        }
        Ok(())
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

    fn file_store() -> (tempfile::TempDir, SecretStore) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        (
            tmp,
            SecretStore::new(paths, true).with_service(&test_service()),
        )
    }

    /// The real service name must not appear in any test.
    #[test]
    fn tests_never_point_at_the_real_keyring() {
        let (_tmp, store) = file_store();
        assert_ne!(store.service, KEYRING_SERVICE);
        assert!(store.service.starts_with("snob-ig-test-"));
    }

    #[test]
    fn file_round_trip() {
        let (_tmp, store) = file_store();
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
        let (_tmp, store) = file_store();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn delete_removes_the_session() {
        let (_tmp, store) = file_store();
        let s = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        store.save(&s).unwrap();
        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn delete_with_nothing_to_delete_does_not_fail() {
        let (_tmp, store) = file_store();
        store.delete().unwrap();
    }

    #[test]
    fn a_session_in_the_legacy_location_is_rescued() {
        let (_tmp, store) = file_store();
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
        let (_tmp, store) = file_store();
        let previous = store.paths.legacy_session_file().unwrap();
        std::fs::create_dir_all(previous.parent().unwrap()).unwrap();
        std::fs::write(&previous, b"{}").unwrap();

        store.delete().unwrap();
        assert!(!previous.exists());
    }

    #[test]
    fn a_corrupt_file_gives_a_clear_error() {
        let (_tmp, store) = file_store();
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
        let (_tmp, store) = file_store();
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
        let (_tmp, store) = file_store();
        let s = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        store.save(&s).unwrap();
        let mode = std::fs::metadata(store.paths.session_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "actual mode: {:o}", mode & 0o777);
    }
}
