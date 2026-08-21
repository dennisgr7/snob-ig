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

    Ok(())
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
