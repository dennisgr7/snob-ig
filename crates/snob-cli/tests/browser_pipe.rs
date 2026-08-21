//! The login browser, against a real browser.
//!
//! Everything else about `cdp` and `pipe` is tested without one — the framing,
//! the reply matching, the inheritance blob, the argument quoting. What cannot
//! be tested that way is the only thing that actually matters about this
//! change: that Chromium finds the protocol where we put it, and that no
//! loopback port exists any more.
//!
//! **Skipped when no browser is installed**, rather than failed. A machine with
//! no Chromium is a machine where `snob login --browser` is not an option
//! anyway, and a test that cannot run there should say nothing rather than
//! something false.
//!
//! Nothing here logs into anything. The browser is pointed at its own empty
//! profile under a temporary directory, asked one question about itself, and
//! closed.

use std::time::Duration;

use snob_cli::{browser, cdp};
use snob_core::paths::AppPaths;
use snob_ig::pace::CancelToken;

/// The one thing that has to be true of the transport: a browser we started
/// speaks the protocol over the pipe, and there is no port anywhere.
///
/// The port is the finding this closes. With `--remote-debugging-port=0` the
/// browser wrote `DevToolsActivePort` into its profile and listened on
/// 127.0.0.1, where a second local process read the session cookie out of it
/// through `Storage.getCookies` despite `httpOnly`. Loopback has no per-user
/// access control, so that was every account on the machine.
#[tokio::test]
async fn the_browser_answers_on_the_pipe_and_opens_no_port() {
    let Some(found) = browser::detect() else {
        eprintln!("no browser installed; skipping");
        return;
    };

    let temporary = tempfile::tempdir().expect("a temporary directory");
    let paths = AppPaths::rooted_at(temporary.path());
    let cancel = CancelToken::default();

    let launched = cdp::launch(&found, &paths, &cancel)
        .await
        .expect("the browser starts");
    let mut cdp = cdp::Cdp::connect(launched, &cancel)
        .await
        .expect("the browser answers its debugging pipe");

    let user_agent = cdp.user_agent().await.expect("it reports its User-Agent");
    assert!(
        user_agent.contains("Mozilla/5.0"),
        "that is not a User-Agent: {user_agent}"
    );

    // The file the old transport wrote, and the one anything looking for the
    // port would read. `launch` removes a stale copy before starting, so its
    // absence here means this browser did not write one.
    let active_port = paths.browser_profile().join("DevToolsActivePort");
    assert!(
        !active_port.exists(),
        "the browser wrote {} , so it is listening on a port after all",
        active_port.display()
    );

    // And the port itself, asked of the operating system rather than inferred
    // from the absence of a file. The browser process is the one that used to
    // listen; its renderers never did.
    #[cfg(windows)]
    assert_eq!(
        listening_ports(cdp.browser_pid()),
        Vec::<String>::new(),
        "the browser is listening on a socket, which is what this change removed"
    );

    cdp.close().await;
}

/// Every TCP socket this pid is listening on.
///
/// Read from `netstat` rather than from a crate, because what is being checked
/// is what an attacker would see: the operating system's own list of what can
/// be connected to.
#[cfg(windows)]
fn listening_ports(pid: u32) -> Vec<String> {
    let mut netstat = std::path::PathBuf::from(
        std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()),
    );
    netstat.push(r"System32\netstat.exe");
    let output = std::process::Command::new(netstat)
        .args(["-ano", "-p", "TCP"])
        .output()
        .expect("netstat runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("LISTENING"))
        .filter(|line| line.split_whitespace().last() == Some(&pid.to_string()))
        .map(|line| line.split_whitespace().nth(1).unwrap_or("?").to_string())
        .collect()
}

/// Snob killed from outside takes the browser with it.
///
/// This is the case no handler of ours can catch — Task Manager, `taskkill /F`,
/// a service manager's stop timeout — and it is what the job object is for.
/// Dropping the process handle is the same kernel event that a killed snob
/// produces: the last handle to the job closes, and
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` empties it. Doing it by drop is what
/// makes the test deterministic rather than a race against a second process.
#[cfg(windows)]
#[tokio::test]
async fn losing_the_job_handle_takes_the_browser_down() {
    let Some(found) = browser::detect() else {
        eprintln!("no browser installed; skipping");
        return;
    };

    let temporary = tempfile::tempdir().expect("a temporary directory");
    let profile = temporary.path().join("profile");
    let arguments = vec![
        format!("--user-data-dir={}", profile.display()),
        "--remote-debugging-pipe".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "about:blank".to_string(),
    ];

    let (process, _transport) =
        snob_cli::pipe::spawn(&found.path, &arguments).expect("the browser starts");
    let pid = process.id();
    assert!(pid != 0);
    assert!(is_running(pid), "it should be up before we let go of it");

    // No kill, no close, no polite request: just letting go, which is what
    // being killed from outside amounts to.
    drop(process);

    for _ in 0..100 {
        if !is_running(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the browser outlived the job it was in");
}

/// Whether a process id still names a live process.
///
/// Asked through the job's own effect rather than by opening the process,
/// because a killed process keeps its id until every handle to it is closed and
/// `OpenProcess` would still succeed on that zombie. `tasklist` reads the live
/// list.
#[cfg(windows)]
fn is_running(pid: u32) -> bool {
    let mut tasklist = std::path::PathBuf::from(
        std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()),
    );
    tasklist.push(r"System32\tasklist.exe");
    let output = std::process::Command::new(tasklist)
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("tasklist runs");
    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}
