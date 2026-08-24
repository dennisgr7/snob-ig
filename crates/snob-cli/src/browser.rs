//! Detection of the installed browser and its User-Agent.
//!
//! This exists so the user never has to copy the User-Agent out of the
//! developer tools console. That had two serious problems: Instagram shows a
//! full-width warning in that console saying that anyone asking you to paste
//! something there is scamming you, and copying from a console easily drags in
//! dozens of log lines which, pasted into a terminal, run as separate commands.
//!
//! Chrome's User-Agent is reconstructible: since Google reduced it, only the
//! major version number varies, and that comes from the browser itself.

use std::path::PathBuf;
#[cfg(not(windows))]
use std::process::Command;

use snob_core::session::Session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browser {
    pub name: &'static str,
    pub path: PathBuf,
    pub major_version: u32,
    /// Chromium derivatives append their own brand at the end.
    suffix: Option<String>,
}

impl Browser {
    /// Rebuilds the User-Agent this browser sends.
    pub fn user_agent(&self) -> String {
        let platform = platform();
        let mut ua = format!(
            "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/{}.0.0.0 Safari/537.36",
            self.major_version
        );
        if let Some(suffix) = &self.suffix {
            ua.push_str(&format!(" {suffix}/{}.0.0.0", self.major_version));
        }
        ua
    }
}

/// The platform part of the User-Agent.
///
/// On Windows for ARM, Chrome still announces `Win64; x64` for compatibility,
/// so there is no distinction by architecture.
fn platform() -> &'static str {
    #[cfg(windows)]
    {
        "Windows NT 10.0; Win64; x64"
    }
    #[cfg(target_os = "macos")]
    {
        "Macintosh; Intel Mac OS X 10_15_7"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "X11; Linux x86_64"
    }
}

/// How long a stored User-Agent is trusted before the installed browser is
/// looked at again.
///
/// Browsers update every few weeks, so a day is far more often than needed. The
/// point of having an interval at all is that on Linux the check runs the
/// browser to ask its version, and paying eighty milliseconds on every single
/// command to learn nothing is not worth it.
const RECHECK_AFTER_SECS: i64 = 24 * 3600;

/// Brings a stored User-Agent up to date with the browser that is installed now.
///
/// A session whose User-Agent says Chrome 141 while the machine has been on 151
/// for months is an anomaly that grows on its own: the real browser started
/// sending the new version with that same cookie the moment it updated, so the
/// stale one is ours alone. Instagram tolerates the version moving — otherwise
/// every Chrome update would sign everybody out — which is exactly why keeping
/// up is safe and standing still is not.
///
/// Only the version is touched, never the shape. Rebuilding the whole string
/// would throw away whatever the real browser said about itself, and the point
/// is to follow that browser rather than to replace it.
pub fn refresh_user_agent(session: &mut Session) -> bool {
    if session.user_agent_pinned {
        return false;
    }

    let now = snob_core::clock::now();
    if session
        .user_agent_checked_at
        .is_some_and(|last| now - last < RECHECK_AFTER_SECS)
    {
        return false;
    }
    session.user_agent_checked_at = Some(now);

    // The browser this session belongs to, not whichever comes first: a
    // session created in Edge must not be handed Chrome's version number, and
    // on a machine with both that is exactly what the preference order gives.
    let installed = match &session.browser {
        Some(name) => detect_named(name),
        None => detect(),
    };
    let Some(installed) = installed else {
        return true; // nothing to compare against; the timestamp still moved
    };

    match bumped_to(&session.user_agent, installed.major_version) {
        Some(fresh) => {
            tracing::debug!(
                from = %session.user_agent,
                to = %fresh,
                "the installed browser moved on; following it"
            );
            session.user_agent = fresh;
            true
        }
        None => true,
    }
}

/// Rewrites the major version inside a User-Agent, leaving everything else
/// byte for byte. `None` when there was nothing to change.
///
/// Since Chrome reduced its User-Agent the minor, build and patch parts are
/// frozen at zero, so the major really is the only thing that moves — which is
/// what makes a substitution correct rather than an approximation. Derivatives
/// carry their own token (`Edg/`, `OPR/`) and it moves with the same number.
fn bumped_to(user_agent: &str, major: u32) -> Option<String> {
    let mut fresh = user_agent.to_string();
    let mut changed = false;

    // `OPR/` is deliberately absent: Opera's own version does not follow
    // Chromium's — it runs dozens of majors behind — so moving it to match
    // would invent a version that has never existed.
    for token in ["Chrome/", "Edg/"] {
        let Some(at) = fresh.find(token) else {
            continue;
        };
        let from = at + token.len();
        let len = fresh[from..]
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(fresh.len() - from);
        if len == 0 {
            continue;
        }
        if fresh[from..from + len] != *major.to_string() {
            fresh.replace_range(from..from + len, &major.to_string());
            changed = true;
        }
    }

    changed.then_some(fresh)
}

/// The first Chromium-based browser found, in order of preference.
pub fn detect() -> Option<Browser> {
    detect_all().into_iter().next()
}

/// Every Chromium-based browser installed, in order of preference.
///
/// Worth having all of them rather than just the first: a machine with both
/// Chrome and Edge is ordinary, the Instagram session lives in exactly one of
/// them, and picking for the user is picking wrong half the time.
pub fn detect_all() -> Vec<Browser> {
    probe(|_| true)
}

/// The one brand asked for, without launching the others.
///
/// `refresh_user_agent` wants exactly one — the browser the session belongs to
/// — and used to get it by detecting everything and then filtering. Off Windows
/// detection launches each candidate with `--version` and blocks on the answer,
/// so reading one version cost three processes.
///
/// It keeps `detect_all`'s semantics deliberately: every path of that brand is
/// tried and it stops on the first that yields a **version**, not on the first
/// that exists. On Linux the candidates are `$PATH` crossed with several
/// executable names, so a path can be there and answer nothing.
pub fn detect_named(name: &str) -> Option<Browser> {
    probe(|candidate| candidate == name).into_iter().next()
}

fn probe(wanted: impl Fn(&str) -> bool) -> Vec<Browser> {
    let mut found = Vec::new();
    for (name, suffix, paths) in candidates() {
        if !wanted(name) {
            continue;
        }
        for path in paths {
            if !path.is_file() {
                continue;
            }
            if let Some(major_version) = version_of(&path) {
                found.push(Browser {
                    name,
                    path,
                    major_version,
                    suffix: suffix.clone(),
                });
                // One entry per brand: the same browser turns up under several
                // roots when it is installed both per-user and per-machine.
                break;
            }
        }
    }
    found
}

type Candidate = (&'static str, Option<String>, Vec<PathBuf>);

#[cfg(windows)]
fn candidates() -> Vec<Candidate> {
    // Per-user installs under LOCALAPPDATA are the case people forget, and they
    // are fairly common.
    let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(|v| std::env::var_os(v).map(PathBuf::from))
        .collect();

    let under =
        |relative: &str| -> Vec<PathBuf> { roots.iter().map(|r| r.join(relative)).collect() };

    vec![
        (
            "Chrome",
            None,
            under(r"Google\Chrome\Application\chrome.exe"),
        ),
        (
            "Edge",
            Some("Edg".to_string()),
            under(r"Microsoft\Edge\Application\msedge.exe"),
        ),
        (
            "Brave",
            None,
            under(r"BraveSoftware\Brave-Browser\Application\brave.exe"),
        ),
    ]
}

#[cfg(target_os = "macos")]
fn candidates() -> Vec<Candidate> {
    vec![
        (
            "Chrome",
            None,
            vec![PathBuf::from(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            )],
        ),
        (
            "Edge",
            Some("Edg".to_string()),
            vec![PathBuf::from(
                "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            )],
        ),
        (
            "Brave",
            None,
            vec![PathBuf::from(
                "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            )],
        ),
    ]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn candidates() -> Vec<Candidate> {
    let on_path = |names: &[&str]| -> Vec<PathBuf> {
        let Some(path) = std::env::var_os("PATH") else {
            return Vec::new();
        };
        std::env::split_paths(&path)
            .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
            .collect()
    };

    vec![
        (
            "Chrome",
            None,
            on_path(&["google-chrome-stable", "google-chrome", "chromium"]),
        ),
        (
            "Edge",
            Some("Edg".to_string()),
            on_path(&["microsoft-edge"]),
        ),
        ("Brave", None, on_path(&["brave-browser"])),
    ]
}

/// Works out the browser's major version.
///
/// On Windows this is read from disk rather than by launching the process:
/// Chrome is a GUI application, it detaches from the console, and `--version`
/// leaves nothing for a child process to capture. Conveniently, Chromium keeps
/// its files in a directory named after the version right next to the
/// executable, and that holds for Chrome, Edge and Brave alike.
#[cfg(windows)]
fn version_of(path: &std::path::Path) -> Option<u32> {
    let directory = path.parent()?;
    std::fs::read_dir(directory)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| major_version_from_text(&e.file_name().to_string_lossy()))
        .max()
}

/// Off Windows the executable can be asked directly; it prints something like
/// "Google Chrome 151.0.7922.47".
#[cfg(not(windows))]
fn version_of(path: &std::path::Path) -> Option<u32> {
    let output = Command::new(path).arg("--version").output().ok()?;
    major_version_from_text(&String::from_utf8_lossy(&output.stdout))
}

fn major_version_from_text(text: &str) -> Option<u32> {
    text.split_whitespace()
        .find_map(|word| {
            let major = word.split('.').next()?;
            // It has to be the first part of something shaped like a version,
            // not a stray number from the product name.
            if word.contains('.') {
                major.parse::<u32>().ok()
            } else {
                None
            }
        })
        .filter(|v| *v > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_reads_the_major_version_from_what_the_browser_prints() {
        assert_eq!(
            major_version_from_text("Google Chrome 151.0.7922.47"),
            Some(151)
        );
        assert_eq!(
            major_version_from_text("Microsoft Edge 140.0.3485.14"),
            Some(140)
        );
        assert_eq!(major_version_from_text("Brave Browser 1.60.114"), Some(1));
        assert_eq!(
            major_version_from_text("Chromium 138.0.7204.100\n"),
            Some(138)
        );
    }

    #[test]
    fn it_does_not_invent_a_version_when_there_is_none() {
        assert_eq!(major_version_from_text(""), None);
        assert_eq!(major_version_from_text("not a browser"), None);
        assert_eq!(major_version_from_text("Chrome 5"), None);
    }

    #[test]
    fn it_recognizes_a_version_directory_name() {
        // This is how the version is detected on Windows.
        assert_eq!(major_version_from_text("151.0.7922.47"), Some(151));
        // And Chromium's other subdirectories do not slip through.
        assert_eq!(major_version_from_text("Locales"), None);
        assert_eq!(major_version_from_text("SetupMetrics"), None);
    }

    #[test]
    fn the_user_agent_has_the_shape_instagram_expects() {
        let b = Browser {
            name: "Chrome",
            path: PathBuf::new(),
            major_version: 151,
            suffix: None,
        };
        let ua = b.user_agent();
        assert!(ua.starts_with("Mozilla/5.0 ("));
        assert!(ua.contains("Chrome/151.0.0.0"));
        assert!(ua.ends_with("Safari/537.36"));
        // The session validator rejects anything not starting like this.
        assert!(
            snob_core::session::Session::from_sessionid(
                "1%3Aa%3A2",
                &ua,
                snob_core::session::SessionOrigin::Paste
            )
            .is_ok()
        );
    }

    #[test]
    fn edge_appends_its_brand() {
        let b = Browser {
            name: "Edge",
            path: PathBuf::new(),
            major_version: 140,
            suffix: Some("Edg".to_string()),
        };
        assert!(b.user_agent().ends_with("Edg/140.0.0.0"));
    }

    fn session_with(user_agent: &str) -> Session {
        Session::from_sessionid(
            "1%3Aa%3A2",
            user_agent,
            snob_core::session::SessionOrigin::Paste,
        )
        .unwrap()
    }

    #[test]
    fn only_the_version_moves() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
        assert_eq!(
            bumped_to(ua, 151).unwrap(),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36"
        );
    }

    /// A derivative carries two version tokens and both move together, because
    /// on the real browser they do.
    #[test]
    fn a_derivative_has_both_of_its_versions_moved() {
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                    (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";
        let fresh = bumped_to(edge, 151).unwrap();
        assert!(fresh.contains("Chrome/151.0.0.0"), "{fresh}");
        assert!(fresh.contains("Edg/151.0.0.0"), "{fresh}");
    }

    #[test]
    fn nothing_to_change_reports_nothing() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";
        assert_eq!(bumped_to(ua, 151), None);
        // A browser with no version token of ours in it is left alone.
        let firefox = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:142.0) \
                       Gecko/20100101 Firefox/142.0";
        assert_eq!(bumped_to(firefox, 151), None);
    }

    /// The User-Agent the user typed is theirs. Following the installed browser
    /// is a convenience for the one we worked out ourselves, never a correction
    /// of an explicit choice.
    #[test]
    fn a_pinned_user_agent_is_never_rewritten() {
        let mut session = session_with(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/100.0.0.0 Safari/537.36",
        );
        session.user_agent_pinned = true;
        let before = session.user_agent.clone();

        assert!(!refresh_user_agent(&mut session));
        assert_eq!(session.user_agent, before);
        assert_eq!(session.user_agent_checked_at, None);
    }

    /// The check is throttled so that on Linux, where it costs a process
    /// launch, it does not run on every single command.
    ///
    /// Nothing here is measured against the wall clock. The timestamp is set
    /// before `detect` runs, and `detect` launches one process per installed
    /// browser — on a busy machine with three of them that took longer than
    /// the few seconds an earlier version of this test allowed, and the test
    /// failed for being slow rather than for being wrong. What matters is that
    /// the timestamp moved and that the throttle closed behind it, and both
    /// can be asserted without asking what time it is.
    #[test]
    fn a_recent_check_is_not_repeated() {
        let mut session = session_with(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/100.0.0.0 Safari/537.36",
        );
        session.user_agent_checked_at = Some(snob_core::clock::now());
        assert!(!refresh_user_agent(&mut session));

        // Past the interval it looks again, whatever it then decides.
        let stale =
            snob_core::clock::now() - std::time::Duration::from_secs(RECHECK_AFTER_SECS as u64 + 1);
        session.user_agent_checked_at = Some(stale);
        assert!(refresh_user_agent(&mut session));

        let moved = session.user_agent_checked_at.unwrap();
        assert!(moved > stale, "the check ran, so its timestamp must move");
        assert!(
            !refresh_user_agent(&mut session),
            "and having moved, it is inside the throttling window again"
        );
    }

    /// If there is a browser on this machine, the reconstructed User-Agent has
    /// to be acceptable to the session validator.
    #[test]
    fn the_browser_on_this_machine_yields_a_valid_user_agent() {
        let Some(b) = detect() else {
            return; // no browser installed, nothing to check
        };
        assert!(b.major_version > 0);
        let ua = b.user_agent();
        assert!(ua.starts_with("Mozilla/5.0 ("), "{ua}");
        assert!(ua.contains(&format!("Chrome/{}.0.0.0", b.major_version)));
    }
}
