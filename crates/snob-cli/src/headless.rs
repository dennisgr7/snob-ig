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
//!   isolated world ([`isolated_world`]).
//!
//! What is left is left knowingly: WebGL is absent — `getContext` returns
//! nothing without a GPU the browser will use headless — and the switch that
//! brings in the software renderer is one Chromium itself calls unsafe.
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
/// pipe's ceiling, for the envelope. A message over that ceiling ends the
/// connection rather than failing the one request, so the cap has to be
/// applied in the page, before anything is sent back.
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
    /// is on now; `None` until one is made. See [`isolated_world`].
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
        let mut cdp = Cdp::connect(launched, &cancel).await?;
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
        ];
        cdp.on_attach(crate::cdp::OnAttach {
            page: page_commands.clone(),
            worker: vec![("Network.setUserAgentOverride", identity)],
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

        let tab = attach_to_a_tab(&mut cdp).await?;
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
async fn attach_to_a_tab(cdp: &mut Cdp) -> Result<String> {
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

/// What [`machine_hints`] came back with.
struct Hints {
    values: Option<Value>,
    /// Asked just now rather than read from the mark, which then needs writing.
    fresh: bool,
}

/// What this browser says about the machine: the brands, the architecture,
/// the bitness, the operating system's version — the client hints a page can
/// ask for.
///
/// **Asked of a second browser, once, and kept.** The browser the requests go
/// from runs with `--user-agent`, and that flag makes Chromium blank every one
/// of these fields (see [`probe_platform`]); the fallbacks for a blank are only
/// true on some machines — an empty platform version is Linux's answer and
/// nobody else's. So a throwaway browser with no such flag is asked, on a
/// profile of its own that is deleted after, and the answer is written into
/// the profile mark for this browser and major version. It costs a browser
/// start the first time and after each browser update, and nothing on any
/// other run. A failure costs only the fallbacks.
async fn machine_hints(
    browser: &crate::browser::Browser,
    paths: &AppPaths,
    mark: &mut ProfileMark,
) -> Hints {
    if let Some(known) = &mark.hints
        && known.browser == browser.path
        && known.major == browser.major_version
    {
        return Hints {
            values: Some(known.values.clone()),
            fresh: false,
        };
    }
    match ask_without_the_flag(browser, paths).await {
        Ok(values) => {
            mark.hints = Some(MachineHints {
                browser: browser.path.clone(),
                major: browser.major_version,
                values: values.clone(),
            });
            Hints {
                values: Some(values),
                fresh: true,
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "could not ask the browser about the machine");
            Hints {
                values: None,
                fresh: false,
            }
        }
    }
}

async fn ask_without_the_flag(
    browser: &crate::browser::Browser,
    paths: &AppPaths,
) -> Result<Value> {
    paths.ensure_dirs()?;
    let dir = paths.data_dir().join("browser-probe");
    snob_store::paths::create_fresh_private_dir(&dir)?;
    let asked = async {
        let cancel = CancelToken::default();
        let flags = ["--headless=new".to_string()];
        let launched = crate::cdp::launch_throwaway(browser, &dir, &flags, &cancel).await?;
        let mut cdp = Cdp::connect(launched, &cancel).await?;
        let found = match attach_to_a_tab(&mut cdp).await {
            Ok(tab) => probe_platform(&mut cdp, &tab).await,
            Err(e) => Err(e),
        };
        cdp.close().await;
        found
    }
    .await;
    if let Err(e) = snob_store::paths::remove_tree(&dir) {
        tracing::debug!(error = %e, "could not remove the probe's profile");
    }
    asked
}

/// What a browser says about itself and the machine: its brands, the
/// operating system's version, the architecture, the bitness.
///
/// Asked of the throwaway browser [`machine_hints`] starts **without**
/// `--user-agent`, because with it Chromium blanks every field but the brands,
/// on the grounds that it can no longer vouch for them. The brands matter most:
/// a headless Chromium reports the ones its windowed build does — measured on
/// Chromium 153, `Chromium` and the GREASE entry, no `HeadlessChrome` — and
/// computing them from the User-Agent instead named every Chromium on Linux
/// `Google Chrome`, since the two send the same User-Agent and only the brand
/// list tells them apart. [`metadata`] still treats an empty string as "not
/// said", for the day a field comes back blank anyway.
///
/// Only readable from a secure context, and `about:blank` is not one;
/// `http://127.0.0.1` is. So a page is served on a loopback port for the
/// length of one question. Nothing leaves the machine, and nothing about
/// Instagram is involved.
async fn probe_platform(cdp: &mut Cdp, tab: &str) -> Result<Value> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer).await;
            let body = "<!doctype html><title>snob</title>";
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    });

    let found = async {
        navigate_tab(cdp, tab, &format!("http://127.0.0.1:{port}/")).await?;
        let answer = evaluate(
            cdp,
            tab,
            "navigator.userAgentData.getHighEntropyValues(\
             ['architecture','bitness','model','platformVersion','wow64'])\
             .then(v => Object.assign({ brands: navigator.userAgentData.brands }, v))",
            COMMAND_TIMEOUT,
        )
        .await?;
        Ok::<_, anyhow::Error>(answer)
    }
    .await;
    server.abort();
    found
}

/// The client-hints metadata the override states.
///
/// The brands are the browser's own when it said them (see
/// [`probe_platform`]), and computed from the User-Agent only when it did
/// not. Every other field is what the browser said, unless it said nothing —
/// an empty string included — and then the one thing this process knows for
/// itself, or empty rather than invented.
fn metadata(user_agent: &str, full_version: &str, platform: Option<&Value>) -> Value {
    let said = platform
        .and_then(|p| p.get("brands"))
        .and_then(Value::as_array)
        .map(|all| {
            all.iter()
                .filter_map(|b| {
                    Some((
                        b.get("brand")?.as_str()?.to_string(),
                        b.get("version")?.as_str()?.to_string(),
                    ))
                })
                .collect::<Vec<_>>()
        })
        .filter(|brands| !brands.is_empty());
    let brands = said
        .or_else(|| snob_ig::client_hints::brand_list(user_agent))
        .unwrap_or_default();
    let full = |version: &str| {
        if version.parse::<u32>().is_ok() && full_version.starts_with(&format!("{version}.")) {
            full_version.to_string()
        } else {
            format!("{version}.0.0.0")
        }
    };
    let field = |name: &str, fallback: &str| {
        platform
            .and_then(|p| p.get(name))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(fallback)
            .to_string()
    };
    json!({
        "brands": brands
            .iter()
            .map(|(brand, version)| json!({ "brand": brand, "version": version }))
            .collect::<Vec<_>>(),
        "fullVersionList": brands
            .iter()
            .map(|(brand, version)| json!({ "brand": brand, "version": full(version) }))
            .collect::<Vec<_>>(),
        "fullVersion": full_version,
        "platform": snob_ig::client_hints::platform_name(user_agent),
        "platformVersion": field("platformVersion", ""),
        "architecture": field(
            "architecture",
            if cfg!(target_arch = "aarch64") { "arm" } else { "x86" },
        ),
        "model": field("model", ""),
        "mobile": false,
        "bitness": field("bitness", "64"),
        "wow64": platform
            .and_then(|p| p.get("wow64"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

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
/// **Another account's browser is emptied first.** Cookies and site data both:
/// `mid`, `ig_did` and `datr` name the device, and carrying one account's
/// into another's session is how two accounts come to look like one person's.
async fn sync_cookies(
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

    if holds_this_account && !holds_another && mark.session.as_deref() == Some(given.as_str()) {
        return Ok(());
    }

    let mut device_kept = std::collections::HashSet::new();
    if holds_another {
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
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileMark {
    /// The executable that created the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<std::path::PathBuf>,
    /// The account the profile holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pk: Option<u64>,
    /// The fingerprint of the stored session last handed to the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// What the browser said about the machine; see [`machine_hints`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<MachineHints>,
}

/// The browser's own description of the machine, kept per browser and major
/// version. Not a secret, and not about any account.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MachineHints {
    pub browser: std::path::PathBuf,
    pub major: u32,
    pub values: Value,
}

impl ProfileMark {
    const FILE: &'static str = "snob-profile.json";

    /// The mark beside this profile, or an empty one: a profile without a
    /// mark is one nothing is known about, which is what empty says.
    pub fn read(paths: &AppPaths) -> Self {
        std::fs::read(paths.browser_profile().join(Self::FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Written in place: the file is a hint, and a torn one reads as empty,
    /// which costs one cookie write on the next run and nothing else.
    pub fn write(&self, paths: &AppPaths) {
        let path = paths.browser_profile().join(Self::FILE);
        let written = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&path, bytes));
        if let Err(e) = written {
            tracing::debug!(error = %e, path = %path.display(), "could not write the profile mark");
        }
    }

    /// The mark a browser login leaves: the profile was made by `browser`, and
    /// the session it produced is the one being stored.
    pub fn after_login(paths: &AppPaths, browser: &crate::browser::Browser, session: &Session) {
        let kept = Self::read(paths).hints;
        Self {
            browser: Some(browser.path.clone()),
            pk: Some(session.ds_user_id.get()),
            session: Some(session.fingerprint()),
            hints: kept,
        }
        .write(paths);
    }
}

/// Sends the tab somewhere and waits for it to finish loading.
async fn navigate(live: &mut Live, url: &str) -> Result<(), PageError> {
    // The world dies with the document it was made in.
    live.world = None;
    navigate_tab(&mut live.cdp, &live.tab, url).await?;
    tokio::time::sleep(SETTLE).await;
    Ok(())
}

/// The navigation itself. A page that could not be reached, or never finished
/// loading, is the network's failure and says so; a protocol command that
/// failed is the browser's.
async fn navigate_tab(cdp: &mut Cdp, tab: &str, url: &str) -> Result<(), PageError> {
    let broken = |e: anyhow::Error| PageError::Browser(format!("{e:#}"));
    let went = cdp
        .page_call(tab, "Page.navigate", json!({ "url": url }), COMMAND_TIMEOUT)
        .await
        .map_err(broken)?;
    if let Some(error) = went.get("errorText").and_then(Value::as_str)
        && !error.is_empty()
    {
        return Err(PageError::Unreachable(format!(
            "the browser could not open {url}: {error}"
        )));
    }
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    loop {
        let ready = evaluate(cdp, tab, "document.readyState", COMMAND_TIMEOUT)
            .await
            .map_err(broken)?;
        if ready.as_str() == Some("complete") {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(PageError::Unreachable(format!(
                "{url} did not finish loading"
            )));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A navigation whose answer is the document it lands on.
///
/// **The status and the address are the ones the browser saw**, not assumed.
/// This used to answer 200 and "not redirected" whatever happened, so a 429
/// on the page a write reads its tokens from, or a redirect to the login or
/// challenge page, arrived as a page with no tokens in it and was reported as
/// an expired session — with no cooldown written. The navigation's own
/// timing entry carries the status (Chromium 109 and later) and how many
/// redirects led to it; a browser too old to say is taken at its word that
/// the page loaded.
async fn navigate_and_read(live: &mut Live, url: &str) -> Result<PageResponse, PageError> {
    let broken = |e: anyhow::Error| PageError::Browser(format!("{e:#}"));
    navigate(live, url).await?;
    let page = evaluate(
        &mut live.cdp,
        &live.tab,
        &format!(
            "(() => {{ const n = performance.getEntriesByType('navigation')[0] || {{}}; \
             const body = document.documentElement.outerHTML; {WIRE_LENGTH} \
             const tooLarge = wire(body) > {PAGE_WIRE_CAP}; \
             return {{ url: location.href, status: n.responseStatus || 0, \
             hops: n.redirectCount || 0, body: tooLarge ? '' : body, tooLarge }}; }})()"
        ),
        COMMAND_TIMEOUT,
    )
    .await
    .map_err(broken)?;
    let landed = page
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or(url)
        .to_string();
    live.origin = origin_of(&landed).map_err(broken)?;
    let status = page
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .filter(|s| *s != 0)
        .unwrap_or(200);
    let hops = page.get("hops").and_then(Value::as_u64).unwrap_or(0);
    Ok(PageResponse {
        status,
        headers: Vec::new(),
        body: page
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        redirected: hops > 0 || !same_document(url, &landed),
        url: landed,
        too_large: page
            .get("tooLarge")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// How many bytes a string takes as a protocol message: six for every
/// character outside printable ASCII, which the browser writes as `\uXXXX`,
/// two for a quote or a backslash, one for the rest. Declared into the
/// scripts that hand a body back, so the cap is measured in what it protects.
const WIRE_LENGTH: &str = "const wire = (t) => { let n = t.length; \
    for (let i = 0; i < t.length; i++) { const c = t.charCodeAt(i); \
    if (c < 0x20 || c > 0x7e) n += 5; else if (c === 34 || c === 92) n += 1; } \
    return n; };";

/// Whether two addresses name the same document: the fragment aside, which a
/// page is free to change without anything having been requested.
fn same_document(asked: &str, landed: &str) -> bool {
    let strip = |u: &str| {
        url::Url::parse(u).ok().map(|mut u| {
            u.set_fragment(None);
            u
        })
    };
    strip(asked) == strip(landed)
}

/// The script that sends one request from the page.
///
/// `X-CSRFToken` is read from the cookie at the moment of sending, because the
/// browser rotates it and this process never sees the rotation. The claim is
/// the app's own when this process has not been given one yet.
///
/// **No redirect is followed, GET or POST.** A hop the browser follows by
/// itself is a request sent before this process could pay for it, and to
/// wherever the answer pointed — so it could only be charged afterwards and
/// held to the origin rule after it had already gone. Instagram's API does
/// not redirect a working call; the one it is known to redirect is a dead
/// session's, to the login page. With `manual` that arrives as an opaque
/// redirect, status 0, and is refused without a second request having been
/// made. A POST was always this way: a followed redirect is a write sent
/// twice.
const FETCH: &str = r#"(async (q) => {
  const headers = new Headers(q.headers);
  const csrf = document.cookie.match(/(?:^|;\s*)csrftoken=([^;]*)/);
  if (csrf) headers.set('X-CSRFToken', decodeURIComponent(csrf[1]));
  else if (q.method !== 'GET') return { error: 'no CSRF token', kind: 'csrf' };
  if (headers.get('X-IG-WWW-Claim') === '0') {
    try {
      const kept = sessionStorage.getItem('www-claim-v2');
      if (kept) headers.set('X-IG-WWW-Claim', kept);
    } catch (e) {}
  }
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), q.timeout_ms);
  try {
    const r = await fetch(q.url, {
      method: q.method,
      headers,
      body: q.body === null ? undefined : q.body,
      referrer: q.referrer,
      redirect: 'manual',
      signal: abort.signal,
    });
    const fresh = r.headers.get('x-ig-set-www-claim');
    if (fresh) { try { sessionStorage.setItem('www-claim-v2', fresh); } catch (e) {} }
    const list = [];
    r.headers.forEach((value, name) => list.push([name, value]));
    const text = r.type === 'opaqueredirect' ? '' : await r.text();
    const tooLarge = wire(text) > q.cap;
    return {
      status: r.status, headers: list, body: tooLarge ? '' : text,
      url: r.url, redirected: r.redirected, tooLarge,
    };
  } catch (e) {
    return { error: String(e), kind: 'network' };
  } finally {
    clearTimeout(timer);
  }
})"#;

async fn fetch(live: &mut Live, request: &PageRequest) -> Result<PageResponse, PageError> {
    let argument = json!({
        "method": request.method,
        "url": request.url,
        "headers": request.headers,
        "referrer": request.referrer,
        "body": request.body,
        "cap": request.cap.min(PAGE_WIRE_CAP),
        "timeout_ms": request.timeout_ms,
    });
    let expression = format!("(() => {{ {WIRE_LENGTH} return {FETCH}({argument}); }})()");
    let timeout = Duration::from_millis(request.timeout_ms) + Duration::from_secs(10);
    let world = isolated_world(live).await?;
    let answer = evaluate_in(&mut live.cdp, &live.tab, Some(world), &expression, timeout)
        .await
        .map_err(|e| PageError::Browser(format!("{e:#}")))?;
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        return Err(match answer.get("kind").and_then(Value::as_str) {
            Some("csrf") => PageError::NoCsrfToken,
            Some("network") => PageError::Unreachable(error.to_string()),
            _ => PageError::Browser(format!("the page could not send the request: {error}")),
        });
    }
    Ok(PageResponse {
        status: answer
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|s| u16::try_from(s).ok())
            .unwrap_or(0),
        headers: answer
            .get("headers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|pair| {
                let pair = pair.as_array()?;
                Some((
                    pair.first()?.as_str()?.to_ascii_lowercase(),
                    pair.get(1)?.as_str()?.to_string(),
                ))
            })
            .collect(),
        body: answer
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url: answer
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        redirected: answer
            .get("redirected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        too_large: answer
            .get("tooLarge")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Runs an expression in the tab and hands back its value.
///
/// `Runtime.evaluate` without `Runtime.enable`: enabling the domain is what
/// makes the console report objects back over the pipe, and that reporting is
/// the side effect pages use to notice a debugger is attached.
async fn evaluate(cdp: &mut Cdp, tab: &str, expression: &str, timeout: Duration) -> Result<Value> {
    evaluate_in(cdp, tab, None, expression, timeout).await
}

/// [`evaluate`], in a given world of the tab's document.
async fn evaluate_in(
    cdp: &mut Cdp,
    tab: &str,
    world: Option<i64>,
    expression: &str,
    timeout: Duration,
) -> Result<Value> {
    let mut params = json!({
        "expression": expression,
        "awaitPromise": true,
        "returnByValue": true,
    });
    if let Some(world) = world {
        params["contextId"] = json!(world);
    }
    let result = cdp
        .page_call(tab, "Runtime.evaluate", params, timeout)
        .await?;
    if let Some(details) = result.get("exceptionDetails") {
        let text = details
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(Value::as_str)
            .or_else(|| details.get("text").and_then(Value::as_str))
            .unwrap_or("an exception");
        bail!("the page threw: {text}");
    }
    Ok(result
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

/// The world the requests are sent from, made once per document.
///
/// **Not the page's own.** `Runtime.evaluate` runs in the page's main world by
/// default, where the site's scripts run too: a `fetch` they have wrapped sees
/// every call snob makes, and the page's resource timing lists each one —
/// both measured. An isolated world shares the document, its cookies and its
/// storage, sends as the page's origin, and is out of the page's reach. It
/// dies with its document, so [`navigate`] forgets it and the next request
/// makes another.
async fn isolated_world(live: &mut Live) -> Result<i64, PageError> {
    if let Some(world) = live.world {
        return Ok(world);
    }
    let broken = |e: anyhow::Error| PageError::Browser(format!("{e:#}"));
    let tree = live
        .cdp
        .page_call(&live.tab, "Page.getFrameTree", json!({}), COMMAND_TIMEOUT)
        .await
        .map_err(broken)?;
    let frame = tree
        .pointer("/frameTree/frame/id")
        .and_then(Value::as_str)
        .ok_or_else(|| PageError::Browser("the tab has no document".to_string()))?
        .to_string();
    let made = live
        .cdp
        .page_call(
            &live.tab,
            "Page.createIsolatedWorld",
            json!({ "frameId": frame, "worldName": "" }),
            COMMAND_TIMEOUT,
        )
        .await
        .map_err(broken)?;
    let world = made
        .get("executionContextId")
        .and_then(Value::as_i64)
        .ok_or_else(|| PageError::Browser("the browser made no world".to_string()))?;
    live.world = Some(world);
    Ok(world)
}

/// Scheme, host and port, with no trailing slash.
fn origin_of(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url).with_context(|| format!("not an address: {url}"))?;
    Ok(parsed.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

    /// The metadata states the installed brand, never the headless one, with
    /// the running browser's full version on the two real entries and the
    /// machine's own platform details where it could read them.
    #[test]
    fn the_metadata_names_the_real_browser() {
        let platform = json!({
            "platformVersion": "19.0.0",
            "architecture": "x86",
            "bitness": "64",
            "model": "",
            "wow64": false,
        });
        let m = metadata(UA, "141.0.7390.54", Some(&platform));

        let brands: Vec<&str> = m["brands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["brand"].as_str().unwrap())
            .collect();
        assert!(brands.contains(&"Google Chrome"), "{brands:?}");
        assert!(brands.contains(&"Chromium"), "{brands:?}");
        assert!(!brands.iter().any(|b| b.contains("Headless")), "{brands:?}");

        let full: Vec<(&str, &str)> = m["fullVersionList"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| (b["brand"].as_str().unwrap(), b["version"].as_str().unwrap()))
            .collect();
        assert!(
            full.contains(&("Google Chrome", "141.0.7390.54")),
            "{full:?}"
        );
        assert!(full.contains(&("Chromium", "141.0.7390.54")), "{full:?}");
        assert!(
            full.iter()
                .any(|(b, v)| b.starts_with("Not") && v.ends_with(".0.0.0")),
            "the GREASE entry keeps its own version: {full:?}"
        );

        assert_eq!(m["platform"], "Windows");
        assert_eq!(m["platformVersion"], "19.0.0");
        assert_eq!(m["mobile"], false);
    }

    /// A Chromium says it is Chromium. Linux's `chromium` sends the same
    /// User-Agent as Google Chrome, so computing the brands from it claimed a
    /// brand the binary does not have — measured on the wire from Chromium
    /// 153. The probe is what the browser reports, and it wins.
    #[test]
    fn a_chromium_is_not_announced_as_google_chrome() {
        let probed = json!({
            "brands": [
                { "brand": "Chromium", "version": "153" },
                { "brand": "Not_A Brand", "version": "8" },
            ],
            "architecture": "",
            "bitness": "",
            "platformVersion": "",
        });
        let m = metadata(UA, "153.0.8010.52", Some(&probed));

        let brands: Vec<&str> = m["brands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["brand"].as_str().unwrap())
            .collect();
        assert_eq!(brands, ["Chromium", "Not_A Brand"]);
        let full: Vec<&str> = m["fullVersionList"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["version"].as_str().unwrap())
            .collect();
        assert_eq!(full, ["153.0.8010.52", "8.0.0.0"]);
    }

    /// `--user-agent` makes Chromium answer the high-entropy fields with empty
    /// strings. Those are "not said", not values: stating `architecture: ""`
    /// is a header no browser sends.
    #[test]
    fn an_empty_answer_is_not_a_value() {
        let blank = json!({ "architecture": "", "bitness": "", "platformVersion": "" });
        let m = metadata(UA, "141.0.7390.54", Some(&blank));
        assert_eq!(
            m["architecture"],
            if cfg!(target_arch = "aarch64") {
                "arm"
            } else {
                "x86"
            }
        );
        assert_eq!(m["bitness"], "64");
        assert_eq!(m["platformVersion"], "");
    }

    /// Without the probe, the platform version is left empty rather than
    /// guessed, and the rest still holds.
    #[test]
    fn without_the_probe_nothing_is_invented() {
        let m = metadata(UA, "141.0.7390.54", None);
        assert_eq!(m["platformVersion"], "");
        assert_eq!(m["bitness"], "64");
    }

    /// A fragment is not a redirect; a different path or query is.
    #[test]
    fn only_a_different_document_counts_as_a_redirect() {
        let asked = "https://www.instagram.com/someone/";
        assert!(same_document(
            asked,
            "https://www.instagram.com/someone/#top"
        ));
        assert!(!same_document(
            asked,
            "https://www.instagram.com/accounts/login/?next=%2Fsomeone%2F"
        ));
        assert!(!same_document(
            asked,
            "https://www.instagram.com/challenge/"
        ));
    }

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
