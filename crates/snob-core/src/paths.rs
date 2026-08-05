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
        }
    }

    /// Where configuration **would** live. Nothing writes there yet, and the
    /// directory is deliberately not created until something does: an empty
    /// folder in the user's roaming profile is litter, and this tool left one
    /// on every machine it ran on.
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

    /// Browser profile used by `snob login`. Never the user's real profile.
    pub fn browser_profile(&self) -> PathBuf {
        self.data.join("browser-profile")
    }

    /// Every directory this tool may have created, for `snob purge` to remove.
    ///
    /// Assembled here rather than by the command so that a directory added
    /// later cannot be forgotten by the one command whose whole job is to leave
    /// nothing behind. The configuration directory is on the list even though
    /// nothing writes there yet, for that same reason.
    ///
    /// Deduplicated, because on macOS the configuration and data directories
    /// are the same path: listed twice, the second removal would fail on a
    /// directory the first one had already taken and be reported as a problem.
    pub fn owned_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for dir in [
            Some(&self.data),
            Some(&self.config),
            self.legacy_data.as_ref(),
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
    /// Only the data directory: there is no configuration file, so creating a
    /// folder for one leaves an empty directory in the roaming profile of every
    /// machine this has ever run on. When configuration arrives, whatever
    /// writes it creates its own directory.
    pub fn ensure_dirs(&self) -> Result<(), PathError> {
        create_private_dir(&self.data)
    }
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_hang_off_their_directories() {
        let paths = AppPaths::rooted_at("/tmp/test");
        assert!(paths.db_file().starts_with(paths.data_dir()));
        assert!(paths.session_file().starts_with(paths.data_dir()));
        assert!(paths.browser_profile().starts_with(paths.data_dir()));
    }

    /// Only what gets written to. A directory for a configuration file that
    /// does not exist is litter left on every machine the tool runs on.
    #[test]
    fn only_the_data_directory_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        paths.ensure_dirs().unwrap();

        assert!(paths.data_dir().is_dir());
        assert!(
            !paths.config_dir().exists(),
            "nothing writes configuration yet, so nothing should create its home"
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
