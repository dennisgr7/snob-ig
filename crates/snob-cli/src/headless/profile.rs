//! The profile a browser runs on, and the session it is handed: what snob
//! writes beside it, and how the cookies it carries are kept the right ones.

use anyhow::Result;
use serde_json::{Value, json};
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
/// **Another account's browser is emptied first.** Cookies and site data both:
/// `mid`, `ig_did` and `datr` name the device, and carrying one account's
/// into another's session is how two accounts come to look like one person's.
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
    /// What the browser said about the machine; see [`super::identity::machine_hints`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<MachineHints>,
}

/// The browser's own description of the machine, kept per browser and major
/// version. Not a secret, and not about any account.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MachineHints {
    pub browser: std::path::PathBuf,
    /// The browser's whole version when it was asked. A new one is asked
    /// again: the full version list changes with every update.
    pub version: String,
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
