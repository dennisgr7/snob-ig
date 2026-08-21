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
//! real browser's store instead would mean going through the encryption the
//! operating system put around it — on Windows, App-Bound Encryption since
//! Chrome 127 — and that protection is there on purpose. There is no need to go
//! near it: a profile of our own answers the same question, with the user
//! signing in themselves and watching it happen, and that is the route taken.
//!
//! `Storage.getCookies` is the method that matters, because it returns
//! `HttpOnly` cookies too. `sessionid` is `HttpOnly`, which is also why no
//! console snippet can ever read it.
//!
//! **The protocol travels on a pipe, not on a port**, and that is the whole of
//! what [`crate::pipe`] is for. It used to be `--remote-debugging-port=0`, and
//! the cost of that was demonstrated rather than argued: a second local process
//! read the port out of `DevToolsActivePort`, called `/json/version` with no
//! credential, and got this session cookie back from `Storage.getCookies`.
//! Loopback carries no per-user access control, so that was every account on
//! the machine. Two anonymous pipes have no address, so there is nothing for a
//! second process to connect to.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use snob_ig::login::BrowserCookies;
use snob_ig::pace::CancelToken;
use snob_store::paths::AppPaths;

use crate::browser::Browser;
use crate::pipe::{BrowserProcess, PipeTransport};

/// How long to wait for the browser to answer its first command. Opening the
/// pipe is the first thing it does, so this only ever runs out when it did not
/// start.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for someone to finish logging in. Generous on purpose:
/// two-factor codes arrive by SMS and people go looking for their phone.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Gap between cookie checks.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Gap between looks at the pipe while waiting for the browser to come up.
///
/// Short, because the other thing this wait watches is the process: Chrome's
/// profile singleton exits within a second, and the whole point of noticing
/// that is not to sit out the startup timeout first.
const POLL_FOR_READY: Duration = Duration::from_millis(100);

/// How long a single protocol command may take.
///
/// Without this, a browser that keeps the socket open but stops answering
/// leaves the read below waiting forever, and neither the login deadline nor
/// Ctrl+C would ever be looked at again.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

const LOGIN_URL: &str = "https://www.instagram.com/accounts/login/";

/// A browser we started, and the pipe the protocol travels on.
///
/// Killed when dropped, so an early exit — a failed handshake, an error —
/// takes it down without having to remember to.
///
/// Dropping is not enough on its own for the three ways out that skip
/// destructors: the release profile aborts on panic, a second Ctrl+C exits the
/// process outright, and nothing at all runs when this process is killed from
/// outside. The first two go through [`kill_launched`]; the third is what the
/// job object in [`crate::pipe`] is for, and it is the only one of the three
/// that no code of ours can reach.
pub struct Launched {
    process: BrowserProcess,
    transport: PipeTransport,
    /// Only to name it in a message when the browser leaves early.
    profile: PathBuf,
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
    snob_store::paths::create_private_dir(&profile)
        .with_context(|| format!("could not create {}", profile.display()))?;
    let profile = profile.as_path();

    // A leftover from when this listened on a port. Removed rather than
    // ignored: a file named `DevToolsActivePort` sitting in snob's profile is
    // exactly the thing the reader of this code will go looking for to decide
    // whether a port is open, and finding a stale one would answer wrongly.
    let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));

    let arguments = vec![
        format!("--user-data-dir={}", profile.display()),
        // The protocol on two inherited pipes rather than on a loopback
        // socket. Nothing else on this machine can reach it, because there is
        // no address for it to reach.
        "--remote-debugging-pipe".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-features=Translate".to_string(),
        LOGIN_URL.to_string(),
    ];

    // Chrome narrates to standard error: GCM registration failures, a
    // TensorFlow notice. None of it is ours and all of it lands in the middle
    // of our own instructions, so `pipe::spawn` gives it nowhere to go.
    let (process, transport) = crate::pipe::spawn(&browser.path, &arguments)
        .with_context(|| format!("could not start {}", browser.name))?;

    LAUNCHED_PID.store(process.id(), std::sync::atomic::Ordering::Relaxed);
    kill_on_panic();

    if cancel.is_canceled() {
        kill_launched();
        bail!("canceled");
    }

    Ok(Launched {
        process,
        transport,
        profile: profile.to_path_buf(),
    })
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

/// An open DevTools connection, and the browser on the other end of it.
///
/// The two travel together because neither is useful alone: the connection is
/// what asks the browser to leave, and the browser is what has to be killed if
/// it will not.
pub struct Cdp {
    launched: Launched,
    next_id: u64,
}

impl Cdp {
    /// Takes a started browser and waits until it answers.
    ///
    /// There is no connecting to do any more — the pipe was opened before the
    /// browser existed — so what this waits for is the browser reaching the
    /// point of reading it. **The wait watches the process as well as the
    /// pipe**, and that is not tidiness: Chrome's profile singleton makes a
    /// second `snob login --browser` hand its command line to the instance
    /// already holding the profile and exit within a second, and watching only
    /// the pipe meant waiting the full thirty seconds and then blaming the
    /// debugging transport, which is the wrong problem.
    pub async fn connect(launched: Launched, cancel: &CancelToken) -> Result<Self> {
        let mut cdp = Self {
            launched,
            next_id: 1,
        };
        match cdp.wait_until_ready(cancel).await {
            Ok(()) => Ok(cdp),
            Err(e) => {
                // The browser is taken down here rather than left for whoever
                // forgets; `Launched` clears the pid on the way out.
                kill_launched();
                Err(e)
            }
        }
    }

    async fn wait_until_ready(&mut self, cancel: &CancelToken) -> Result<()> {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        let id = self.next_id;
        self.next_id += 1;
        self.send("Browser.getVersion", json!({}), id).await?;

        loop {
            if cancel.is_canceled() {
                bail!("canceled");
            }
            // The browser leaving is an answer too, and a faster one than the
            // deadline.
            if let Ok(Some(ended)) = self.launched.process.try_wait() {
                bail!("{}", died_early(ended.code, &self.launched.profile));
            }
            match tokio::time::timeout(POLL_FOR_READY, self.launched.transport.recv()).await {
                Ok(Some(message)) => {
                    if reply_to(id, &message).is_some() {
                        return Ok(());
                    }
                }
                // The pipe closed. The process check at the top of the next
                // turn is what says why, so this only has to not spin.
                Ok(None) => tokio::time::sleep(POLL_FOR_READY).await,
                Err(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "the browser did not answer its debugging pipe within {} seconds",
                    STARTUP_TIMEOUT.as_secs()
                );
            }
        }
    }

    /// Closes the browser politely, so the profile is not left looking like it
    /// crashed and offering to restore tabs on the next login.
    pub async fn close(mut self) {
        // `call` carries its own timeout, so a browser that has stopped
        // answering delays the exit rather than preventing it.
        let _ = self.call("Browser.close", json!({})).await;
        // It was asked to leave; this makes sure it did. `Launched` clears the
        // pid on the way out, so nothing here has to remember to.
        if self
            .launched
            .process
            .wait_up_to(Duration::from_secs(5))
            .await
            .is_none()
        {
            self.launched.process.kill();
        }
    }

    async fn send(&mut self, method: &str, params: Value, id: u64) -> Result<()> {
        let request = json!({ "id": id, "method": method, "params": params });
        self.launched
            .transport
            .send(request.to_string().into_bytes())
            .await
            .with_context(|| format!("could not send {method} to the browser"))
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
        self.send(method, params, id).await?;

        while let Some(message) = self.launched.transport.recv().await {
            let Some(reply) = reply_to(id, &message) else {
                continue;
            };
            if let Some(error) = reply.get("error") {
                bail!("the browser refused {method}: {error}");
            }
            return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
        }

        Err(anyhow!(
            "the browser closed the connection before answering {method}"
        ))
    }

    /// The browser's process id.
    ///
    /// Only so a test can ask the operating system what that process is
    /// listening on. Nothing in the program needs it: `kill_launched` reads the
    /// global, because it runs where this value cannot be reached.
    pub fn browser_pid(&self) -> u32 {
        self.launched.process.id()
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

/// One protocol message, if it is the reply to `id`.
///
/// Split out because two places need it, and because it is what stands between
/// a browser's chatter and a command's answer: events carry no `id`, and
/// replies to commands nobody is waiting for any more carry somebody else's.
/// A message that is not JSON at all is not a reply either — the transport
/// hands over bytes and says nothing about what is in them.
fn reply_to(id: u64, message: &[u8]) -> Option<Value> {
    let value: Value = serde_json::from_slice(message).ok()?;
    (value.get("id").and_then(Value::as_u64) == Some(id)).then_some(value)
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

    /// The reply to a command is told from everything else the browser says.
    ///
    /// Over a WebSocket this was a library's problem. Over a pipe it is ours,
    /// and it is the piece that decides whether a login reads the cookies or
    /// hangs: the browser emits events nobody subscribed to, and an event
    /// carries no `id` at all.
    #[test]
    fn only_the_reply_with_our_id_counts() {
        assert!(reply_to(7, br#"{"id":7,"result":{"ok":true}}"#).is_some());
        assert!(reply_to(7, br#"{"id":8,"result":{}}"#).is_none());
        assert!(reply_to(7, br#"{"method":"Target.targetCreated","params":{}}"#).is_none());
        assert!(reply_to(7, b"not json at all").is_none());
        assert!(reply_to(7, b"").is_none());
    }

    /// A refusal is carried back rather than read as an answer, and an id that
    /// arrived as a string is not our id.
    #[test]
    fn a_refusal_is_still_the_reply_it_answers() {
        let refused = reply_to(3, br#"{"id":3,"error":{"code":-32601}}"#).unwrap();
        assert!(refused.get("error").is_some());
        assert!(reply_to(3, br#"{"id":"3","result":{}}"#).is_none());
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
