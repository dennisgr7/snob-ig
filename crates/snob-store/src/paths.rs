//! Where the tool keeps its data, following each platform's conventions.
//!
//! **These are per user, never per directory.** Everything here comes from
//! `directories`, which reads the user's profile, so `snob` run from two
//! different folders is the same session, the same database and the same cache.
//! The only thing the working directory decides is where an exported file lands
//! when no `-o` was given, which is what any command-line tool does.
//!
//! Data goes to the **local** directory, not the one that roams with the user
//! profile. On Windows that is the difference between Roaming and Local, and it
//! matters here: the database uses WAL, and WAL on a directory synced by
//! OneDrive or a roaming profile is a documented cause of SQLite corruption. It
//! also keeps megabytes of snapshots out of the roaming profile. Configuration
//! would stay in the syncable directory, where it belongs — but there is none
//! yet, so nothing creates that directory.
//!
//! On Linux and macOS both paths are the same, so the distinction only shows on
//! Windows.

use std::path::{Path, PathBuf};

use thiserror::Error;

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "";
const APPLICATION: &str = "snob-ig";

#[derive(Debug, Error)]
pub enum PathError {
    #[error("could not determine the user's data directory")]
    NoHome,
    #[error("could not create the directory {path}: {source}")]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The directory exists and could not be limited to this account.
    ///
    /// Its own variant rather than a [`PathError::Create`], because the
    /// directory was created perfectly well and blaming the creation sends
    /// somebody looking in the wrong place. What failed is the half that makes
    /// it private, and that is the half worth naming: the database inside it is
    /// the whole follower history in the clear.
    #[error("could not restrict {path} to your account: {detail}")]
    NotPrivate { path: PathBuf, detail: String },
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    config: PathBuf,
    data: PathBuf,
    /// Data directory used by earlier versions. Only exists on Windows, where
    /// it pointed at Roaming. It is read to rescue whatever is there, never
    /// written to.
    legacy_data: Option<PathBuf>,
    /// Set by [`Self::rooted_at`], and the reason [`Self::stories_root`] is not
    /// simply the system temporary directory.
    ///
    /// Every other location this type hands out is already under the root a
    /// test or `--sandbox-root` gave it. The scratch directory is the one that
    /// would otherwise escape, because it is the one that comes from the
    /// environment rather than from `directories`. "Every file this run
    /// touches is under one directory" is a promise the sandbox seam makes in
    /// `AGENTS.md`, and a hole in it is a test writing into the real
    /// `%TEMP%`.
    sandbox: Option<PathBuf>,
}

impl AppPaths {
    pub fn discover() -> Result<Self, PathError> {
        let dirs = directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .ok_or(PathError::NoHome)?;

        let data = dirs.data_local_dir().to_path_buf();
        let previous = dirs.data_dir().to_path_buf();

        Ok(Self {
            config: dirs.config_dir().to_path_buf(),
            legacy_data: (previous != data).then_some(previous),
            data,
            sandbox: None,
        })
    }

    /// For tests: puts every directory under one root.
    pub fn rooted_at(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            config: root.join("config"),
            data: root.join("data"),
            // Always defined so the rescue path can be tested on any platform,
            // not just Windows.
            legacy_data: Some(root.join("legacy-data")),
            sandbox: Some(root.to_path_buf()),
        }
    }

    /// Where configuration lives: `watch.toml`, written by `snob watch setup`.
    /// The directory is not created until something writes to it, because an
    /// empty folder in the user's roaming profile is litter and this tool left
    /// one on every machine it ran on.
    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    pub fn db_file(&self) -> PathBuf {
        self.data.join("snob.db")
    }

    /// Only used when the system keyring is unavailable.
    pub fn session_file(&self) -> PathBuf {
        self.data.join("session.json")
    }

    /// Where an earlier version of the tool would look for the session.
    pub fn legacy_session_file(&self) -> Option<PathBuf> {
        self.legacy_data.as_ref().map(|d| d.join("session.json"))
    }

    /// Every file a session can be sitting in, for the same reason
    /// [`Self::owned_dirs`] exists: the list is assembled here so that a
    /// location added later cannot be forgotten by one of the callers whose
    /// whole job is to leave no live cookie behind.
    ///
    /// Both `SecretStore::save` and `SecretStore::delete` walk it. `save` is on
    /// the list because `load` checks the keyring first, so once an entry exists
    /// the rescue path that would have found the legacy file is never reached
    /// again and an older account's cookie stays in the roaming profile.
    pub fn session_files(&self) -> Vec<PathBuf> {
        [Some(self.session_file()), self.legacy_session_file()]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Browser profile used by `snob login`. Never the user's real profile.
    pub fn browser_profile(&self) -> PathBuf {
        self.data.join("browser-profile")
    }

    /// Where `snob stories --interactive` puts a story it is about to hand to
    /// the system viewer.
    ///
    /// **Under the operating system's temporary directory, one directory per
    /// process, and this was measured rather than reasoned.** The first version
    /// of this put it under the data directory and gave three reasons, all of
    /// which turned out to be wrong on Windows 11 in August 2026:
    ///
    /// - *"An image viewer holds the file open and Windows refuses to delete
    ///   it."* Photos with the picture on screen and Media Player with the
    ///   video playing both hold **no** handle: Restart Manager reports nobody,
    ///   and `DeleteFileW` succeeds while the window is still up. A viewer that
    ///   opens without `FILE_SHARE_DELETE` does exist and nothing can delete
    ///   underneath it — but it is not the default, and it is not the case the
    ///   design should have been built around.
    /// - *"The temporary directory is world-readable."* True of `/tmp` on
    ///   Linux, and false on Windows: `%TEMP%` and `%LOCALAPPDATA%` carry
    ///   **identical** ACLs — SYSTEM, Administrators, the user — both inherited.
    ///   On Linux the honest answer is better than either: `$XDG_RUNTIME_DIR`
    ///   is 0700 *by specification*, on tmpfs, and dies with the session.
    /// - *"The data directory is the more private default."* Windows Search
    ///   **indexes `%LOCALAPPDATA%\snob-ig` today** — a query against the live
    ///   index returns files out of the browser profile. It buys no privacy.
    ///
    /// So the file goes where a temporary file goes. `std::env::temp_dir()`
    /// already does the right thing on all three platforms, and on Linux
    /// [`runtime_dir`] prefers `$XDG_RUNTIME_DIR` when there is one.
    ///
    /// **The process id is in the name on purpose**, twice over: two `snob`
    /// runs must not share a directory one of them will delete, and a
    /// fixed-name directory in a shared temporary space is a name anybody can
    /// predict and create first.
    ///
    /// [`Self::owned_dirs`] carries the parent, so `snob purge` still reaches
    /// whatever a run left behind — which is the case that matters, because the
    /// system's own cleaner is not something to lean on: the oldest thing in
    /// this machine's `%TEMP%` had been there forty-nine days.
    pub fn story_scratch(&self) -> PathBuf {
        self.stories_root()
            .join(format!("run-{}", std::process::id()))
    }

    /// The parent of every [`Self::story_scratch`], which is what `purge`
    /// removes and what the age sweep walks.
    pub fn stories_root(&self) -> PathBuf {
        // A sandbox root replaces every other location a run touches, and this
        // is one of them: a test must not be able to reach into the real
        // temporary directory, and `--sandbox-root` promising "every file this
        // run touches" has to keep being true.
        match &self.sandbox {
            Some(root) => root.join("stories"),
            None => runtime_dir().join("snob-ig-stories"),
        }
    }

    /// Every directory this tool may have created, for `snob purge` to remove.
    ///
    /// Assembled here rather than by the command so that a directory added
    /// later cannot be forgotten by the one command whose whole job is to leave
    /// nothing behind -- the configuration directory included, which is where
    /// `watch.toml` goes.
    ///
    /// Deduplicated, because on macOS the configuration and data directories
    /// are the same path: listed twice, the second removal would fail on a
    /// directory the first one had already taken and be reported as a problem.
    pub fn owned_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let stories = self.stories_root();
        for dir in [
            Some(&self.data),
            Some(&self.config),
            self.legacy_data.as_ref(),
            // The one location outside the data directory this tool writes to.
            // It is here rather than left to the system's temporary-file
            // cleaner because that cleaner is not a mechanism: the oldest thing
            // in this machine's `%TEMP%` had been sitting there for forty-nine
            // days. See [`Self::story_scratch`].
            Some(&stories),
        ]
        .into_iter()
        .flatten()
        {
            if !dirs.contains(dir) {
                dirs.push(dir.clone());
            }
        }
        dirs
    }

    /// Creates what the tool actually writes to.
    ///
    /// Only the data directory. There **is** a configuration file --
    /// `snob watch setup` writes one -- but that command creates the directory
    /// itself, because creating it for everybody leaves an empty folder in the
    /// roaming profile of every machine this has ever run on.
    pub fn ensure_dirs(&self) -> Result<(), PathError> {
        create_private_dir(&self.data)
    }
}

/// The folder name every path here is built from.
///
/// Exposed so that a caller recognizing one of our own directories compares
/// against the value `ProjectDirs` was given rather than against a copy of it.
/// `snob purge` needs exactly that, and a second spelling of this string is one
/// that stops matching the day the application is renamed — silently, because
/// the guard simply never fires again.
pub fn app_dir_name() -> &'static str {
    APPLICATION
}

/// Whether a directory is plausible as one of ours, and so may be deleted whole.
///
/// `snob purge` removes directories recursively, which is the only destructive
/// thing this tool does anywhere. It does not take the path on trust: a
/// `ProjectDirs` that resolved oddly — an empty `HOME`, a profile variable that
/// never expanded — turns "remove the data directory" into something far worse,
/// and the check that rules it out costs nothing. Anything this tool creates
/// sits at least two levels below the root and is never the home directory
/// itself.
pub fn is_safe_to_remove(dir: &Path) -> bool {
    // Rejects `/`, `C:\`, `/data` and `C:\data` alike.
    if dir.parent().is_none_or(|parent| parent.parent().is_none()) {
        return false;
    }

    match directories::BaseDirs::new() {
        Some(base) => dir != base.home_dir(),
        // With no home to compare against, the depth check above is all there
        // is — and it is the one that matters.
        None => true,
    }
}

/// Where a file that should not outlive the session goes.
///
/// `$XDG_RUNTIME_DIR` when there is one, and [`std::env::temp_dir`] otherwise.
/// The preference is Linux's alone in practice, and it is worth the three
/// lines: the XDG base directory specification requires that directory to be
/// owned by the user, `0700`, on a filesystem that is not shared, and **removed
/// when the session ends** — which is every property wanted here and none of
/// which `/tmp` promises. `std::env::temp_dir` reads `GetTempPath2W` on
/// Windows and `$TMPDIR` on macOS, both of which are already per-user.
///
/// The variable is checked for being an absolute path that exists, because it
/// arrives from the environment and a relative one would put somebody else's
/// photograph in the working directory.
fn runtime_dir() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(runtime);
        if path.is_absolute() && path.is_dir() {
            return path;
        }
    }
    std::env::temp_dir()
}

/// Removes what earlier runs left in the scratch directory.
///
/// **The one mechanism here that does not depend on anything having gone
/// right.** Everything else — the viewer releasing the file, the session
/// reaching its own cleanup — is a thing that usually happens. A process killed
/// mid-view, a viewer that opened the file without sharing delete, a machine
/// restarted: none of those clean up after themselves, and on Windows the
/// temporary directory is not emptied on boot.
///
/// It walks by age rather than by process id, because a process id is reused
/// and a directory named after a run that ended last week may be named after a
/// run in progress today. `older_than` is deliberately generous: a browsing
/// session is minutes, so hours is far past anything live.
///
/// Every failure is ignored on purpose. This is housekeeping, and a run that
/// cannot tidy up after an earlier one still has a story to show.
pub fn sweep_old_scratch(root: &Path, older_than: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return; // nothing has ever run, which is the common case
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > older_than)
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Creates a file only its owner can read, at exactly this name, and syncs
/// its contents to disk before returning.
///
/// The recipe existed twice -- the session fallback in `secrets` and the
/// monitor's `watch.toml` in `config` -- and the second copy was
/// `create(true).truncate(true)` for a while, which differs in two ways that
/// matter: an existing file at a predictable name is opened **with whatever
/// permissions it already had**, since the mode only applies at creation, and
/// the open follows a symlink. That defect was found in one copy and fixed
/// there; the next hardening lands here once instead.
///
/// The pieces, each load-bearing:
/// - anything left over from a failed run is removed first, which is what
///   makes `create_new` usable at a fixed name;
/// - `create_new`, not `create`: never open something that already exists,
///   never follow a link;
/// - on Unix the mode is set **at creation** -- a later chmod leaves a window
///   in which the file is readable by others;
/// - `sync_all` before returning: a file that is renamed into place with its
///   contents still in the page cache is not the protection against a
///   half-written file that writing to a temporary is for.
///
/// The caller owns what the name means -- `secrets` hands a temporary in and
/// renames it over the real name; `config` writes the real name directly --
/// and owns mapping the error into its own vocabulary.
pub fn write_new_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let _ = std::fs::remove_file(path);
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// Creates the directory and restricts it to its owner.
///
/// **Both halves of that sentence are enforced now.** On Unix it chmods 0700,
/// which it always did. On Windows it used to do nothing at all and rely on the
/// ACL inherited from `%LOCALAPPDATA%` — and the comment here said so, which is
/// the whole defect: on a machine whose profile ACL is not the default, nothing
/// inherited limits anything, and the database is the entire follower history
/// in the clear. `session.json` is DPAPI-sealed and was never the exposure;
/// the database and the browser profile were.
///
/// So on Windows this writes a DACL of its own, with
/// `PROTECTED_DACL_SECURITY_INFORMATION`, which is the flag that stops
/// inheritance rather than merely adding to it. One access-allowed ACE, for the
/// user this process is running as, and nothing else — the faithful reading of
/// 0700. Administrators and `SYSTEM` are deliberately not listed: on Unix root
/// is not in a 0700 mode either, and on Windows both hold the privileges that
/// let them take ownership regardless, so naming them would widen the written
/// rule without narrowing what anybody can actually reach.
///
/// **A failure is an error, not a warning**, for the same reason the Unix side
/// has always been one: a directory this could not restrict is a directory
/// holding the follower history where the check said it would not be, and a
/// caller that is told "fine" cannot act on it. It gets its own variant so the
/// sentence names what really went wrong instead of blaming the creation.
pub fn create_private_dir(dir: &Path) -> Result<(), PathError> {
    std::fs::create_dir_all(dir).map_err(|source| PathError::Create {
        path: dir.to_path_buf(),
        source,
    })?;
    restrict_to_this_account(dir)
}

/// Creates a private directory that **did not exist a moment ago**, under a
/// parent that is made private first.
///
/// For a directory whose name somebody else can predict, which is what a
/// scratch directory named after the process id is. [`create_private_dir`]
/// adopts whatever is already at the path: `create_dir_all` answers `Ok` when
/// the entry exists, a symbolic link to a directory included, and the
/// permissions are then set on **whatever it points at**. On a machine with
/// other users, a `/tmp/snob-ig-stories/run-<pid>` planted ahead of time as a
/// link was therefore a way to choose where a run wrote every story it
/// fetched, and to have a directory of the victim's own made `0700`. That
/// needs the parent to be creatable by anybody -- which `/tmp` is, and which
/// the parent was, because only the leaf was ever restricted.
///
/// Two things close it. The parent is created and restricted first, so on a
/// run that is the first to use it nobody else can put an entry inside; and
/// the leaf is created with `create_dir`, which fails rather than adopts when
/// something is already there. What is already there is removed if it is
/// something this tool could have left -- an earlier run's directory under a
/// reused process id, which Windows hands out again freely -- and the creation
/// is tried once more. A link is removed as a link, never followed:
/// `remove_dir_all` on a symbolic link deletes the link.
pub fn create_fresh_private_dir(dir: &Path) -> Result<(), PathError> {
    let create = |source| PathError::Create {
        path: dir.to_path_buf(),
        source,
    };
    if let Some(parent) = dir.parent() {
        create_private_dir(parent)?;
    }
    if let Err(e) = std::fs::create_dir(dir) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(create(e));
        }
        // `symlink_metadata` does not follow, so a link is seen as a link.
        let found = std::fs::symlink_metadata(dir).map_err(create)?;
        if found.is_dir() {
            std::fs::remove_dir_all(dir).map_err(create)?;
        } else if is_link_to_a_directory(&found) {
            // Windows keeps two kinds of link, and a link to a directory is
            // removed with `RemoveDirectory`: `DeleteFile` answers
            // `ERROR_ACCESS_DENIED` on it, which is how the planted-link test
            // failed on every machine where the link could be made at all --
            // Developer Mode, or the Administrator a CI runner is. Either way
            // this takes the link and leaves what it pointed at alone.
            std::fs::remove_dir(dir).map_err(create)?;
        } else {
            std::fs::remove_file(dir).map_err(create)?;
        }
        std::fs::create_dir(dir).map_err(create)?;
    }
    restrict_to_this_account(dir)
}

/// Whether a directory entry is a link whose target is a directory.
///
/// Only Windows tells the two kinds of link apart, and only there does it
/// matter: a directory link is a directory to the call that removes it. On
/// Unix every link is a file and `remove_file` takes it.
fn is_link_to_a_directory(found: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt as _;
        found.file_type().is_symlink_dir()
    }
    #[cfg(not(windows))]
    {
        let _ = found;
        false
    }
}

/// The half of [`create_private_dir`] that makes the directory private.
fn restrict_to_this_account(dir: &Path) -> Result<(), PathError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(dir, perms).map_err(|source| PathError::Create {
            path: dir.to_path_buf(),
            source,
        })?;
    }

    #[cfg(windows)]
    windows_acl::restrict_to_owner(dir)?;
    // The ACL says who may read it; this says who may copy it into a database
    // that outlives it. Two separate findings, one function.
    #[cfg(windows)]
    keep_out_of_the_search_index(dir);

    Ok(())
}

/// Marks a directory so that Windows Search does not index what is inside it.
///
/// **This closes a leak that was measured rather than imagined.** A query
/// against the live index in August 2026 returned files out of
/// `%LOCALAPPDATA%\snob-ig\data\browser-profile` — the profile that holds a
/// second copy of the session while a login is in progress. The crawl scope
/// includes `AppData\Local`, so everything this tool writes was being read by
/// the indexer and copied into its database, where deleting the original does
/// not remove it.
///
/// `FILE_ATTRIBUTE_NOT_CONTENT_INDEXED` is inherited by files created inside
/// afterwards, which was checked: a file created in a marked directory carries
/// the attribute without anything setting it. So this is set once, on the
/// directory, at creation.
///
/// Failure is ignored deliberately. The attribute is a defense in depth on top
/// of the ACL, not the thing keeping anybody out, and a tool that refuses to
/// run because an attribute would not set is worse than one that is indexed.
#[cfg(windows)]
fn keep_out_of_the_search_index(dir: &Path) {
    use std::os::windows::ffi::OsStrExt;

    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);

    // SAFETY: `SetFileAttributesW` reads a null-terminated wide string and
    // returns a boolean. The buffer above is null-terminated and outlives the
    // call, and the attribute value is a documented constant. Microsoft
    // documents that it does not clear other attributes when the value is
    // combined, so the directory bit is preserved by reading first.
    unsafe {
        let existing = windows_sys::Win32::Storage::FileSystem::GetFileAttributesW(wide.as_ptr());
        if existing == windows_sys::Win32::Storage::FileSystem::INVALID_FILE_ATTRIBUTES {
            return;
        }
        windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(
            wide.as_ptr(),
            existing | windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
        );
    }
}

/// Giving a directory a DACL that names only the user running this process.
///
/// Written against `windows-sys` rather than through a crate because it is one
/// call each to five documented functions, and because the shape of the answer
/// — a protected DACL with exactly one ACE — is what
/// `the_data_directory_is_not_readable_by_other_accounts` reads back.
#[cfg(windows)]
mod windows_acl {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
        GetLengthSid, GetTokenInformation, InitializeAcl, OBJECT_INHERIT_ACE,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::PathError;

    /// The user this process is running as, as a SID.
    ///
    /// The buffer is kept alongside the pointer because a `PSID` points into
    /// it: handing back the pointer alone would be a dangling one the moment
    /// the `Vec` dropped, and it would still work often enough to look correct.
    struct TokenUserSid {
        buffer: Vec<u8>,
    }

    impl TokenUserSid {
        fn sid(&self) -> PSID {
            // SAFETY: `buffer` holds a `TOKEN_USER` written by
            // `GetTokenInformation`, whose `User.Sid` points inside it.
            unsafe { (*(self.buffer.as_ptr() as *const TOKEN_USER)).User.Sid }
        }
    }

    fn last_error(dir: &Path, what: &str) -> PathError {
        PathError::NotPrivate {
            path: dir.to_path_buf(),
            // SAFETY: reads a thread-local error code.
            detail: format!("{what} failed: Windows error {}", unsafe { GetLastError() }),
        }
    }

    fn token_user(dir: &Path) -> Result<TokenUserSid, PathError> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo-handle for this process, and an out-parameter.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(last_error(dir, "OpenProcessToken"));
        }

        // Asked for its size first, which is the documented two-call shape.
        let mut needed: u32 = 0;
        // SAFETY: a null buffer with a zero length is how the size is asked
        // for; the call is expected to fail.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };

        let mut buffer = vec![0u8; needed as usize];
        // SAFETY: `buffer` is exactly the size the call just asked for.
        let read = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                needed,
                &mut needed,
            )
        };
        // SAFETY: a token handle this function owns.
        unsafe { CloseHandle(token) };

        if read == 0 {
            return Err(last_error(dir, "GetTokenInformation"));
        }
        Ok(TokenUserSid { buffer })
    }

    pub(super) fn restrict_to_owner(dir: &Path) -> Result<(), PathError> {
        let user = token_user(dir)?;
        let sid = user.sid();

        // SAFETY: a SID this function owns, still alive in `user`.
        let sid_length = unsafe { GetLengthSid(sid) };
        // An `ACCESS_ALLOWED_ACE` carries the first `u32` of the SID inside
        // itself, so the SID's length replaces that field rather than adding to
        // it. Getting this wrong is how an ACL ends up one DWORD short and
        // `AddAccessAllowedAceEx` fails with a length error nobody can read.
        let ace_size = std::mem::size_of::<windows_sys::Win32::Security::ACCESS_ALLOWED_ACE>()
            - std::mem::size_of::<u32>()
            + sid_length as usize;
        let acl_size = std::mem::size_of::<ACL>() + ace_size;

        let mut acl_buffer = vec![0u8; acl_size];
        let acl = acl_buffer.as_mut_ptr() as *mut ACL;

        // SAFETY: `acl` points at `acl_size` bytes, which is what is declared.
        if unsafe { InitializeAcl(acl, acl_size as u32, ACL_REVISION) } == 0 {
            return Err(last_error(dir, "InitializeAcl"));
        }

        // Inherited by what is created inside, because the point is the
        // database and the browser profile rather than the folder itself.
        //
        // SAFETY: the ACL was initialized with room for exactly this ACE.
        let added = unsafe {
            AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
                FILE_ALL_ACCESS,
                sid,
            )
        };
        if added == 0 {
            return Err(last_error(dir, "AddAccessAllowedAceEx"));
        }

        let mut wide: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // `PROTECTED_DACL_SECURITY_INFORMATION` is the half that matters. Set
        // the DACL without it and the inherited entries stay, which is the
        // situation this exists to end.
        //
        // SAFETY: a null-terminated path, and an ACL that outlives the call.
        let set = unsafe {
            SetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null_mut(),
            )
        };
        if set != 0 {
            return Err(PathError::NotPrivate {
                path: dir.to_path_buf(),
                detail: format!("SetNamedSecurityInfo failed: Windows error {set}"),
            });
        }
        Ok(())
    }

    /// What the directory's DACL actually says, for the test that reads it
    /// back. Returns whether the DACL is protected, and every SID in it.
    #[cfg(test)]
    pub(super) fn describe(dir: &Path) -> (bool, Vec<String>) {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, GetNamedSecurityInfoW,
        };
        use windows_sys::Win32::Security::{
            ACE_HEADER, ACL_SIZE_INFORMATION, AclSizeInformation, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, SE_DACL_PROTECTED, SECURITY_DESCRIPTOR_CONTROL,
        };

        let mut wide: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut acl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();

        // SAFETY: a null-terminated path and out-parameters; the descriptor is
        // freed below.
        let read = unsafe {
            GetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(read, 0, "GetNamedSecurityInfo failed");

        let mut control: SECURITY_DESCRIPTOR_CONTROL = 0;
        let mut revision: u32 = 0;
        // SAFETY: a descriptor the call above produced.
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
        let protected = control & SE_DACL_PROTECTED != 0;

        // SAFETY: an out parameter of three integers, which `GetAclInformation`
        // fills below; all-zero is a valid starting value for every one of them.
        let mut sizes: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the ACL points into the descriptor, which is still alive.
        unsafe {
            GetAclInformation(
                acl,
                &mut sizes as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };

        let mut sids = Vec::new();
        for index in 0..sizes.AceCount {
            let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: index is below the count the ACL just reported.
            if unsafe { GetAce(acl, index, &mut ace) } == 0 {
                continue;
            }
            // Every ACE type this can produce puts its SID immediately after
            // the access mask, which is one `u32` past the header.
            //
            // SAFETY: the layout of an access-allowed ACE.
            let sid = unsafe {
                (ace as *const u8)
                    .add(std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>())
                    as PSID
            };
            let mut text: *mut u16 = std::ptr::null_mut();
            // SAFETY: a SID inside the descriptor, and an out-parameter freed
            // immediately after it is read.
            unsafe {
                if ConvertSidToStringSidW(sid, &mut text) != 0 {
                    let mut length = 0;
                    while *text.add(length) != 0 {
                        length += 1;
                    }
                    sids.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                        text, length,
                    )));
                    LocalFree(text as *mut std::ffi::c_void);
                }
            }
        }

        // SAFETY: the descriptor the read produced, freed once.
        unsafe { LocalFree(descriptor) };
        (protected, sids)
    }

    /// This process's user, as a string SID, so a test can compare.
    #[cfg(test)]
    pub(super) fn current_user_sid() -> String {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

        let user = token_user(Path::new(".")).expect("this process has a token");
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: a SID this function owns, and an out-parameter freed after
        // it is read.
        unsafe {
            assert_ne!(ConvertSidToStringSidW(user.sid(), &mut text), 0);
            let mut length = 0;
            while *text.add(length) != 0 {
                length += 1;
            }
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
            LocalFree(text as *mut std::ffi::c_void);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A sandboxed run must not reach the real temporary directory.**
    /// The scratch directory is the one location that comes from the
    /// environment rather than from `directories`, so it is the one that would
    /// escape a root — and "every file this run touches is under one
    /// directory" is what `--sandbox-root` promises in `AGENTS.md`.
    #[test]
    fn a_rooted_run_keeps_its_scratch_under_the_root() {
        let paths = AppPaths::rooted_at("/tmp/test");
        assert!(paths.stories_root().starts_with("/tmp/test"));
        assert!(paths.story_scratch().starts_with(paths.stories_root()));
    }

    /// Two runs at once must not share a directory one of them will delete.
    #[test]
    fn each_run_gets_a_scratch_directory_of_its_own() {
        let paths = AppPaths::rooted_at("/tmp/test");
        let mine = paths.story_scratch();
        assert!(
            mine.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(&std::process::id().to_string())),
            "the run has to be named in the path: {}",
            mine.display()
        );
    }

    /// The scratch root is on the list `purge` reads. It is the only thing this
    /// tool writes outside the data directory, so it is the only one that could
    /// be forgotten there.
    #[test]
    fn the_scratch_root_is_something_purge_removes() {
        let paths = AppPaths::rooted_at("/tmp/test");
        assert!(paths.owned_dirs().contains(&paths.stories_root()));
    }

    /// The sweep takes what is old and leaves what is not. Both halves matter:
    /// deleting a live run's directory takes the file out from under the viewer
    /// it was just handed to.
    #[test]
    fn the_sweep_takes_the_abandoned_and_leaves_the_living() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("run-old");
        let fresh = tmp.path().join("run-fresh");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(old.join("story.jpg"), b"x").unwrap();

        // Zero, so everything already on disk counts as abandoned; then a
        // generous one, so nothing does. Testing it this way rather than by
        // waiting keeps the suite off the wall clock, which is a rule this
        // repository has broken before.
        sweep_old_scratch(tmp.path(), std::time::Duration::ZERO);
        assert!(!old.exists(), "an abandoned directory should have gone");

        std::fs::create_dir_all(&fresh).unwrap();
        sweep_old_scratch(tmp.path(), std::time::Duration::from_secs(3600));
        assert!(fresh.exists(), "a live directory must be left alone");
    }

    /// A root that does not exist is not an error. It is the common case: the
    /// first run of `snob stories` on a machine.
    #[test]
    fn sweeping_a_root_nothing_has_created_is_quiet() {
        sweep_old_scratch(
            Path::new("/nonexistent-snob-scratch"),
            std::time::Duration::ZERO,
        );
    }

    #[test]
    fn files_hang_off_their_directories() {
        let paths = AppPaths::rooted_at("/tmp/test");
        assert!(paths.db_file().starts_with(paths.data_dir()));
        assert!(paths.session_file().starts_with(paths.data_dir()));
        assert!(paths.browser_profile().starts_with(paths.data_dir()));
    }

    /// Only what gets written to. A directory for a configuration file nobody
    /// has written is litter left on every machine the tool runs on.
    ///
    /// There *is* a configuration file now — `snob watch setup` writes one —
    /// and this still holds, because that command creates the directory itself
    /// rather than `ensure_dirs` creating it for everybody. Somebody who never
    /// runs the monitor still gets no empty folder in their roaming profile,
    /// which is what this has always been about.
    #[test]
    fn only_the_data_directory_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        paths.ensure_dirs().unwrap();

        assert!(paths.data_dir().is_dir());
        assert!(
            !paths.config_dir().exists(),
            "the directory belongs to whatever writes configuration, not to every run"
        );
    }

    /// The data directory is limited to this account, and says so to the
    /// operating system rather than in a comment.
    ///
    /// This is what the old comment asserted and the old code did not do: it
    /// chmodded on Unix and did nothing at all on Windows, on the assumption
    /// that `%LOCALAPPDATA%` already limits access. On a machine whose profile
    /// ACL is not the default it does not, and the database inside is the whole
    /// follower history in the clear.
    ///
    /// Two things are read back, and the first is the one that is easy to lose:
    /// the DACL has to be **protected**, because a DACL set without that flag
    /// keeps every inherited entry and the exposure is exactly those entries.
    /// The second is that the only account named is this one.
    #[cfg(windows)]
    #[test]
    fn the_data_directory_is_not_readable_by_other_accounts() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        paths.ensure_dirs().unwrap();

        let (protected, sids) = windows_acl::describe(paths.data_dir());
        assert!(
            protected,
            "the DACL is not protected, so whatever the profile grants still applies"
        );
        assert_eq!(
            sids,
            vec![windows_acl::current_user_sid()],
            "somebody other than this account is named in the directory's DACL"
        );
    }

    /// Where the data goes must not depend on the directory the command was
    /// run from: it is the user's data, not the folder's.
    #[test]
    fn the_paths_do_not_depend_on_the_working_directory() {
        let Ok(paths) = AppPaths::discover() else {
            return; // no HOME in the test environment
        };
        assert!(
            paths.db_file().is_absolute(),
            "a relative path would follow whoever ran the command around"
        );
        assert!(paths.data_dir().is_absolute());
    }

    /// A scratch directory with a guessable name is never adopted.
    ///
    /// The planted entry is a symbolic link to a directory of the victim's
    /// own. `create_dir_all` answered `Ok` on it and the permissions then
    /// landed on the target, and every story fetched afterwards was written
    /// wherever the link pointed. The fresh variant removes the link as a
    /// link -- the target is untouched -- and makes a real directory.
    #[test]
    fn a_planted_link_under_the_scratch_name_is_replaced_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("theirs.txt"), b"untouched").unwrap();

        let root = tmp.path().join("snob-ig-stories");
        std::fs::create_dir(&root).unwrap();
        let leaf = root.join("run-1234");
        #[cfg(unix)]
        let planted = std::os::unix::fs::symlink(&elsewhere, &leaf).is_ok();
        // A link to a directory needs a privilege on Windows that a test
        // runner does not always have; without it the case is a leftover
        // directory, which the second half covers.
        #[cfg(windows)]
        let planted = std::os::windows::fs::symlink_dir(&elsewhere, &leaf).is_ok();

        create_fresh_private_dir(&leaf).unwrap();

        let made = std::fs::symlink_metadata(&leaf).unwrap();
        assert!(made.is_dir() && !made.is_symlink(), "a link was adopted");
        assert!(
            elsewhere.join("theirs.txt").exists(),
            "removing the link must not reach through it"
        );
        let _ = planted;

        // An earlier run's directory under a reused process id is replaced,
        // and what it held does not survive into the new run.
        std::fs::write(leaf.join("stale.jpg"), b"x").unwrap();
        create_fresh_private_dir(&leaf).unwrap();
        assert!(!leaf.join("stale.jpg").exists());
        assert!(leaf.is_dir());
    }

    /// The parent is made private before the leaf, so on a first run nobody
    /// else can put an entry inside it.
    #[cfg(unix)]
    #[test]
    fn the_scratch_root_is_private_too() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let leaf = tmp.path().join("snob-ig-stories").join("run-1");
        create_fresh_private_dir(&leaf).unwrap();
        for dir in [leaf.parent().unwrap(), leaf.as_path()] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
    }

    #[test]
    fn the_legacy_path_is_not_the_current_one() {
        let paths = AppPaths::rooted_at("/tmp/test");
        let legacy = paths.legacy_session_file().unwrap();
        assert_ne!(legacy, paths.session_file());
    }

    /// Everything `purge` has to take away has to be under something this
    /// returns, or the command quietly stops leaving nothing behind.
    #[test]
    fn every_directory_written_to_is_on_the_purge_list() {
        let paths = AppPaths::rooted_at("/tmp/test");
        let owned = paths.owned_dirs();

        for path in [
            paths.db_file(),
            paths.session_file(),
            paths.browser_profile(),
            paths.legacy_session_file().unwrap(),
            // The configuration file. Written since `snob watch setup` landed,
            // and missing from this list until it was: taking `self.config` out
            // of `owned_dirs` left `watch.toml` in the user's roaming profile
            // after `snob purge`, and nothing here noticed.
            crate::config::path(&paths),
        ] {
            assert!(
                owned.iter().any(|dir| path.starts_with(dir)),
                "{} is under no directory purge would remove",
                path.display()
            );
        }
    }

    /// On macOS the configuration and data directories are one path. Listed
    /// twice, the second removal fails on what the first already took.
    #[test]
    fn the_purge_list_has_no_repeats() {
        let Ok(paths) = AppPaths::discover() else {
            return; // no HOME in the test environment
        };
        let owned = paths.owned_dirs();
        let mut unique = owned.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), owned.len(), "{owned:?}");
    }

    /// The real directories have to survive the guard, or purge removes
    /// nothing at all — a failure that would look exactly like success.
    #[test]
    fn the_real_directories_are_removable() {
        let Ok(paths) = AppPaths::discover() else {
            return; // no HOME in the test environment
        };
        for dir in paths.owned_dirs() {
            assert!(is_safe_to_remove(&dir), "{}", dir.display());
        }
    }

    #[test]
    fn nothing_near_the_root_is_removable() {
        for dangerous in ["/", "/home", "/data", "C:\\", "C:\\Users"] {
            assert!(
                !is_safe_to_remove(Path::new(dangerous)),
                "{dangerous} should never be removable"
            );
        }
    }

    /// A `ProjectDirs` that resolved to the home directory itself would take
    /// everything the user owns with it.
    #[test]
    fn the_home_directory_is_not_removable() {
        let Some(base) = directories::BaseDirs::new() else {
            return;
        };
        assert!(!is_safe_to_remove(base.home_dir()));
    }

    /// The database must never end up in a directory that syncs. On Windows
    /// that means Local rather than Roaming; elsewhere both coincide and the
    /// check is trivially true.
    #[test]
    fn data_goes_to_the_local_directory() {
        let Ok(paths) = AppPaths::discover() else {
            return; // no HOME in the test environment
        };
        let dirs = directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION).unwrap();
        assert_eq!(paths.data_dir(), dirs.data_local_dir());
        #[cfg(windows)]
        assert!(!paths.db_file().to_string_lossy().contains("Roaming"));
    }
}
