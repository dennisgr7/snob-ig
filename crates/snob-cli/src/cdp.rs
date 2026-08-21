//! As much of the Chrome DevTools Protocol as capturing a login needs.
//!
//! **Nothing here touches the user's own browser, or the cookie store that
//! belongs to it.** What this drives is a browser *this program started*,
//! pointed at a profile directory under snob's own data directory — empty until
//! the user logs into Instagram themselves, in the window that opens in front
//! of them. The cookie then comes back from that browser, through the browser's
//! own debugging protocol, and describes a session the user created a moment
//! earlier.
//!
//! That boundary is deliberate, and it is where the project stops. Reading the
//! real browser's store instead would mean defeating the encryption the
//! operating system put around it — on Windows, App-Bound Encryption since
//! Chrome 127 — which is the business credential-stealing malware is in. There
//! is no need to go anywhere near it: a profile of our own answers the same
//! question, with the user's knowledge, and that is the route taken.
//!
//! `Storage.getCookies` is the method that matters, because it returns
//! `HttpOnly` cookies too. `sessionid` is `HttpOnly`, which is also why no
//! console snippet can ever read it.
//!
//! What this costs, stated plainly: while the login is in progress the browser
//! is listening on a loopback port, and that port has no authentication —
//! anything else running on the machine can ask it for the same cookies. The
//! port number is random and the window closes as soon as the session is
//! captured, but on a machine shared with people you do not trust, `--paste`
//! is the safer way in.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use snob_core::paths::AppPaths;
use snob_ig::login::BrowserCookies;
use snob_ig::pace::CancelToken;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::browser::Browser;

/// How long to wait for the browser to write its debugging endpoint. It is the
/// first thing it does, so this only ever runs out when it did not start.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for someone to finish logging in. Generous on purpose:
/// two-factor codes arrive by SMS and people go looking for their phone.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Gap between cookie checks.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long a single protocol command may take.
///
/// Without this, a browser that keeps the socket open but stops answering
/// leaves the read below waiting forever, and neither the login deadline nor
/// Ctrl+C would ever be looked at again.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

const LOGIN_URL: &str = "https://www.instagram.com/accounts/login/";

/// A browser we started. Killed when dropped, so an early exit — a failed
/// connection, an error — takes it down without having to remember to.
///
/// Dropping is not enough on its own for the two ways out that skip
/// destructors: the release profile aborts on panic, and a second Ctrl+C exits
/// the process outright. Those go through [`kill_launched`].
pub struct Launched {
    child: tokio::process::Child,
    endpoint: String,
}

/// Forgets the pid when the browser goes.
///
/// A pid that outlives its process names whatever the operating system hands
/// that number to next, and `kill_launched` would then shoot at a stranger.
impl Drop for Launched {
    fn drop(&mut self) {
        LAUNCHED_PID.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The browser this process started, if it is still running.
///
/// A global because the paths that need it — the signal handler — have no way
/// to reach the value. Zero means there is nothing to kill.
static LAUNCHED_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Makes a panic take the browser with it.
///
/// The release profile is `panic = "abort"`, so no destructor runs on the way
/// out — `kill_on_drop` included. Without this, a panic anywhere in the ten
/// minutes `wait_for_login` may sit there leaves a browser running with an
/// unauthenticated debugging port and a live Instagram session behind it, until
/// somebody notices the window. That port is the one thing in this program that
/// hands out the credential to whoever asks.
///
/// Installed at launch rather than at startup so that a run which never opens a
/// browser keeps the default panic behavior untouched.
fn kill_on_panic() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            kill_launched();
            previous(info);
        }));
    });
}

/// Kills the browser without waiting for anything, for use on the way out of a
/// process that is not going to run any destructors.
///
/// Leaving it alive would leave its debugging port open with a logged-in
/// session behind it, for as long as the window stays up.
pub fn kill_launched() {
    let pid = LAUNCHED_PID.swap(0, std::sync::atomic::Ordering::Relaxed);
    if pid == 0 {
        return;
    }

    // No tokio here: this runs from a signal handler on its way to exit.
    //
    // **Both helpers are named by absolute path.** A bare name is resolved by
    // search, and on Windows the first place searched is the *calling
    // executable's own directory* -- not the working directory, not `PATH`.
    // That was checked: a planted `taskkill.exe` beside `snob.exe` wins. This
    // function runs from the panic hook and from the second Ctrl+C, which is
    // exactly the moment a browser is up with a live session behind an open
    // debugging port, and it is reachable by anybody who can write next to the
    // binary -- a `snob.exe` run out of a downloads folder, a share, a USB
    // stick.
    #[cfg(windows)]
    {
        let mut taskkill = std::path::PathBuf::from(
            std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()),
        );
        taskkill.push(r"System32\taskkill.exe");
        let _ = std::process::Command::new(taskkill)
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[cfg(unix)]
    let _ = std::process::Command::new("/bin/kill")
        .args(["-9", &pid.to_string()])
        .status();
}

/// Starts the browser against our own profile with debugging enabled.
pub async fn launch(browser: &Browser, paths: &AppPaths, cancel: &CancelToken) -> Result<Launched> {
    // The profile ends up holding a live Instagram session, so the directory it
    // sits in has to be the owner's alone. On a fresh install with the keyring
    // backend nothing has created the data directory yet, and a bare
    // `create_dir_all` would leave it at whatever the umask says.
    paths.ensure_dirs()?;
    let profile = paths.browser_profile();
    snob_core::paths::create_private_dir(&profile)
        .with_context(|| format!("could not create {}", profile.display()))?;
    let profile = profile.as_path();

    // Written by the browser at startup. A stale one from a previous run would
    // be read as this run's endpoint and point at a port nobody is listening on.
    let active_port = profile.join("DevToolsActivePort");
    let _ = std::fs::remove_file(&active_port);

    let mut child = tokio::process::Command::new(&browser.path)
        .arg(format!("--user-data-dir={}", profile.display()))
        // Port 0 means "pick a free one and write it down", which avoids both
        // colliding with something already on 9222 and handing a fixed port to
        // anything else on this machine.
        .arg("--remote-debugging-port=0")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-features=Translate")
        .arg(LOGIN_URL)
        // Chrome narrates to stderr: the debugging endpoint, GCM registration
        // failures, a TensorFlow notice. None of it is ours and all of it lands
        // in the middle of our own instructions.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("could not start {}", browser.name))?;

    LAUNCHED_PID.store(
        child.id().unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );
    kill_on_panic();

    // From here on the pid must not outlive the process it names: a stale one
    // would make a later `kill_launched` shoot at whatever has since been given
    // that number. Every way out of this function clears it except the one that
    // hands the browser over to `Launched`.
    let endpoint = match wait_for_endpoint(&active_port, &mut child, profile, cancel).await {
        Ok(endpoint) => endpoint,
        Err(e) => {
            kill_launched();
            return Err(e);
        }
    };
    Ok(Launched { child, endpoint })
}

/// Both waits below are the same shape, and every one of their endings matters:
/// found, the browser exited, gave up, ran out of time. They are written out
/// rather than shared because an async closure holding the connection across
/// the await does not survive the borrow checker, and eight duplicated lines
/// are a better price than the contortion that would.
///
/// Reads the endpoint the browser wrote: the port on the first line and the
/// path to the browser-level target on the second.
async fn wait_for_endpoint(
    active_port: &Path,
    child: &mut tokio::process::Child,
    profile: &Path,
    cancel: &CancelToken,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;

    loop {
        if cancel.is_canceled() {
            bail!("canceled");
        }
        // The file first, then the child: a browser that wrote its endpoint and
        // then exited still handed us a usable one.
        if let Ok(text) = std::fs::read_to_string(active_port)
            && let Some((port, path)) = parse_endpoint(&text)
        {
            return Ok(format!("ws://127.0.0.1:{port}{path}"));
        }
        // Chrome's profile singleton makes this ordinary rather than exotic: a
        // second `snob login --browser` hands its command line to the instance
        // already holding the profile and exits within a second. Watching only
        // the port file meant waiting the full thirty seconds and then blaming
        // the debugging port, which is the wrong problem — and holding a dead
        // pid the whole time, which is what the comment above `LAUNCHED_PID`
        // warns about.
        if let Ok(Some(status)) = child.try_wait() {
            // `try_wait` is what reaps the child, so this is the moment the
            // number becomes reusable. Clearing it here also makes the
            // `kill_launched()` in the caller's error arm a deliberate no-op
            // rather than a shot at a stranger.
            LAUNCHED_PID.store(0, std::sync::atomic::Ordering::Relaxed);
            bail!("{}", died_early(status.code(), profile));
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "the browser did not open its debugging port within {} seconds",
                STARTUP_TIMEOUT.as_secs()
            );
        }
        if cancel.sleep_or_cancel(Duration::from_millis(100)).await {
            bail!("canceled");
        }
    }
}

/// What to say when the browser started and stopped again.
///
/// Split out so the wording can be read and changed without a browser: nothing
/// about the branch above is testable without launching one, and `ExitStatus`
/// cannot be constructed portably in a test anyway, so this takes the code.
fn died_early(code: Option<i32>, profile: &Path) -> String {
    match code {
        // Exiting cleanly and immediately is the singleton: the browser handed
        // its command line to the instance that already has this profile open.
        Some(0) => format!(
            "the browser closed straight away, which means one is already open on snob's \
             profile at {}.\n\
             Close that window and try again, or use \"snob login --paste\".",
            profile.display()
        ),
        // No code at all means something killed it — a signal on Unix, an
        // external terminate on Windows. Reporting that as the singleton sent
        // people hunting for a window that was never open, and the profile path
        // in the message made the wrong story convincing.
        None => "the browser was killed before it opened its debugging port.\n\
                 Try again, or use \"snob login --paste\"."
            .to_string(),
        Some(code) => format!(
            "the browser exited with code {code} instead of starting.\n\
             Try \"snob login --paste\" instead."
        ),
    }
}

/// The file holds the port and the target path on two lines. It is written in
/// two steps, so a half-written one has to read as "not ready" rather than as
/// an endpoint.
fn parse_endpoint(text: &str) -> Option<(u16, String)> {
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    if port == 0 || !path.starts_with('/') {
        return None;
    }
    Some((port, path.to_string()))
}

/// An open DevTools connection, and the browser on the other end of it.
///
/// The two travel together because neither is useful alone: the connection is
/// what asks the browser to leave, and the browser is what has to be killed if
/// it will not.
pub struct Cdp {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    launched: Launched,
    next_id: u64,
}

impl Cdp {
    pub async fn connect(launched: Launched) -> Result<Self> {
        let (socket, _) = tokio_tungstenite::connect_async(&launched.endpoint)
            .await
            .context("could not connect to the browser's debugging port")?;
        Ok(Self {
            socket,
            launched,
            next_id: 1,
        })
    }

    /// Closes the browser politely, so the profile is not left looking like it
    /// crashed and offering to restore tabs on the next login.
    pub async fn close(mut self) {
        // `call` carries its own timeout, so a browser that has stopped
        // answering delays the exit rather than preventing it.
        let _ = self.call("Browser.close", json!({})).await;
        // It was asked to leave; this makes sure it did. `Launched` clears the
        // pid on the way out, so nothing here has to remember to.
        let child = &mut self.launched.child;
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        let _ = child.start_kill();
    }

    /// Sends one command and waits for the answer with its id.
    ///
    /// The protocol interleaves events with replies, so anything that is not
    /// the reply being waited for is dropped: this subscribes to no events, and
    /// the browser emits some regardless.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        tokio::time::timeout(CALL_TIMEOUT, self.call_forever(method, params))
            .await
            .map_err(|_| anyhow!("the browser stopped answering ({method})"))?
    }

    async fn call_forever(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let request = json!({ "id": id, "method": method, "params": params });
        self.socket
            .send(Message::Text(request.to_string().into()))
            .await
            .with_context(|| format!("could not send {method} to the browser"))?;

        while let Some(message) = self.socket.next().await {
            let message = message.context("the connection to the browser broke")?;
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                bail!("the browser refused {method}: {error}");
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }

        Err(anyhow!(
            "the browser closed the connection before answering {method}"
        ))
    }

    /// The exact User-Agent this browser sends.
    ///
    /// Worth asking for rather than reconstructing: the session is tied to it,
    /// and a User-Agent that does not match the browser that created the cookie
    /// is what makes Instagram answer `useragent mismatch`.
    pub async fn user_agent(&mut self) -> Result<String> {
        let result = self.call("Browser.getVersion", json!({})).await?;
        result
            .get("userAgent")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("the browser did not report its User-Agent"))
    }

    /// The Instagram session currently in this browser, if there is one.
    pub async fn instagram_cookies(&mut self) -> Result<Option<BrowserCookies>> {
        let result = self.call("Storage.getCookies", json!({})).await?;
        let cookies = result
            .get("cookies")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok(collect(cookies))
    }
}

/// Picks the Instagram cookies out of everything the browser holds.
///
/// Returns `None` until `sessionid` is there, which is what "logged in" means:
/// the other cookies show up as soon as the login page loads.
fn collect(cookies: &[Value]) -> Option<BrowserCookies> {
    let mut found = BrowserCookies::default();

    for cookie in cookies {
        let domain = cookie.get("domain").and_then(Value::as_str).unwrap_or("");
        if !(domain == "instagram.com" || domain.ends_with(".instagram.com")) {
            continue;
        }
        let (Some(name), Some(value)) = (
            cookie.get("name").and_then(Value::as_str),
            cookie.get("value").and_then(Value::as_str),
        ) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }

        match name {
            "sessionid" => found.sessionid = value.into(),
            "ds_user_id" => found.ds_user_id = Some(value.to_string()),
            "csrftoken" => found.csrftoken = Some(value.into()),
            "mid" => found.mid = Some(value.to_string()),
            "ig_did" => found.ig_did = Some(value.to_string()),
            _ => {}
        }
    }

    (!found.sessionid.is_empty()).then_some(found)
}

/// Waits for the login to happen, checking every couple of seconds.
pub async fn wait_for_login(cdp: &mut Cdp, cancel: &CancelToken) -> Result<BrowserCookies> {
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;

    loop {
        if cancel.is_canceled() {
            bail!("canceled");
        }
        if let Some(cookies) = cdp.instagram_cookies().await? {
            return Ok(cookies);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "no login happened within {} minutes",
                LOGIN_TIMEOUT.as_secs() / 60
            );
        }
        if cancel.sleep_or_cancel(POLL_INTERVAL).await {
            bail!("canceled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second `snob login --browser` hands its command line to the instance
    /// already holding the profile and exits within a second. Waiting the full
    /// thirty seconds and then talking about the debugging port described the
    /// wrong problem entirely.
    #[test]
    fn a_browser_that_exited_cleanly_names_the_profile() {
        let profile = Path::new("C:/somewhere/browser-profile");
        let said = died_early(Some(0), profile);
        assert!(said.contains("already open"), "{said}");
        assert!(said.contains("browser-profile"), "{said}");
        assert!(said.contains("--paste"), "{said}");
        assert!(
            !said.contains("did not open"),
            "that is the startup timeout, a different failure: {said}"
        );
    }

    /// A browser that failed to start is a different thing, and says so.
    #[test]
    fn a_browser_that_failed_reports_its_code() {
        let said = died_early(Some(127), Path::new("/tmp/p"));
        assert!(said.contains("127"), "{said}");
        assert!(!said.contains("already open"), "{said}");
    }

    #[test]
    fn it_reads_the_two_line_endpoint() {
        let (port, path) = parse_endpoint("54321\n/devtools/browser/abc-123\n").unwrap();
        assert_eq!(port, 54321);
        assert_eq!(path, "/devtools/browser/abc-123");
    }

    /// The file is written in two steps. Catching it half-written must read as
    /// "not ready yet", not as an endpoint on a port nobody is listening on.
    #[test]
    fn a_half_written_file_is_not_an_endpoint() {
        assert!(parse_endpoint("54321").is_none());
        assert!(parse_endpoint("").is_none());
        assert!(parse_endpoint("54321\n").is_none());
        assert!(parse_endpoint("0\n/devtools/browser/abc").is_none());
        assert!(parse_endpoint("not-a-port\n/devtools/browser/abc").is_none());
        assert!(parse_endpoint("54321\ndevtools-without-a-slash").is_none());
    }

    fn cookie(name: &str, value: &str, domain: &str) -> Value {
        json!({ "name": name, "value": value, "domain": domain })
    }

    #[test]
    fn it_collects_the_session_once_it_appears() {
        let cookies = vec![
            cookie("sessionid", "42%3AAbCd%3A20", ".instagram.com"),
            cookie("ds_user_id", "42", ".instagram.com"),
            cookie("csrftoken", "tok", ".instagram.com"),
            cookie("mid", "m", "instagram.com"),
            cookie("ig_did", "d", ".instagram.com"),
        ];

        let found = collect(&cookies).unwrap();
        assert_eq!(found.sessionid.expose(), "42%3AAbCd%3A20");
        assert_eq!(found.ds_user_id.as_deref(), Some("42"));
        assert_eq!(
            found
                .csrftoken
                .as_ref()
                .map(snob_core::secret::Secret::expose),
            Some("tok")
        );
        assert_eq!(found.ig_did.as_deref(), Some("d"));
    }

    /// Everything but `sessionid` is there from the moment the login page
    /// loads, so only `sessionid` means the login actually happened.
    #[test]
    fn before_the_login_there_is_no_session_yet() {
        let cookies = vec![
            cookie("csrftoken", "tok", ".instagram.com"),
            cookie("mid", "m", ".instagram.com"),
        ];
        assert!(collect(&cookies).is_none());
    }

    #[test]
    fn an_empty_session_cookie_does_not_count_as_a_login() {
        assert!(collect(&[cookie("sessionid", "", ".instagram.com")]).is_none());
    }

    /// Only Instagram's own cookies. A look-alike domain must not be read, and
    /// no other site's cookies are of any interest.
    #[test]
    fn other_sites_are_left_alone() {
        let cookies = vec![
            cookie("sessionid", "someone-elses", "notinstagram.com"),
            cookie("sessionid", "also-not", "instagram.com.example.net"),
            cookie("sessionid", "nope", "example.com"),
        ];
        assert!(collect(&cookies).is_none());

        let real = vec![cookie("sessionid", "mine", "www.instagram.com")];
        assert_eq!(collect(&real).unwrap().sessionid.expose(), "mine");
    }

    /// Three different things happen when a launched browser is not there any
    /// more, and they were reported as two.
    ///
    /// A clean immediate exit is Chrome's profile singleton: the second launch
    /// handed its command line to the instance already holding the profile.
    /// **No code at all is not that** — it means something killed the process —
    /// and it used to share the singleton's sentence, so the message named a
    /// profile and a window to close that had never been open.
    ///
    /// The wording is split out precisely so it can be read without launching a
    /// browser: `ExitStatus` cannot be built portably in a test, so this takes
    /// the code instead.
    #[test]
    fn what_killed_the_browser_decides_what_is_said() {
        let profile = std::path::Path::new("/tmp/snob-profile");

        let killed = died_early(None, profile);
        assert!(killed.contains("killed"), "{killed}");
        assert!(
            !killed.contains("already open"),
            "there is no window to close: {killed}"
        );

        let failed = died_early(Some(3), profile);
        assert!(failed.contains("code 3"), "{failed}");
    }

    /// Every line of these has to start where the terminal puts it. The two
    /// literals carried the source's own newline and indentation inside the
    /// string, so the message came out with a fourteen-space gap in the middle
    /// of a sentence and a thirteen-space hanging indent -- which
    /// `report::indented` then widened by seven more.
    #[test]
    fn the_browser_messages_have_no_source_indentation_in_them() {
        let profile = std::path::Path::new("/tmp/snob-profile");
        for message in [
            died_early(Some(0), profile),
            died_early(None, profile),
            died_early(Some(3), profile),
        ] {
            assert!(
                !message.contains("  "),
                "a run of spaces survived: {message:?}"
            );
            for line in message.lines() {
                assert!(!line.starts_with(' '), "a line is indented: {line:?}");
            }
        }
    }
}
