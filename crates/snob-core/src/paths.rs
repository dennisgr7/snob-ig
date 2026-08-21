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

/// Creates the directory and, on Unix, restricts it to its owner. On Windows
/// the ACL inherited from `%LOCALAPPDATA%` already limits access to the user.
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

pub fn create_private_dir(dir: &Path) -> Result<(), PathError> {
    std::fs::create_dir_all(dir).map_err(|source| PathError::Create {
        path: dir.to_path_buf(),
        source,
    })?;

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
            crate::watch::config::path(&paths),
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
