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
//! **What a headless Chrome gives away, and what is done about each.** Every
//! item was measured against Chromium 141 in September 2026:
//!
//! - `navigator.webdriver` is `true` under the debugging pipe, headless or
//!   not. `--disable-blink-features=AutomationControlled`, in `cdp.rs`.
//! - The User-Agent says `HeadlessChrome`. `--user-agent` with the installed
//!   browser's own, which the session already carries.
//! - That flag alone leaves `Sec-CH-UA-Full-Version-List` **empty**, which is
//!   worse than the name it hides. `Emulation.setUserAgentOverride` with full
//!   metadata fixes every client hint: the brands are built by the same
//!   algorithm `client_hints.rs` checks against real captures, the versions
//!   come from the running browser, and the platform details are read from
//!   the machine itself (see [`probe_platform`]).
//! - The screen is 800 by 600. `--window-size` and `--screen-info`.
//! - `navigator.languages` is `en-US`. The override's language list, read
//!   from the system like `Accept-Language` always was.
//!
//! What is left is left knowingly: WebGL reports a software renderer, as it
//! does on any machine without a GPU the browser will use headless.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use snob_core::Pk;
use snob_core::session::Session;
use snob_ig::client::page::{Page, PageFactory, PageFuture, PageRequest, PageResponse};
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

/// The window, and the screen it claims to be on. The commonest desktop size.
const WINDOW: (u32, u32) = (1920, 1080);

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
    /// The account whose cookies have been checked in the browser.
    synced: Option<Pk>,
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

    async fn send_inner(&self, request: PageRequest) -> Result<PageResponse> {
        let mut state = self.state.lock().await;
        if state.is_none() {
            *state = Some(self.start().await?);
        }
        let live = state.as_mut().expect("started just above");

        let result = self.send_on(live, &request).await;
        if result.is_err() {
            // A tab that failed once is not trusted with the next request: the
            // browser may be gone. The next request starts a fresh one.
            if let Some(live) = state.take() {
                live.cdp.close().await;
            }
        }
        result
    }

    async fn send_on(&self, live: &mut Live, request: &PageRequest) -> Result<PageResponse> {
        let origin = origin_of(&request.url)?;
        let session = self.wanted()?;
        if live.synced != Some(session.ds_user_id) {
            sync_cookies(live, &session, &origin).await?;
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
        let browser = session
            .browser
            .as_deref()
            .and_then(crate::browser::detect_named)
            .or_else(crate::browser::detect)
            .ok_or_else(|| {
                anyhow!(
                    "snob sends its requests from a browser, and no Chrome, Edge or Chromium \
                     was found on this machine"
                )
            })?;
        // The session's User-Agent is the installed browser's, kept current by
        // `browser::refresh_user_agent` on every run.
        let user_agent = session.user_agent.replace("HeadlessChrome/", "Chrome/");

        let flags = vec![
            "--headless=new".to_string(),
            format!("--user-agent={user_agent}"),
            format!("--window-size={},{}", WINDOW.0, WINDOW.1),
            format!("--screen-info={{{}x{}}}", WINDOW.0, WINDOW.1),
        ];
        let cancel = CancelToken::default();
        let launched = crate::cdp::launch_headless(&browser, &self.paths, &flags, &cancel)
            .await
            .with_context(|| format!("could not start {} without a window", browser.name))?;
        let mut cdp = Cdp::connect(launched, &cancel).await?;

        let version = cdp.browser_call("Browser.getVersion", json!({})).await?;
        let full_version = version
            .get("product")
            .and_then(Value::as_str)
            .and_then(|product| product.split('/').nth(1))
            .unwrap_or_default()
            .to_string();

        let tab = attach_to_a_tab(&mut cdp).await?;
        let platform = match probe_platform(&mut cdp, &tab).await {
            Ok(found) => Some(found),
            Err(e) => {
                tracing::debug!(error = %e, "could not read the platform details");
                None
            }
        };
        let metadata = metadata(&user_agent, &full_version, platform.as_ref());
        cdp.page_call(
            &tab,
            "Emulation.setUserAgentOverride",
            json!({
                "userAgent": user_agent,
                "acceptLanguage": snob_ig::client_hints::languages(),
                "userAgentMetadata": metadata,
            }),
            COMMAND_TIMEOUT,
        )
        .await?;

        Ok(Live {
            cdp,
            tab,
            origin: "about:blank".to_string(),
            synced: None,
        })
    }
}

impl Page for Headless {
    fn send(&self, request: PageRequest) -> PageFuture<'_> {
        Box::pin(async move { self.send_inner(request).await.map_err(|e| format!("{e:#}")) })
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

/// What this machine really is, asked of the browser before anything is
/// overridden: the operating system's version, the architecture, the bitness.
///
/// Those are only readable from a secure context, and `about:blank` is not
/// one; `http://127.0.0.1` is. So a page is served on a loopback port for the
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
             ['architecture','bitness','model','platformVersion','wow64'])",
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
fn metadata(user_agent: &str, full_version: &str, platform: Option<&Value>) -> Value {
    let brands = snob_ig::client_hints::brand_list(user_agent).unwrap_or_default();
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

/// Makes sure the browser carries this session.
///
/// The profile is where the login happened, so ordinarily it already does and
/// nothing is written: the browser's own cookies are fresher than anything
/// stored. A session that came from a paste, or one stored before requests
/// moved into the browser, is written in once.
async fn sync_cookies(live: &mut Live, session: &Session, origin: &str) -> Result<()> {
    let host = url::Url::parse(origin)?
        .host_str()
        .unwrap_or_default()
        .to_string();
    let on_instagram = host == "instagram.com" || host.ends_with(".instagram.com");

    let cookies = live
        .cdp
        .browser_call("Storage.getCookies", json!({}))
        .await?;
    let already = cookies
        .get("cookies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| {
            let domain = c.get("domain").and_then(Value::as_str).unwrap_or("");
            if on_instagram {
                domain == "instagram.com" || domain.ends_with(".instagram.com")
            } else {
                domain.trim_start_matches('.') == host
            }
        })
        .any(|c| {
            c.get("name").and_then(Value::as_str) == Some("sessionid")
                && c.get("value")
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.starts_with(&format!("{}%3A", session.ds_user_id)))
        });
    if already {
        return Ok(());
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
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            set.push(cookie(name, value, name != "mid"));
        }
    }
    live.cdp
        .browser_call("Storage.setCookies", json!({ "cookies": set }))
        .await?;
    Ok(())
}

/// Sends the tab somewhere and waits for it to finish loading.
async fn navigate(live: &mut Live, url: &str) -> Result<()> {
    navigate_tab(&mut live.cdp, &live.tab, url).await?;
    tokio::time::sleep(SETTLE).await;
    Ok(())
}

async fn navigate_tab(cdp: &mut Cdp, tab: &str, url: &str) -> Result<()> {
    let went = cdp
        .page_call(tab, "Page.navigate", json!({ "url": url }), COMMAND_TIMEOUT)
        .await?;
    if let Some(error) = went.get("errorText").and_then(Value::as_str)
        && !error.is_empty()
    {
        bail!("the browser could not open {url}: {error}");
    }
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    loop {
        let ready = evaluate(cdp, tab, "document.readyState", COMMAND_TIMEOUT).await?;
        if ready.as_str() == Some("complete") {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("{url} did not finish loading");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A navigation whose answer is the document it lands on.
async fn navigate_and_read(live: &mut Live, url: &str) -> Result<PageResponse> {
    navigate(live, url).await?;
    let page = evaluate(
        &mut live.cdp,
        &live.tab,
        "({ url: location.href, body: document.documentElement.outerHTML })",
        COMMAND_TIMEOUT,
    )
    .await?;
    live.origin = origin_of(page.get("url").and_then(Value::as_str).unwrap_or(url))?;
    Ok(PageResponse {
        status: 200,
        headers: Vec::new(),
        body: page
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url: page
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or(url)
            .to_string(),
        redirected: false,
        too_large: false,
    })
}

/// The script that sends one request from the page.
///
/// `X-CSRFToken` is read from the cookie at the moment of sending, because the
/// browser rotates it and this process never sees the rotation. The claim is
/// the app's own when this process has not been given one yet. A GET follows
/// redirects the way the app's own calls do; a POST follows none, because a
/// followed redirect is a write sent twice.
const FETCH: &str = r#"(async (q) => {
  const headers = new Headers(q.headers);
  const csrf = document.cookie.match(/(?:^|;\s*)csrftoken=([^;]*)/);
  if (csrf) headers.set('X-CSRFToken', decodeURIComponent(csrf[1]));
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
      redirect: q.method === 'GET' ? 'follow' : 'manual',
      signal: abort.signal,
    });
    const fresh = r.headers.get('x-ig-set-www-claim');
    if (fresh) { try { sessionStorage.setItem('www-claim-v2', fresh); } catch (e) {} }
    const list = [];
    r.headers.forEach((value, name) => list.push([name, value]));
    const text = r.type === 'opaqueredirect' ? '' : await r.text();
    const tooLarge = text.length > q.cap;
    return {
      status: r.status, headers: list, body: tooLarge ? '' : text,
      url: r.url, redirected: r.redirected, tooLarge,
    };
  } catch (e) {
    return { error: String(e) };
  } finally {
    clearTimeout(timer);
  }
})"#;

async fn fetch(live: &mut Live, request: &PageRequest) -> Result<PageResponse> {
    let argument = json!({
        "method": request.method,
        "url": request.url,
        "headers": request.headers,
        "referrer": request.referrer,
        "body": request.body,
        "cap": request.cap,
        "timeout_ms": request.timeout_ms,
    });
    let expression = format!("{FETCH}({argument})");
    let timeout = Duration::from_millis(request.timeout_ms) + Duration::from_secs(10);
    let answer = evaluate(&mut live.cdp, &live.tab, &expression, timeout).await?;
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        bail!("the page could not send the request: {error}");
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
    let result = cdp
        .page_call(
            tab,
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "awaitPromise": true,
                "returnByValue": true,
            }),
            timeout,
        )
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

    /// Without the probe, the platform version is left empty rather than
    /// guessed, and the rest still holds.
    #[test]
    fn without_the_probe_nothing_is_invented() {
        let m = metadata(UA, "141.0.7390.54", None);
        assert_eq!(m["platformVersion"], "");
        assert_eq!(m["bitness"], "64");
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
