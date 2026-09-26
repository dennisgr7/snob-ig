//! The profile a browser runs on, and the session it is handed: what snob
//! writes beside it, and how the cookies it carries are kept the right ones.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use snob_core::Pk;
use snob_core::session::Session;
use snob_store::paths::AppPaths;

use super::{COMMAND_TIMEOUT, Live};

/// Makes sure the browser carries this session, and only this account.
///
/// **A login is authoritative; after it, the browser is.** The browser's cookie
/// jar is fresher than anything stored — it is where `csrftoken` and `rur`
/// rotate — so a session it already carries is left alone. What tells "the
/// session it already carries" apart from "a new login's" is the profile mark
/// ([`ProfileMark`]): the fingerprint of the stored session last handed to
/// this profile. A stored session whose fingerprint is not the mark's came
/// from a login since, and its cookies are written in over whatever the
/// browser had. This used to ask only whether the browser held *a* session
/// for the account, so pasting a fresh session over a dead one changed
/// nothing, and every retry validated the dead one again.
///
/// **A browser already holding exactly the stored session is left alone** too,
/// whatever its mark says, and the mark is brought up to date. The mark is
/// written after the session is stored when the browser's own copy is written
/// back at the end of a run, so a run that stopped between the two leaves a
/// mark one session behind; the cookie itself is what says they agree.
///
/// **Another account's cookies are emptied out first.** Cookies and site data
/// both: `mid`, `ig_did` and `datr` name the device, and carrying one
/// account's into another's session is how two accounts come to look like one
/// person's. With a profile per account this should not happen at all, so it
/// is said when it does.
pub(super) async fn sync_cookies(
    live: &mut Live,
    session: &Session,
    origin: &str,
    mark: &mut ProfileMark,
) -> Result<()> {
    let host = url::Url::parse(origin)?
        .host_str()
        .unwrap_or_default()
        .to_string();
    let on_instagram = host == "instagram.com" || host.ends_with(".instagram.com");
    let ours = |c: &&Value| {
        let domain = c.get("domain").and_then(Value::as_str).unwrap_or("");
        if on_instagram {
            domain == "instagram.com" || domain.ends_with(".instagram.com")
        } else {
            domain.trim_start_matches('.') == host
        }
    };
    let named = |c: &Value, name: &str| c.get("name").and_then(Value::as_str) == Some(name);

    let jar = live
        .cdp
        .browser_call("Storage.getCookies", json!({}))
        .await?;
    let site: Vec<&Value> = jar
        .get("cookies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(ours)
        .collect();
    let this_account = format!("{}%3A", session.ds_user_id);
    let held = site
        .iter()
        .filter(|c| named(c, "sessionid"))
        .filter_map(|c| c.get("value").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let holds_this_account = held.iter().any(|v| v.starts_with(&this_account));
    let holds_another = held.iter().any(|v| !v.starts_with(&this_account))
        || mark.pk.is_some_and(|pk| pk != session.ds_user_id.get());
    let given = session.fingerprint();

    let holds_exactly = held.contains(&session.sessionid.expose());
    if holds_this_account
        && !holds_another
        && (mark.session.as_deref() == Some(given.as_str()) || holds_exactly)
    {
        mark.pk = Some(session.ds_user_id.get());
        mark.session = Some(given);
        return Ok(());
    }

    let mut device_kept = std::collections::HashSet::new();
    if holds_another {
        tracing::warn!(
            account = %session.ds_user_id,
            "the account's browser profile held another account's cookies; emptying it"
        );
        live.cdp
            .browser_call("Storage.clearCookies", json!({}))
            .await?;
        // Sent to the tab, not the browser: on the browser's own session this
        // answers "Internal error" whatever it is asked, measured on Chromium
        // 153, and on a tab's it clears.
        live.cdp
            .page_call(
                &live.tab,
                "Storage.clearDataForOrigin",
                json!({ "origin": origin, "storageTypes": "all" }),
                COMMAND_TIMEOUT,
            )
            .await?;
    } else {
        // The device cookies the browser already has are its own, and stay.
        for name in ["mid", "ig_did", "datr"] {
            if site.iter().any(|c| named(c, name)) {
                device_kept.insert(name);
            }
        }
    }

    let a_year = snob_core::clock::now().get() + 365 * 24 * 3600;
    let secure = origin.starts_with("https://");
    let cookie = |name: &str, value: &str, http_only: bool| {
        let mut c = json!({
            "name": name,
            "value": value,
            "path": "/",
            "secure": secure,
            "httpOnly": http_only,
            "expires": a_year,
        });
        if on_instagram {
            c["domain"] = json!(".instagram.com");
        } else {
            c["url"] = json!(format!("{origin}/"));
        }
        c
    };
    let mut set = vec![
        cookie("sessionid", session.sessionid.expose(), true),
        cookie("ds_user_id", &session.ds_user_id.to_string(), false),
    ];
    if let Some(token) = &session.csrftoken {
        set.push(cookie("csrftoken", token.expose(), false));
    }
    for (name, value) in [
        ("mid", session.mid.as_deref()),
        ("ig_did", session.ig_did.as_deref()),
        ("datr", session.datr.as_deref()),
    ] {
        if let Some(value) = value.filter(|v| !v.is_empty())
            && !device_kept.contains(name)
        {
            set.push(cookie(name, value, name != "mid"));
        }
    }
    live.cdp
        .browser_call("Storage.setCookies", json!({ "cookies": set }))
        .await?;

    mark.pk = Some(session.ds_user_id.get());
    mark.session = Some(given);
    Ok(())
}

/// What snob writes down beside the browser's profile, in the profile's own
/// directory so that it goes wherever the profile goes.
///
/// Two facts the profile cannot tell about itself. **Which browser made it**:
/// Chrome, Chromium and Edge each seal their cookies with a key of their own,
/// so a profile opened by the wrong one looks logged out at best, and a newer
/// profile opened by an older browser can be damaged — and "the first browser
/// found" changes the day somebody installs another. **Which session it was
/// given**: see [`sync_cookies`]. Neither is a secret; the session is named by
/// [`Session::fingerprint`], which cannot be turned back into the cookie.
///
/// A mark written while there was one profile also carried what the browser
/// said about the machine; that is kept for every profile now
/// ([`super::identity::KnownHints`]), and [`settle_the_layout`] moves it there.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileMark {
    /// The executable that created the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<PathBuf>,
    /// The account the profile holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pk: Option<u64>,
    /// The fingerprint of the stored session last handed to the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl ProfileMark {
    const FILE: &'static str = "snob-profile.json";

    /// The mark beside the profile at `profile`, or an empty one: a profile
    /// without a mark is one nothing is known about, which is what empty says.
    pub fn read(profile: &Path) -> Self {
        std::fs::read(profile.join(Self::FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Written in place: the file is a hint, and a torn one reads as empty,
    /// which costs one cookie write on the next run and nothing else.
    pub fn write(&self, profile: &Path) {
        let path = profile.join(Self::FILE);
        let written = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&path, bytes));
        if let Err(e) = written {
            tracing::debug!(error = %e, path = %path.display(), "could not write the profile mark");
        }
    }

    /// The mark a browser login leaves: the profile was made by `browser`, and
    /// the session it produced is the one being stored.
    pub fn after_login(profile: &Path, browser: &crate::browser::Browser, session: &Session) {
        Self {
            browser: Some(browser.path.clone()),
            pk: Some(session.ds_user_id.get()),
            session: Some(session.fingerprint()),
        }
        .write(profile);
    }
}

/// The profile an account's requests are sent from, created if it is not
/// there yet, and whether it was already there.
///
/// **One per account** ([`AppPaths::browser_profile_for`]): one account is
/// only ever sent from one browser, and two accounts never share a profile.
/// A profile from before that rule is moved under the account it holds first
/// ([`settle_the_layout`]).
pub fn for_account(paths: &AppPaths, pk: Pk) -> Result<(PathBuf, bool)> {
    settle_the_layout(paths, Some(pk))?;
    let profile = paths.browser_profile_for(pk);
    let existed = profile.is_dir();
    create_private(&paths.browser_profile())?;
    create_private(&profile)?;
    Ok((profile, existed))
}

/// A fresh profile for a login whose account is not known yet.
///
/// Named after this process, under the directory only this user can enter, and
/// made anew whatever was left at the name: a login that did not finish is the
/// only thing that leaves one. [`ProfileSwap`] puts it under its account once
/// the login says whose it is.
pub fn for_a_login(paths: &AppPaths) -> Result<PathBuf> {
    settle_the_layout(paths, None)?;
    create_private(&paths.browser_profile())?;
    let profile = paths
        .browser_profile()
        .join(format!("{LOGIN_PREFIX}{}", std::process::id()));
    snob_store::paths::create_fresh_private_dir(&profile)
        .with_context(|| format!("could not create {}", profile.display()))?;
    Ok(profile)
}

/// What a login's own profile is named after, before its account is known.
const LOGIN_PREFIX: &str = "login-";

/// Where a profile from before per-account profiles waits while it is moved.
/// Beside the directory, since it cannot be moved into itself in one step.
fn moving(paths: &AppPaths) -> PathBuf {
    paths.data_dir().join("browser-profile.moving")
}

/// Every place a profile holding a live session can be, for `snob logout`:
/// the directory all of them live under, and a move that did not finish.
pub fn every_profile(paths: &AppPaths) -> Vec<PathBuf> {
    vec![paths.browser_profile(), moving(paths)]
}

/// Moves a profile from before profiles were kept per account under the
/// account it holds.
///
/// That profile is the directory itself: Chromium's `Local State` and
/// `Default` at the top, and the mark beside them. It is renamed aside, the
/// directory is made again, and it goes back in under the account its mark
/// names — or, with no mark, the account asking for it, which is who it has
/// been sending as — and `unclaimed` when neither is known, where `snob
/// logout` still finds it. A move that stopped halfway is finished by the next
/// run, which finds it aside. What its mark said about the machine goes to
/// [`super::identity::KnownHints`].
///
/// **Never while a browser has it open.** Renamed under a running browser, the
/// profile would be written to where it no longer is: an older snob mid-run is
/// told to finish first, as a second snob always was.
fn settle_the_layout(paths: &AppPaths, asking: Option<Pk>) -> Result<()> {
    paths.ensure_dirs()?;
    let root = paths.browser_profile();
    let aside = moving(paths);
    if !aside.exists() {
        if !is_one_profile(&root) {
            return Ok(());
        }
        if in_use(&root) {
            bail!(
                "another snob is using the browser profile at {}, which this version keeps \
                 per account.\n\
                 Wait for that run to finish and try again.",
                root.display()
            );
        }
        rename_patiently(&root, &aside)?;
    }

    let mark = std::fs::read(aside.join(ProfileMark::FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    if let Some(hints) = mark
        .get("hints")
        .cloned()
        .and_then(|h| serde_json::from_value::<super::identity::MachineHints>(h).ok())
    {
        let mut known = super::identity::KnownHints::read(paths);
        known.remember(hints);
        known.write(paths);
    }
    let owner = mark
        .get("pk")
        .and_then(Value::as_u64)
        .map(Pk::new)
        .or(asking);
    create_private(&root)?;
    let target = match owner {
        Some(pk) => paths.browser_profile_for(pk),
        None => root.join("unclaimed"),
    };
    if target.exists() {
        // Only an interrupted run of this very move could have made it; the
        // copy aside is the one that was complete.
        snob_store::paths::remove_tree(&target)
            .with_context(|| format!("could not replace {}", target.display()))?;
    }
    rename_patiently(&aside, &target)?;
    // Written back without what it said about the machine, which lives in
    // its own file now.
    let mut settled = ProfileMark::read(&target);
    if settled.pk.is_none() {
        settled.pk = owner.map(Pk::get);
    }
    settled.write(&target);
    tracing::debug!(to = %target.display(), "moved the browser profile under its account");
    Ok(())
}

/// Whether `dir` is itself a browser profile, as the one profile from before
/// they were kept per account was.
fn is_one_profile(dir: &Path) -> bool {
    dir.join(ProfileMark::FILE).is_file()
        || dir.join("Local State").is_file()
        || dir.join("Default").is_dir()
}

/// Whether a browser on this machine has the profile at `dir` open.
///
/// Chromium locks a profile with a link naming the host and the process that
/// holds it. A lock left by a process that is gone, or by another host, is no
/// browser of this machine's. On Windows there is no such link; a rename under
/// an open browser fails there instead, which [`rename_patiently`] reports.
fn in_use(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        let Ok(target) = std::fs::read_link(dir.join("SingletonLock")) else {
            return false;
        };
        let target = target.to_string_lossy();
        let Some((host, pid)) = target.rsplit_once('-') else {
            return false;
        };
        let Ok(pid) = pid.parse::<i32>() else {
            return false;
        };
        if host != this_host() || pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 sends nothing; it asks whether the process exists.
        let asked = unsafe { libc::kill(pid, 0) };
        asked == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        false
    }
}

#[cfg(unix)]
fn this_host() -> String {
    let mut name = [0u8; 256];
    // SAFETY: the buffer is valid for its whole length, and the call writes at
    // most that many bytes into it.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return String::new();
    }
    std::ffi::CStr::from_bytes_until_nul(&name)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Renames a profile, a few times over a few seconds before giving up.
///
/// Windows keeps a directory while any handle inside it is open, and a
/// browser that has just been told to close takes a moment to let go of them
/// all — the same wait `snob login` gives a profile it removes.
fn rename_patiently(from: &Path, to: &Path) -> Result<()> {
    let mut last = None;
    for attempt in 0..10u64 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1)));
            }
        }
    }
    let why = last.map(|e| e.to_string()).unwrap_or_default();
    bail!(
        "could not move the browser profile at {} to {} ({why}); another snob may be \
         using it",
        from.display(),
        to.display()
    )
}

fn create_private(dir: &Path) -> Result<()> {
    snob_store::paths::create_private_dir(dir)
        .with_context(|| format!("could not create {}", dir.display()))
}

/// A login's profile, put under the account the login turned out to be.
///
/// The login happened in that profile, so it is the device Instagram just saw
/// sign in, and it takes the account's place: the account's older profile is
/// set aside rather than deleted until the session is known to be good, and
/// comes back if it is not. Nothing is moved when the login was made in the
/// account's own profile already.
pub struct ProfileSwap {
    /// Where the login's profile now is: the account's.
    pub profile: PathBuf,
    /// The account's older profile, set aside.
    aside: Option<PathBuf>,
    /// Whether anything was moved at all.
    moved: bool,
}

impl ProfileSwap {
    pub fn replace(paths: &AppPaths, used: &Path, pk: Pk) -> Result<Self> {
        let target = paths.browser_profile_for(pk);
        if used == target {
            return Ok(Self {
                profile: target,
                aside: None,
                moved: false,
            });
        }
        let aside = if target.exists() {
            let aside = paths
                .browser_profile()
                .join(format!("{pk}.replaced-{}", std::process::id()));
            if aside.exists() {
                snob_store::paths::remove_tree(&aside)
                    .with_context(|| format!("could not remove {}", aside.display()))?;
            }
            rename_patiently(&target, &aside)?;
            Some(aside)
        } else {
            None
        };
        if let Err(e) = rename_patiently(used, &target) {
            if let Some(aside) = &aside {
                let _ = std::fs::rename(aside, &target);
            }
            return Err(e);
        }
        Ok(Self {
            profile: target,
            aside,
            moved: true,
        })
    }

    /// The session is good: the older profile goes.
    pub fn keep(self) {
        if let Some(aside) = &self.aside
            && let Err(e) = snob_store::paths::remove_tree(aside)
        {
            tracing::warn!(error = %e, path = %aside.display(), "could not remove the replaced profile");
        }
    }

    /// The session is not good: the login's profile goes, holding a session
    /// Instagram has just refused, and the older one comes back.
    pub fn undo(self) {
        if !self.moved {
            return;
        }
        if let Err(e) = snob_store::paths::remove_tree(&self.profile) {
            tracing::warn!(error = %e, path = %self.profile.display(), "could not remove the login's profile");
            return;
        }
        if let Some(aside) = &self.aside
            && let Err(e) = std::fs::rename(aside, &self.profile)
        {
            tracing::warn!(error = %e, path = %aside.display(), "could not put the older profile back");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(root: &Path) -> AppPaths {
        AppPaths::rooted_at(root)
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    /// The one profile from before goes under the account its mark names —
    /// not the one asking — and what it said about the machine leaves it for
    /// the file every profile shares.
    #[test]
    fn a_single_profile_moves_under_the_account_it_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let root = paths.browser_profile();
        touch(&root.join("Local State"));
        std::fs::write(
            root.join(ProfileMark::FILE),
            serde_json::to_vec(&json!({
                "pk": 42,
                "session": "0123456789abcdef",
                "hints": { "browser": "/usr/bin/chromium", "version": "141.0.1.2", "values": {} },
            }))
            .unwrap(),
        )
        .unwrap();

        let (asking, existed) = for_account(&paths, Pk::new(99)).unwrap();
        assert_eq!(asking, paths.browser_profile_for(Pk::new(99)));
        assert!(!existed, "the account asking had no profile of its own");

        let moved = paths.browser_profile_for(Pk::new(42));
        assert!(moved.join("Local State").is_file());
        assert!(!root.join("Local State").exists());
        let mark = ProfileMark::read(&moved);
        assert_eq!(mark.pk, Some(42));
        assert_eq!(mark.session.as_deref(), Some("0123456789abcdef"));
        let written = std::fs::read_to_string(moved.join(ProfileMark::FILE)).unwrap();
        assert!(!written.contains("hints"), "{written}");
        let known = super::super::identity::KnownHints::read(&paths);
        assert_eq!(known.hints.len(), 1);
        assert!(!moving(&paths).exists());
    }

    /// With no mark to say whose it is, it goes to the account asking for
    /// it, which is who it has been sending as.
    #[test]
    fn with_no_mark_the_profile_goes_to_the_account_asking() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        touch(&paths.browser_profile().join("Default").join("Cookies"));

        let (profile, existed) = for_account(&paths, Pk::new(7)).unwrap();
        assert!(existed);
        assert!(profile.join("Default").join("Cookies").is_file());
        assert_eq!(ProfileMark::read(&profile).pk, Some(7));
    }

    /// A login whose account is not known yet leaves it unclaimed, where
    /// `snob logout` still finds it.
    #[test]
    fn with_nobody_to_claim_it_the_profile_is_kept_unclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        touch(&paths.browser_profile().join("Local State"));

        let fresh = for_a_login(&paths).unwrap();
        assert!(fresh.is_dir());
        assert!(
            paths
                .browser_profile()
                .join("unclaimed")
                .join("Local State")
                .is_file()
        );
        assert!(
            fresh.starts_with(paths.browser_profile()),
            "a login's profile is where `snob logout` looks"
        );
    }

    /// A move stopped between its two renames is finished by the next run.
    #[test]
    fn a_move_that_stopped_halfway_is_finished() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        paths.ensure_dirs().unwrap();
        let aside = moving(&paths);
        touch(&aside.join("Local State"));
        ProfileMark {
            pk: Some(5),
            ..ProfileMark::default()
        }
        .write(&aside);

        let (profile, existed) = for_account(&paths, Pk::new(5)).unwrap();
        assert!(existed);
        assert!(profile.join("Local State").is_file());
        assert!(!aside.exists());
    }

    /// Profiles already kept per account are left exactly as they are.
    #[test]
    fn a_profile_per_account_is_not_moved_again() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let mine = paths.browser_profile_for(Pk::new(42));
        touch(&mine.join("Local State"));
        let other = paths.browser_profile_for(Pk::new(43));
        touch(&other.join("Local State"));

        let (profile, existed) = for_account(&paths, Pk::new(42)).unwrap();
        assert_eq!(profile, mine);
        assert!(existed);
        assert!(other.join("Local State").is_file());
    }

    /// Renamed under a browser that has it open, the profile would be written
    /// to where it no longer is: a live lock on this machine is left alone.
    #[cfg(unix)]
    #[test]
    fn a_profile_a_browser_has_open_is_left_where_it_is() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let root = paths.browser_profile();
        touch(&root.join("Local State"));
        std::os::unix::fs::symlink(
            format!("{}-{}", this_host(), std::process::id()),
            root.join("SingletonLock"),
        )
        .unwrap();

        let refused = for_account(&paths, Pk::new(42)).unwrap_err();
        assert!(
            format!("{refused:#}").contains("another snob"),
            "{refused:#}"
        );
        assert!(root.join("Local State").is_file(), "nothing was moved");
    }

    /// The login's profile takes the account's place, and the older one
    /// comes back if the session turns out not to be good.
    #[test]
    fn a_login_takes_the_accounts_place_and_gives_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let account = paths.browser_profile_for(Pk::new(42));
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("which"), b"older").unwrap();
        let login = for_a_login(&paths).unwrap();
        std::fs::write(login.join("which"), b"login").unwrap();

        let swap = ProfileSwap::replace(&paths, &login, Pk::new(42)).unwrap();
        assert_eq!(swap.profile, account);
        assert_eq!(std::fs::read(account.join("which")).unwrap(), b"login");
        assert!(!login.exists());
        swap.undo();
        assert_eq!(std::fs::read(account.join("which")).unwrap(), b"older");

        let login = for_a_login(&paths).unwrap();
        std::fs::write(login.join("which"), b"login").unwrap();
        ProfileSwap::replace(&paths, &login, Pk::new(42))
            .unwrap()
            .keep();
        assert_eq!(std::fs::read(account.join("which")).unwrap(), b"login");
        let left: Vec<_> = std::fs::read_dir(paths.browser_profile())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            left,
            [std::ffi::OsString::from("42")],
            "nothing set aside is left"
        );
    }

    /// A login made in the account's own profile moves nothing, and undoing
    /// it removes nothing.
    #[test]
    fn a_login_in_the_accounts_own_profile_moves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let (account, _) = for_account(&paths, Pk::new(42)).unwrap();
        std::fs::write(account.join("which"), b"own").unwrap();

        let swap = ProfileSwap::replace(&paths, &account, Pk::new(42)).unwrap();
        swap.undo();
        assert_eq!(std::fs::read(account.join("which")).unwrap(), b"own");
    }
}
