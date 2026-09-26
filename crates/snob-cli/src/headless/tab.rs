//! The tab requests are sent from: getting it somewhere, and sending from it.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use snob_ig::client::page::{PageError, PageRequest, PageResponse};

use super::{COMMAND_TIMEOUT, LOAD_TIMEOUT, Live, PAGE_WIRE_CAP, SETTLE, origin_of};
use crate::cdp::Cdp;

/// Sends the tab somewhere and waits for it to finish loading.
pub(super) async fn navigate(live: &mut Live, url: &str) -> Result<(), PageError> {
    // The world dies with the document it was made in.
    live.world = None;
    navigate_tab(&live.cdp, &live.tab, url).await?;
    tokio::time::sleep(SETTLE).await;
    Ok(())
}

/// The navigation itself. A page that could not be reached, or never finished
/// loading, is the network's failure and says so; a protocol command that
/// failed is the browser's.
pub(super) async fn navigate_tab(cdp: &Cdp, tab: &str, url: &str) -> Result<(), PageError> {
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
pub(super) async fn navigate_and_read(
    live: &mut Live,
    url: &str,
) -> Result<PageResponse, PageError> {
    let broken = |e: anyhow::Error| PageError::Browser(format!("{e:#}"));
    navigate(live, url).await?;
    let page = evaluate(
        &live.cdp,
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

pub(super) async fn fetch(
    live: &mut Live,
    request: &PageRequest,
) -> Result<PageResponse, PageError> {
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
    let answer = evaluate_in(&live.cdp, &live.tab, Some(world), &expression, timeout)
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
pub(super) async fn evaluate(
    cdp: &Cdp,
    tab: &str,
    expression: &str,
    timeout: Duration,
) -> Result<Value> {
    evaluate_in(cdp, tab, None, expression, timeout).await
}

/// [`evaluate`], in a given world of the tab's document.
pub(super) async fn evaluate_in(
    cdp: &Cdp,
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
pub(super) async fn isolated_world(live: &mut Live) -> Result<i64, PageError> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
