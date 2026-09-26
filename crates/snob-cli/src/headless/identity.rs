//! What the browser says about itself and the machine, and the identity the
//! headless one is given from it: the client hints a page can ask for.

use anyhow::Result;
use serde_json::{Value, json};
use snob_ig::pace::CancelToken;
use snob_store::paths::AppPaths;

use super::tab::{evaluate, navigate_tab};
use super::{COMMAND_TIMEOUT, attach_to_a_tab};
use crate::cdp::Cdp;

/// What this browser says about the machine: the brands, the architecture,
/// the bitness, the operating system's version — the client hints a page can
/// ask for.
///
/// **Asked of a second browser, once, and kept.** The browser the requests go
/// from runs with `--user-agent`, and that flag makes Chromium blank every one
/// of these fields (see [`probe_platform`]); the fallbacks for a blank are only
/// true on some machines — an empty platform version is Linux's answer and
/// nobody else's. So a throwaway browser with no such flag is asked, on a
/// profile of its own that is deleted after, and the answer is kept in
/// [`KnownHints`] for this browser and version. It costs a browser start the
/// first time and after each browser update, and nothing on any other run. A
/// failure costs only the fallbacks.
pub(super) async fn machine_hints(
    browser: &crate::browser::Browser,
    paths: &AppPaths,
) -> Option<Value> {
    let mut known = KnownHints::read(paths);
    if let Some(values) = known.for_browser(browser) {
        return Some(values);
    }
    match ask_without_the_flag(browser, paths).await {
        Ok(values) => {
            known.remember(MachineHints {
                browser: browser.path.clone(),
                version: browser.full_version.clone(),
                values: values.clone(),
            });
            known.write(paths);
            Some(values)
        }
        Err(e) => {
            tracing::debug!(error = %e, "could not ask the browser about the machine");
            None
        }
    }
}

/// The browser's own description of the machine, kept per browser and
/// version. Not a secret, and not about any account.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MachineHints {
    pub browser: std::path::PathBuf,
    /// The browser's whole version when it was asked. A new one is asked
    /// again: the full version list changes with every update.
    pub version: String,
    pub values: Value,
}

/// Every browser's [`MachineHints`], in one file beside the profiles.
///
/// **Beside them, not in them.** They are about the machine, so one answer
/// serves every account's profile; kept in each profile's mark, as they were
/// while there was one profile, every new account would pay a browser start
/// to ask again what the machine already said. One entry per browser, so a
/// machine with an account on Chrome and another on Edge keeps both.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnownHints {
    #[serde(default)]
    pub hints: Vec<MachineHints>,
}

impl KnownHints {
    /// What is kept, or nothing: a file that is missing or torn costs one
    /// question to a browser, and nothing else.
    pub fn read(paths: &AppPaths) -> Self {
        std::fs::read(paths.browser_hints_file())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn write(&self, paths: &AppPaths) {
        let path = paths.browser_hints_file();
        let written = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&path, bytes));
        if let Err(e) = written {
            tracing::debug!(error = %e, path = %path.display(), "could not keep the machine hints");
        }
    }

    /// What this browser said, when it was this very version that said it.
    pub fn for_browser(&self, browser: &crate::browser::Browser) -> Option<Value> {
        self.hints
            .iter()
            .find(|h| h.browser == browser.path && h.version == browser.full_version)
            .map(|h| h.values.clone())
    }

    /// Keeps `hints`, in place of whatever the same browser said before.
    pub fn remember(&mut self, hints: MachineHints) {
        self.hints.retain(|h| h.browser != hints.browser);
        self.hints.push(hints);
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
        let cdp = Cdp::connect(launched, &cancel).await?;
        let found = match attach_to_a_tab(&cdp).await {
            Ok(tab) => probe_platform(&cdp, &tab).await,
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
async fn probe_platform(cdp: &Cdp, tab: &str) -> Result<Value> {
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
             ['architecture','bitness','model','platformVersion','wow64',\
             'fullVersionList','uaFullVersion'])\
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
pub(super) fn metadata(user_agent: &str, full_version: &str, platform: Option<&Value>) -> Value {
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
    // **The browser's own list, when it was said by this very version.** The
    // computed one gives every real brand the product's version, which is
    // right for Chrome and wrong for Edge, whose own number is not
    // Chromium's: `"Chromium";v="141.0.3537.57"` names a Chromium build that
    // never existed, on the browser most Windows machines have.
    let said_in_full = platform
        .filter(|p| p.get("uaFullVersion").and_then(Value::as_str) == Some(full_version))
        .and_then(|p| p.get("fullVersionList"))
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
        .cloned();
    json!({
        "brands": brands
            .iter()
            .map(|(brand, version)| json!({ "brand": brand, "version": version }))
            .collect::<Vec<_>>(),
        "fullVersionList": said_in_full.unwrap_or_else(|| brands
            .iter()
            .map(|(brand, version)| json!({ "brand": brand, "version": full(version) }))
            .collect::<Vec<_>>()),
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

    /// Edge's own number is not Chromium's. The list the browser said for
    /// the version running is used as it was said; one said by another
    /// version is not, and the computed one stands in.
    #[test]
    fn the_full_versions_are_the_browsers_own() {
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                    (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0";
        let said = json!({
            "brands": [
                { "brand": "Microsoft Edge", "version": "141" },
                { "brand": "Not?A_Brand", "version": "8" },
                { "brand": "Chromium", "version": "141" },
            ],
            "uaFullVersion": "141.0.3537.57",
            "fullVersionList": [
                { "brand": "Microsoft Edge", "version": "141.0.3537.57" },
                { "brand": "Not?A_Brand", "version": "8.0.0.0" },
                { "brand": "Chromium", "version": "141.0.7390.54" },
            ],
        });
        let m = metadata(edge, "141.0.3537.57", Some(&said));
        assert_eq!(m["fullVersionList"], said["fullVersionList"]);

        let stale = metadata(edge, "141.0.3537.71", Some(&said));
        assert_ne!(stale["fullVersionList"], said["fullVersionList"]);
    }

    /// One entry per browser: a machine with an account on Chrome and another
    /// on Edge keeps what both said, and a browser that updated is asked
    /// again rather than handed its old answer.
    #[test]
    fn what_each_browser_said_is_kept_apart() {
        let browser = crate::browser::Browser::at;
        let said = |path: &str, version: &str, n: u64| MachineHints {
            browser: std::path::PathBuf::from(path),
            version: version.to_string(),
            values: json!({ "n": n }),
        };
        let mut known = KnownHints::default();
        known.remember(said("/chrome", "141.0.1", 1));
        known.remember(said("/edge", "141.0.9", 2));
        assert_eq!(
            known.for_browser(&browser("/chrome", "141.0.1")),
            Some(json!({ "n": 1 }))
        );
        assert_eq!(
            known.for_browser(&browser("/edge", "141.0.9")),
            Some(json!({ "n": 2 }))
        );
        assert_eq!(known.for_browser(&browser("/chrome", "142.0.0")), None);

        known.remember(said("/chrome", "142.0.0", 3));
        assert_eq!(known.hints.len(), 2, "the newer answer replaced the older");
        assert_eq!(
            known.for_browser(&browser("/chrome", "142.0.0")),
            Some(json!({ "n": 3 }))
        );
    }

    /// Without the probe, the platform version is left empty rather than
    /// guessed, and the rest still holds.
    #[test]
    fn without_the_probe_nothing_is_invented() {
        let m = metadata(UA, "141.0.7390.54", None);
        assert_eq!(m["platformVersion"], "");
        assert_eq!(m["bitness"], "64");
    }
}
