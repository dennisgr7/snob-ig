//! The browser snob sends its requests from.
//!
//! **What changed and why.** Every request to Instagram used to leave through
//! `reqwest`: this process's TLS handshake and HTTP/2 settings, the cookies the
//! login handed over and never updated, and headers computed to agree with a
//! Chrome this process is not. A session a browser created, used by something
//! that is visibly not that browser, is the textbook stolen-session signature,
//! and this project's users started getting Instagram's automated-activity
//! warning. So the requests now come from the browser itself: the Chrome, Edge
//! or Chromium installed on the machine, running without a window against
//! snob's own profile — the same profile the login happens in — and sending
//! each request with `fetch()` from an open instagram.com tab.
//!
//! The browser is started on the first request a run makes, shared by every
//! client in the process (two browsers cannot share a profile), and closed at
//! the end of the run by [`shutdown`]. Nothing that spends no network starts
//! it.
//!
//! **What a headless Chrome gives away, and what is done about each.** The
//! first four were measured against Chromium 141, the rest against Chromium
//! 153 on Linux ARM64, in September 2026, each from a script on a page and a
//! service worker the way a site would ask:
//!
//! - `navigator.webdriver` is `true` under the debugging pipe, headless or
//!   not. `--disable-blink-features=AutomationControlled`, in `cdp.rs`.
//! - The User-Agent says `HeadlessChrome`. `--user-agent` with the launched
//!   browser's own; it is what covers the few requests that belong to no
//!   target, a service worker's script among them.
//! - That flag blanks the high-entropy client hints — architecture, bitness,
//!   platform version, full version list — which is worse than the name it
//!   hides. They are asked once of a browser started without it, and kept
//!   ([`machine_hints`]); the brands are the ones the browser reports for
//!   itself, so a Chromium does not claim to be Google Chrome.
//! - `setUserAgentOverride` holds for one target, and a worker or the
//!   service worker a site registers is a target of its own: the service
//!   worker called itself `HeadlessChrome`. Every target is paused as it
//!   attaches and handed the same override (`cdp::OnAttach`). Its requests
//!   carry no `Sec-CH-UA` either way — neither does a windowed Chromium's,
//!   measured under Xvfb, so that is Chromium and not a tell.
//! - `document.hasFocus()` was `false`: focus is emulated.
//! - The screen was all work area, `availHeight` equal to `height`, and the
//!   window ran off it from (10,10): a taskbar's strip is kept, and the window
//!   fills the rest from the corner.
//! - `(pointer: fine)` and `(hover: hover)` were both false, a phone's answer:
//!   a mouse is declared through `--blink-settings`.
//! - The requests ran in the page's main world, where a `fetch` the site
//!   wrapped and its resource timing list both see them; they run in an
//!   isolated world ([`tab::isolated_world`]).
//!
//! And one thing that is not about looking like a browser but about what a
//! browser does to other people: the feed plays videos on its own, and a play
//! counts. No video reaches the page ([`refuse_video`]).
//!
//! What is left is left knowingly: WebGL is absent — `getContext` returns
//! nothing without a GPU the browser will use headless — and the switch that
//! brings in the software renderer is one Chromium itself calls unsafe, and
//! would only name SwiftShader instead. Whether a headless browser on a
//! Windows desktop reaches the real GPU is to be measured there first.
//! `navigator.languages` and `Accept-Language` are the profile's own, which is
//! what the login sent.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use snob_core::Pk;
use snob_core::session::Session;
use snob_ig::client::page::{Page, PageError, PageFactory, PageFuture, PageRequest, PageResponse};
use snob_ig::pace::CancelToken;
use snob_store::paths::AppPaths;

use crate::cdp::Cdp;

mod identity;
mod profile;
mod tab;

use identity::{machine_hints, metadata};
use profile::sync_cookies;
pub use profile::{MachineHints, ProfileMark};
use tab::{fetch, navigate, navigate_and_read};

/// How long a navigation may take to finish loading.
const LOAD_TIMEOUT: Duration = Duration::from_secs(45);

/// How long after a page finishes loading before the first request is sent
/// from it: the app bootstraps after `load`, and a request from a page that
/// has not started its own is not what a person's first click looks like.
const SETTLE: Duration = Duration::from_millis(1_500);

/// How long a single browser command may take when it is not a fetch.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// The most a page may hand back, measured the way it travels: as a protocol
/// message on the pipe, where every character outside printable ASCII is six
/// bytes (`\uXXXX`) and a quote or backslash two. Two megabytes short of the
/// pipe's ceiling, for the envelope. A message over that ceiling is dropped on
/// the way in and fails its request as a browser failure, so the cap is
/// applied in the page, before anything is sent back: an answer too large then
/// arrives as `too_large`, which the client reads as what it is.
const PAGE_WIRE_CAP: u64 = (crate::pipe::MAX_MESSAGE_BYTES - 2 * 1024 * 1024) as u64;

/// The screen the browser says it is on: the commonest desktop size.
const SCREEN: (u32, u32) = (1920, 1080);

/// The strip at the bottom of that screen a taskbar keeps for itself.
///
/// A headless screen is all work area, so `screen.availHeight` equalled
/// `screen.height` — measured, 1080 of 1080 — which no desktop with a taskbar
/// or a dock reports. The window fills what is left, from the corner, rather
/// than sitting at (10,10) and running off the bottom edge.
const TASKBAR: u32 = 48;

/// The one browser of this process, once something has asked for it.
static HEADLESS: OnceLock<Arc<Headless>> = OnceLock::new();

/// Sends every request of every client built from now on from the browser.
///
/// Called once from `main`, before any client exists.
pub fn install(paths: &AppPaths) {
    let headless = HEADLESS
        .get_or_init(|| Arc::new(Headless::new(paths.clone())))
        .clone();
    let factory: PageFactory = Arc::new(move |session: &Session| {
        headless.want(session);
        Arc::clone(&headless) as Arc<dyn Page>
    });
    let _ = snob_ig::client::page::send_every_request_from(factory);
}

/// Closes the browser, if one was started, the polite way: a browser that is
/// killed can lose the cookies it rotated in the last half minute.
pub async fn shutdown() {
    if let Some(headless) = HEADLESS.get() {
        headless.close().await;
    }
}

/// The browser, started on first use.
pub struct Headless {
    paths: AppPaths,
    /// The session whose cookies the browser has to be carrying.
    wanted: std::sync::Mutex<Option<Session>>,
    /// One request at a time: a tab is not a connection pool, and the pacing
    /// never asks for two at once anyway.
    state: tokio::sync::Mutex<Option<Live>>,
}

struct Live {
    cdp: Cdp,
    /// The DevTools session of the tab the requests are sent from.
    tab: String,
    /// The origin the tab is on. `about:blank` until the first request.
    origin: String,
    /// The isolated world the requests are sent from, in the document the tab
    /// is on now; `None` until one is made. See [`tab::isolated_world`].
    world: Option<i64>,
    /// The account whose cookies have been checked in the browser.
    synced: Option<Pk>,
    /// What is known about the profile; see [`ProfileMark`].
    mark: ProfileMark,
}

impl Headless {
    fn new(paths: AppPaths) -> Self {
        Self {
            paths,
            wanted: std::sync::Mutex::new(None),
            state: tokio::sync::Mutex::new(None),
        }
    }

    fn want(&self, session: &Session) {
        *self.wanted.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.clone());
    }

    fn wanted(&self) -> Result<Session> {
        self.wanted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| anyhow!("no session to send requests as"))
    }

    async fn close(&self) {
        if let Some(live) = self.state.lock().await.take() {
            live.cdp.close().await;
        }
    }

    async fn send_inner(&self, request: PageRequest) -> Result<PageResponse, PageError> {
        let mut state = self.state.lock().await;
        if state.is_none() {
            *state = Some(
                self.start()
                    .await
                    .map_err(|e| PageError::Browser(format!("{e:#}")))?,
            );
        }
        let live = state.as_mut().expect("started just above");

        let result = self.send_on(live, &request).await;
        if let Err(PageError::Browser(_)) = &result {
            // A browser that failed once is not trusted with the next request:
            // it may be gone. The next request starts a fresh one. Only then —
            // a request the network dropped says nothing about the browser,
            // and relaunching it would load the site again for nothing.
            if let Some(live) = state.take() {
                live.cdp.close().await;
            }
        }
        result
    }

    async fn send_on(
        &self,
        live: &mut Live,
        request: &PageRequest,
    ) -> Result<PageResponse, PageError> {
        let broken = |e: anyhow::Error| PageError::Browser(format!("{e:#}"));
        let origin = origin_of(&request.url).map_err(broken)?;
        let session = self.wanted().map_err(broken)?;
        if live.synced != Some(session.ds_user_id) {
            let mut mark = live.mark.clone();
            sync_cookies(live, &session, &origin, &mut mark)
                .await
                .map_err(broken)?;
            if mark != live.mark {
                mark.write(&self.paths);
                live.mark = mark;
            }
            live.synced = Some(session.ds_user_id);
            // A tab already on the site loaded as whoever was there before.
            live.origin = String::new();
        }
        if live.origin != origin {
            navigate(live, &format!("{origin}/")).await?;
            live.origin = origin;
        }
        if request.navigate {
            return navigate_and_read(live, &request.url).await;
        }
        fetch(live, request).await
    }

    /// Starts the browser. A second snob already running one on this profile
    /// makes Chrome hand over to it and exit, which `cdp::connect` recognizes
    /// and says in so many words.
    async fn start(&self) -> Result<Live> {
        let session = self.wanted()?;
        let mut mark = ProfileMark::read(&self.paths);
        // The browser that made the profile, while it is still installed; then
        // the one the session names; then the first found. See `ProfileMark`
        // for why the first answer is the one that matters.
        let made_it = mark.browser.as_deref().and_then(|path| {
            crate::browser::detect_all()
                .into_iter()
                .find(|b| b.path == path)
        });
        let browser = made_it
            .or_else(|| {
                session
                    .browser
                    .as_deref()
                    .and_then(crate::browser::detect_named)
            })
            .or_else(crate::browser::detect)
            .ok_or_else(|| {
                anyhow!(
                    "snob sends its requests from a browser, and no Chrome, Edge, Brave or \
                     Chromium was found on this machine.\n\
                     Install one (Chromium is enough), or set SNOB_NO_BROWSER=1 to send \
                     them directly — which Instagram can tell apart from a browser."
                )
            })?;
        refuse_root()?;
        // A profile nothing has claimed yet, or whose browser is gone: from
        // here on it is this one's. Written once the launch has created the
        // directory it lives in.
        let claim = mark.browser.as_deref() != Some(browser.path.as_path());
        mark.browser = Some(browser.path.clone());
        // The User-Agent of the binary being started, not the one stored with
        // the session. The brands below are what this binary reports, and a
        // stored string that has fallen behind — refreshed once a day at
        // most, and never for a session that came from elsewhere — would put
        // one major version in the User-Agent and another in every client
        // hint beside it. A User-Agent the person pinned at login is theirs
        // to keep.
        let user_agent = if session.user_agent_pinned {
            session.user_agent.replace("HeadlessChrome/", "Chrome/")
        } else {
            browser.user_agent()
        };

        // What this browser says about the machine, asked of it once without
        // `--user-agent` (which blanks it) and kept. See `machine_hints`.
        let hints = machine_hints(&browser, &self.paths, &mut mark).await;

        let flags = vec![
            "--headless=new".to_string(),
            // Kept although every target is overridden below: a few requests
            // belong to no target — the script of a service worker is one — and
            // without it they would name `HeadlessChrome`.
            format!("--user-agent={user_agent}"),
            format!(
                "--screen-info={{0,0 {}x{} workAreaBottom={TASKBAR}}}",
                SCREEN.0, SCREEN.1
            ),
            "--window-position=0,0".to_string(),
            format!("--window-size={},{}", SCREEN.0, SCREEN.1 - TASKBAR),
            // A headless browser has no pointer: `(pointer: fine)` and
            // `(hover: hover)` were both false, which is a phone's answer on a
            // desktop's screen. A mouse is what the machine this claims to be
            // has.
            "--blink-settings=primaryPointerType=4,availablePointerTypes=4,\
             primaryHoverType=2,availableHoverTypes=2"
                .to_string(),
        ];
        let cancel = CancelToken::default();
        let launched = crate::cdp::launch_headless(&browser, &self.paths, &flags, &cancel)
            .await
            .with_context(|| format!("could not start {} without a window", browser.name))?;
        let cdp = Cdp::connect(launched, &cancel).await?;
        if claim || hints.fresh {
            mark.write(&self.paths);
        }

        let version = cdp.browser_call("Browser.getVersion", json!({})).await?;
        let full_version = version
            .get("product")
            .and_then(Value::as_str)
            .and_then(|product| product.split('/').nth(1))
            .unwrap_or_default()
            .to_string();

        // **Every target, not only the tab.** A worker, a frame and the
        // service worker the site registers are targets of their own, and
        // `setUserAgentOverride` holds for the one it was sent to; measured,
        // a service worker's requests carried no client hints at all. With
        // auto-attach each one is paused before it runs and handed the same
        // override (`cdp::OnAttach`). The language is left alone everywhere:
        // the profile's own, which is what the login sent.
        let metadata = metadata(&user_agent, &full_version, hints.values.as_ref());
        let identity = json!({ "userAgent": user_agent, "userAgentMetadata": metadata });
        let page_commands = vec![
            ("Emulation.setUserAgentOverride", identity.clone()),
            // A headless tab never has focus, and `document.hasFocus()` said
            // so — measured false — where a page somebody is looking at says
            // true.
            (
                "Emulation.setFocusEmulationEnabled",
                json!({ "enabled": true }),
            ),
            ("Fetch.enable", refuse_video()),
        ];
        cdp.on_attach(crate::cdp::OnAttach {
            page: page_commands.clone(),
            worker: vec![
                ("Network.setUserAgentOverride", identity),
                ("Fetch.enable", refuse_video()),
            ],
        });
        cdp.browser_call(
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true,
                // The service and shared workers, which belong to the browser
                // rather than to a tab. The tab's own are asked for on it.
                "filter": [
                    { "type": "service_worker", "exclude": false },
                    { "type": "shared_worker", "exclude": false },
                    { "exclude": true },
                ],
            }),
        )
        .await?;

        let tab = attach_to_a_tab(&cdp).await?;
        for (method, params) in page_commands {
            cdp.page_call(&tab, method, params, COMMAND_TIMEOUT).await?;
        }
        cdp.page_call(
            &tab,
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
            COMMAND_TIMEOUT,
        )
        .await?;
        // So that a tab that crashes says so, and the request waiting on it
        // fails at once rather than at its timeout: `Inspector.targetCrashed`
        // is only sent to a session that asked for the domain. Nothing about
        // it reaches the page.
        cdp.page_call(&tab, "Inspector.enable", json!({}), COMMAND_TIMEOUT)
            .await?;

        Ok(Live {
            cdp,
            tab,
            origin: "about:blank".to_string(),
            world: None,
            synced: None,
            mark,
        })
    }
}

/// The requests the browser pauses for this connection to refuse: every video
/// the site would load, feed and reels and stories alike.
///
/// **So that nothing plays.** Opening the site renders the feed, and the feed
/// plays a video that scrolls into view, muted, on its own; Instagram counts a
/// play of a reel from its start. A run of snob would add plays to other
/// people's videos that nobody watched. Measured on Chromium 153, a muted video
/// plays under every `--autoplay-policy` there is, the strictest included —
/// those only hold back sound — and a minimized window freezes the page, the
/// calls with it. What holds is the video never arriving: refused as a content
/// blocker refuses it (`Cdp::refuse_paused`), it fails to load, and a page gets
/// that from the blockers a great many people run.
///
/// By address rather than by kind: the site's player fetches its video in
/// pieces with `fetch()`, which the browser files as a fetch, not as media.
/// Every piece is an `.mp4` on the CDN; nothing snob sends has that in its
/// address, and snob's own downloads of stories do not go through the browser.
fn refuse_video() -> Value {
    json!({
        "patterns": [
            { "urlPattern": "*.mp4*" },
            { "urlPattern": "*.webm*" },
            { "urlPattern": "*.m3u8*" },
            { "resourceType": "Media" },
        ],
    })
}

/// Chromium will not run as root with its sandbox on, and snob does not turn
/// the sandbox off for anybody: this browser loads pages and media a server
/// chose, which is what the sandbox is for. Measured: it exits 1 at once, and
/// "exited with code 1" says nothing about why. Root is the ordinary user in
/// a container, which is where this is most likely to be met.
fn refuse_root() -> Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` takes nothing, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            bail!(
                "snob sends its requests from a browser, and Chromium will not run as root \
                 with its sandbox on.\n\
                 Run snob as an ordinary user, or set SNOB_NO_BROWSER=1 to send the requests \
                 directly — which Instagram can tell apart from a browser."
            );
        }
    }
    Ok(())
}

impl Page for Headless {
    fn send(&self, request: PageRequest) -> PageFuture<'_> {
        Box::pin(self.send_inner(request))
    }
}

/// Attaches to the tab the browser opened with, in flat mode so its commands
/// travel on the same pipe.
async fn attach_to_a_tab(cdp: &Cdp) -> Result<String> {
    let targets = cdp.browser_call("Target.getTargets", json!({})).await?;
    let page = targets
        .get("targetInfos")
        .and_then(Value::as_array)
        .and_then(|all| {
            all.iter()
                .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        })
        .and_then(|t| t.get("targetId"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the browser opened no tab"))?
        .to_string();
    let attached = cdp
        .browser_call(
            "Target.attachToTarget",
            json!({ "targetId": page, "flatten": true }),
        )
        .await?;
    attached
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the browser would not attach to its tab"))
}

/// Scheme, host and port, with no trailing slash.
fn origin_of(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url).with_context(|| format!("not an address: {url}"))?;
    Ok(parsed.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_has_no_path_and_no_trailing_slash() {
        assert_eq!(
            origin_of("https://www.instagram.com/api/v1/x/?a=1").unwrap(),
            "https://www.instagram.com"
        );
        assert_eq!(
            origin_of("http://127.0.0.1:8765/api/v1/x/").unwrap(),
            "http://127.0.0.1:8765"
        );
    }
}
