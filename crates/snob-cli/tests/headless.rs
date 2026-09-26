//! The headless transport, driven end to end: the real binary, a real browser,
//! and a fake Instagram served locally.
//!
//! Every request to Instagram leaves from a browser tab now, and nothing else
//! in the suite sends one that way — `sandbox.rs` reaches its mock servers with
//! `reqwest`, on purpose, so it runs on a machine with no browser. This is the
//! one place the path users actually take is exercised: the tab is opened, the
//! pasted session is written into the browser, the page load sets a CSRF
//! cookie, and the API calls go out with the browser's own cookie jar and
//! headers. `--through-the-browser` is what points that path at a local
//! server; like `--ig-base-url` it exists only in a testing build.
//!
//! **Skipped when no browser will start**, for the reason `browser_pipe.rs`
//! gives: a machine where Chromium cannot run is a machine where snob cannot
//! either, and a test that cannot run there should say nothing rather than
//! something false. It asks the browser directly before anything else, so a
//! browser that starts and then misbehaves under snob is a failure, not a
//! skip.
#![cfg(feature = "testing")]

use std::path::Path;
use std::process::{Command, Output};

use snob_cli::{browser, cdp};
use snob_ig::pace::CancelToken;
use snob_store::paths::AppPaths;
use wiremock::matchers::{method, path as url_path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SESSIONID: &str = "42%3Aheadless%3A17";

/// What the fake Instagram sets on the page load. A pasted session carries no
/// CSRF token at all, so seeing this one on an API call means the call went
/// out with the browser's own cookie jar — `reqwest` never learns it.
const SERVED_CSRF: &str = "set-by-the-page-load";

/// Whether a browser will start headless on this machine at all.
async fn a_browser_starts() -> bool {
    let Some(found) = browser::detect() else {
        eprintln!("no browser installed; skipping");
        return false;
    };
    let temporary = tempfile::tempdir().expect("a temporary directory");
    let paths = AppPaths::rooted_at(temporary.path());
    let cancel = CancelToken::default();
    let flags = ["--headless=new".to_string()];
    let started =
        match cdp::launch_headless(&found, &paths.browser_profile(), &flags, &cancel).await {
            Ok(launched) => cdp::Cdp::connect(launched, &cancel).await,
            Err(e) => Err(e),
        };
    match started {
        Ok(cdp) => {
            cdp.close().await;
            true
        }
        Err(e) => {
            eprintln!("the browser found here will not start ({e}); skipping");
            false
        }
    }
}

async fn fake_instagram() -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .append_header("Set-Cookie", format!("csrftoken={SERVED_CSRF}; Path=/"))
                .append_header("Set-Cookie", "datr=device-of-the-first-account; Path=/")
                .set_body_string("<!doctype html><title>Instagram</title><p>fake</p>"),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v1/friendships/\d+/following/$"))
        .and(query_param("count", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ig-set-www-claim", "hmac.from-the-server")
                .set_body_string(r#"{"users":[]}"#),
        )
        .mount(&server)
        .await;

    for pk in [42, 43] {
        Mock::given(method("GET"))
            .and(url_path(format!("/api/v1/users/{pk}/info/")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"user":{{"pk":{pk},"username":"user{pk}","full_name":"U"}}}}"#
            )))
            .mount(&server)
            .await;
    }

    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"user":{"id":"42","username":"me",
                "edge_followed_by":{"count":2},"edge_follow":{"count":2}}}}"#,
        ))
        .mount(&server)
        .await;

    for kind in ["followers", "following"] {
        Mock::given(method("GET"))
            .and(path_regex(format!(r"^/api/v1/friendships/\d+/{kind}/$")))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"users":[{"pk":1000,"username":"user0"},{"pk":1001,"username":"user1"}]}"#,
            ))
            .mount(&server)
            .await;
    }

    server
}

fn snob(root: &Path, instagram: &MockServer, args: &[&str], typed: Option<&str>) -> Output {
    use std::io::Write;

    let mut child = Command::new(env!("CARGO_BIN_EXE_snob"))
        .arg("--sandbox-root")
        .arg(root)
        .arg("--ig-base-url")
        .arg(instagram.uri())
        .arg("--through-the-browser")
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("SNOB_NO_BROWSER")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");
    if let Some(typed) = typed {
        child
            .stdin
            .as_mut()
            .expect("stdin was piped")
            .write_all(typed.as_bytes())
            .expect("the binary reads what it is given");
    }
    drop(child.stdin.take());
    child.wait_with_output().expect("the binary finishes")
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers.get(name).and_then(|v| v.to_str().ok())
}

/// The whole promise of the transport, asked of what the server received.
#[tokio::test]
async fn every_request_leaves_from_the_browser() {
    if !a_browser_starts().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram().await;

    let out = snob(
        tmp.path(),
        &instagram,
        &["login", "--paste"],
        Some(&format!("{SESSIONID}\n")),
    );
    assert!(out.status.success(), "the login failed: {}", said(&out));

    let out = snob(
        tmp.path(),
        &instagram,
        &["unfollowers", "--format", "json"],
        None,
    );
    assert!(out.status.success(), "the crossing failed: {}", said(&out));

    let received = instagram.received_requests().await.unwrap_or_default();
    let first_api = received
        .iter()
        .position(|r| r.url.path().starts_with("/api/"))
        .expect("the API was called");
    assert!(
        received[..first_api].iter().any(|r| r.url.path() == "/"),
        "the tab opens the site before it asks the API anything"
    );

    for request in received
        .iter()
        .filter(|r| r.url.path().starts_with("/api/"))
    {
        let what = request.url.to_string();
        let agent = header(request, "user-agent").unwrap_or_default();
        assert!(!agent.is_empty(), "{what}: no User-Agent");
        assert!(!agent.contains("Headless"), "{what}: {agent}");
        let brands = header(request, "sec-ch-ua").unwrap_or_default();
        assert!(!brands.contains("Headless"), "{what}: {brands}");

        // Only the browser's own jar holds the token the page load set.
        assert_eq!(
            header(request, "x-csrftoken"),
            Some(SERVED_CSRF),
            "{what}: the CSRF token is not the browser's"
        );
        let cookie = header(request, "cookie").unwrap_or_default();
        assert!(cookie.contains("sessionid="), "{what}: no session cookie");
        assert!(
            cookie.contains(&format!("csrftoken={SERVED_CSRF}")),
            "{what}: the cookie jar is not the browser's: {cookie}"
        );

        // Set by the browser's network stack on a fetch, and by nothing else
        // snob sends from here.
        assert_eq!(header(request, "sec-fetch-mode"), Some("cors"), "{what}");
        assert_eq!(
            header(request, "sec-fetch-site"),
            Some("same-origin"),
            "{what}"
        );
    }

    // The claim the server handed out comes back on the calls after it.
    let claimed = received
        .iter()
        .filter(|r| r.url.path().starts_with("/api/"))
        .skip_while(|r| header(r, "x-ig-www-claim") != Some("hmac.from-the-server"))
        .count();
    assert!(claimed > 0, "the claim the server set was never sent back");
}

/// The session cookie the server saw on the last API call it was sent.
async fn last_session_seen(instagram: &MockServer) -> String {
    let received = instagram.received_requests().await.unwrap_or_default();
    let last = received
        .iter()
        .rev()
        .find(|r| r.url.path().starts_with("/api/"))
        .expect("the API was called");
    header(last, "cookie").unwrap_or_default().to_string()
}

/// A login is authoritative: a fresh paste for the same account reaches the
/// browser, which already carries the old session and used to keep it.
#[tokio::test]
async fn a_new_login_for_the_same_account_reaches_the_browser() {
    if !a_browser_starts().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram().await;

    for token in ["first", "second"] {
        let sessionid = format!("42%3A{token}%3A17");
        let out = snob(
            tmp.path(),
            &instagram,
            &["login", "--paste"],
            Some(&format!("{sessionid}\n")),
        );
        assert!(out.status.success(), "{token}: {}", said(&out));
        let cookie = last_session_seen(&instagram).await;
        assert!(
            cookie.contains(&format!("sessionid={sessionid}")),
            "{token}: the browser sent {cookie}"
        );
    }
}

/// Another account's browser is emptied before this one's session goes in:
/// the device cookie the first account's page load set does not travel with
/// the second account's requests.
#[tokio::test]
async fn a_second_account_does_not_inherit_the_first_ones_device() {
    if !a_browser_starts().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram().await;

    let out = snob(
        tmp.path(),
        &instagram,
        &["login", "--paste"],
        Some("42%3Afirst%3A17\n"),
    );
    assert!(out.status.success(), "{}", said(&out));
    assert!(
        last_session_seen(&instagram)
            .await
            .contains("datr=device-of-the-first-account"),
        "the first account's page load set the device cookie"
    );

    instagram.reset().await;
    let second = fake_instagram().await;
    let out = snob(
        tmp.path(),
        &second,
        &["login", "--paste"],
        Some("43%3Asecond%3A17\n"),
    );
    assert!(out.status.success(), "{}", said(&out));

    // The page load on the second server sets a datr of its own; what must not
    // happen is the first one's arriving at the API before that.
    let received = second.received_requests().await.unwrap_or_default();
    let first_load = received
        .iter()
        .find(|r| r.url.path() == "/")
        .expect("the tab opened the site");
    let carried = header(first_load, "cookie").unwrap_or_default();
    assert!(
        carried.contains("sessionid=43%3Asecond%3A17"),
        "the second account's session went in: {carried}"
    );
    assert!(
        !carried.contains("42%3A") && !carried.contains("device-of-the-first-account"),
        "the first account's cookies went with the second: {carried}"
    );
}

/// What the page and its service worker give away, asked of them from inside.
///
/// The fake site's own page reports what a script can read about the browser
/// it runs in, and registers a service worker that reports its User-Agent.
/// Each was measured wrong on a headless Chromium before the engine covered
/// it: no focus, a screen with no taskbar, no mouse, and a service worker
/// calling itself `HeadlessChrome` — a target of its own that the tab's
/// override never reached.
#[tokio::test]
async fn the_page_and_its_worker_look_like_a_desktop_browser() {
    if !a_browser_starts().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram().await;
    // Mounted first, so it wins over the plain page.
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(
            // `set_body_raw`, because `set_body_string` makes it text/plain
            // whatever the header says, and a page shown as text runs nothing.
            ResponseTemplate::new(200)
                .append_header("Set-Cookie", format!("csrftoken={SERVED_CSRF}; Path=/"))
                .set_body_raw(
                    "<!doctype html><title>Instagram</title><script>
                    navigator.serviceWorker.register('/sw.js');
                    fetch('/page-probe?focus=' + document.hasFocus()
                      + '&avail=' + (screen.availHeight < screen.height)
                      + '&pointer=' + matchMedia('(pointer: fine)').matches
                      + '&hover=' + matchMedia('(hover: hover)').matches
                      + '&webdriver=' + navigator.webdriver);
                    </script>",
                    "text/html",
                ),
        )
        .with_priority(1)
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/sw.js"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "self.addEventListener('install', e => e.waitUntil(fetch('/sw-probe')));",
            "text/javascript",
        ))
        .mount(&instagram)
        .await;
    for probe in ["/page-probe", "/sw-probe"] {
        Mock::given(method("GET"))
            .and(url_path(probe))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&instagram)
            .await;
    }

    let out = snob(
        tmp.path(),
        &instagram,
        &["login", "--paste"],
        Some(&format!("{SESSIONID}\n")),
    );
    assert!(out.status.success(), "the login failed: {}", said(&out));
    // A second run, so the worker registered by the first has certainly been
    // installed and has asked for what it asks for.
    let out = snob(tmp.path(), &instagram, &["whoami"], None);
    assert!(out.status.success(), "{}", said(&out));

    let received = instagram.received_requests().await.unwrap_or_default();
    let paths: Vec<String> = received.iter().map(|r| r.url.path().to_string()).collect();
    let page = received
        .iter()
        .find(|r| r.url.path() == "/page-probe")
        .unwrap_or_else(|| panic!("the page ran its script: {paths:?}"));
    let said_by_page: std::collections::HashMap<_, _> = page.url.query_pairs().collect();
    for (what, expected) in [
        ("focus", "true"),
        ("avail", "true"),
        ("pointer", "true"),
        ("hover", "true"),
        ("webdriver", "false"),
    ] {
        assert_eq!(
            said_by_page.get(what).map(|v| v.as_ref()),
            Some(expected),
            "{what}: {}",
            page.url
        );
    }

    let worker = received
        .iter()
        .find(|r| r.url.path() == "/sw-probe")
        .expect("the service worker was installed and asked");
    let agent = header(worker, "user-agent").unwrap_or_default();
    assert!(
        !agent.is_empty() && !agent.contains("Headless"),
        "the service worker names itself: {agent}"
    );
}

/// No video the site loads reaches the page, so none plays.
///
/// The feed plays a video that scrolls into view, muted, on its own, and a
/// play of a reel counts from its start: every run would add plays nobody
/// watched to other people's videos. Both ways a page loads one are tried
/// here — a `<video>` element, and a piece fetched by script the way the
/// site's player does — and the server must never be asked for either.
#[tokio::test]
async fn no_video_the_site_loads_reaches_the_page() {
    if !a_browser_starts().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram().await;
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("Set-Cookie", format!("csrftoken={SERVED_CSRF}; Path=/"))
                .set_body_raw(
                    "<!doctype html><title>Instagram</title>
                    <video muted autoplay playsinline src='/clip.mp4'></video>
                    <script>
                    fetch('/v/t16/piece.mp4?bytestart=0&byteend=999').catch(() => {});
                    fetch('/page-probe').catch(() => {});
                    </script>",
                    "text/html",
                ),
        )
        .with_priority(1)
        .mount(&instagram)
        .await;
    for path in ["/clip.mp4", "/v/t16/piece.mp4", "/page-probe"] {
        Mock::given(method("GET"))
            .and(url_path(path))
            .respond_with(ResponseTemplate::new(200).set_body_string("x"))
            .mount(&instagram)
            .await;
    }

    let out = snob(
        tmp.path(),
        &instagram,
        &["login", "--paste"],
        Some(&format!("{SESSIONID}\n")),
    );
    assert!(out.status.success(), "the login failed: {}", said(&out));

    let paths: Vec<String> = instagram
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(
        paths.iter().any(|p| p == "/page-probe"),
        "the page ran its script, so the check below means something: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with(".mp4")),
        "a video reached the page: {paths:?}"
    );
}

/// A worker the page starts while snob is between requests runs at once.
///
/// The protocol used to be read only while one of snob's own commands waited
/// for its answer, and a target the browser paused in the gap stayed paused
/// until the next command came along: measured, a worker created one second
/// into a six-second gap started 5.1 s late, which is itself a timing tell.
/// The page here starts one during the pause after the site loads, when snob
/// has nothing in flight, and reports how long it took to begin.
#[tokio::test]
async fn a_worker_created_while_snob_is_idle_starts_at_once() {
    if !a_browser_starts().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram().await;
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("Set-Cookie", format!("csrftoken={SERVED_CSRF}; Path=/"))
                .set_body_raw(
                    "<!doctype html><title>Instagram</title><script>
                    setTimeout(() => {
                      const made = Date.now();
                      const code = new Blob(['postMessage(Date.now())'],
                        { type: 'text/javascript' });
                      const worker = new Worker(URL.createObjectURL(code));
                      worker.onmessage = (e) => fetch('/worker-probe?ms=' + (e.data - made));
                    }, 50);
                    </script>",
                    "text/html",
                ),
        )
        .with_priority(1)
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/worker-probe"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&instagram)
        .await;

    let out = snob(
        tmp.path(),
        &instagram,
        &["login", "--paste"],
        Some(&format!("{SESSIONID}\n")),
    );
    assert!(out.status.success(), "the login failed: {}", said(&out));

    let received = instagram.received_requests().await.unwrap_or_default();
    let paths: Vec<String> = received.iter().map(|r| r.url.to_string()).collect();
    let probe = received
        .iter()
        .find(|r| r.url.path() == "/worker-probe")
        .unwrap_or_else(|| panic!("the worker ran and reported: {paths:?}"));
    let late: u64 = probe
        .url
        .query_pairs()
        .find(|(k, _)| k == "ms")
        .and_then(|(_, v)| v.parse().ok())
        .expect("the delay is a number of milliseconds");
    eprintln!("the worker started {late} ms after it was created");
    // Held until the next command, the worker waited out the rest of the
    // page's one-and-a-half-second settling pause: 1,309 ms, measured before
    // this changed, with the worker made 200 ms in. A reader of its own lets
    // it go in about ten milliseconds here; a macOS runner with every test's
    // browser starting at once took 581, so the line sits between the two.
    assert!(
        late < 1_000,
        "the worker started {late} ms after it was created"
    );
}

/// The fake site's page, handing a device cookie of its own to every browser
/// that does not have one yet — the way Meta's `datr` is set on a first visit.
struct HandsOutDevices(std::sync::atomic::AtomicUsize);

impl wiremock::Respond for HandsOutDevices {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let answer = ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/html")
            .append_header("Set-Cookie", format!("csrftoken={SERVED_CSRF}; Path=/"))
            .set_body_string("<!doctype html><title>Instagram</title>");
        if header(request, "cookie").is_some_and(|c| c.contains("datr=")) {
            return answer;
        }
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        // Kept for a year, as Meta's is: a cookie with no lifetime ends with
        // the browser, and a device that does not outlive the run is not one.
        answer.append_header(
            "Set-Cookie",
            format!("datr=device-{n}; Path=/; Max-Age=31536000"),
        )
    }
}

/// The cookies the first page load of a run carried.
async fn first_page_load_carried(instagram: &MockServer, since: usize) -> String {
    let received = instagram.received_requests().await.unwrap_or_default();
    let load = received[since..]
        .iter()
        .find(|r| r.url.path() == "/")
        .expect("the tab opened the site");
    header(load, "cookie").unwrap_or_default().to_string()
}

/// Every account keeps its own device: the browser another account used in
/// between is not this one's, and coming back finds the device it left.
///
/// There used to be one profile, emptied of cookies and site data whenever
/// the account changed, so that two accounts did not look like one person's
/// browser. That threw the device away each time: coming back to the first
/// account came back from a browser Instagram had never seen.
#[tokio::test]
async fn each_account_keeps_a_device_of_its_own() {
    if !a_browser_starts().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram().await;
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(HandsOutDevices(std::sync::atomic::AtomicUsize::new(0)))
        .with_priority(1)
        .mount(&instagram)
        .await;

    let mut seen = 0;
    for (sessionid, carried) in [
        ("42%3Afirst%3A17", None),
        ("43%3Asecond%3A17", None),
        ("42%3Afirst%3A17", Some("datr=device-1")),
    ] {
        let out = snob(
            tmp.path(),
            &instagram,
            &["login", "--paste"],
            Some(&format!("{sessionid}\n")),
        );
        assert!(out.status.success(), "{sessionid}: {}", said(&out));
        let cookies = first_page_load_carried(&instagram, seen).await;
        match carried {
            Some(device) => assert!(
                cookies.contains(device),
                "{sessionid} came back from another device: {cookies}"
            ),
            None => assert!(
                !cookies.contains("datr="),
                "{sessionid} began with somebody's device: {cookies}"
            ),
        }
        seen = instagram
            .received_requests()
            .await
            .unwrap_or_default()
            .len();
    }

    let paths = AppPaths::rooted_at(tmp.path());
    for pk in [42, 43] {
        assert!(
            paths.browser_profile_for(snob_core::Pk::new(pk)).is_dir(),
            "account {pk} has a profile of its own"
        );
    }
}

/// A profile from before they were kept per account moves under the account
/// it holds, and the browser still finds everything in it: the device it was
/// given comes along.
#[tokio::test]
async fn a_profile_from_before_moves_under_its_account() {
    if !a_browser_starts().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram().await;
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(HandsOutDevices(std::sync::atomic::AtomicUsize::new(0)))
        .with_priority(1)
        .mount(&instagram)
        .await;

    let out = snob(
        tmp.path(),
        &instagram,
        &["login", "--paste"],
        Some(&format!("{SESSIONID}\n")),
    );
    assert!(out.status.success(), "{}", said(&out));

    // Laid out the way an earlier version left it: the profile is the
    // directory itself.
    let paths = AppPaths::rooted_at(tmp.path());
    let root = paths.browser_profile();
    let account = paths.browser_profile_for(snob_core::Pk::new(42));
    let aside = tmp.path().join("old-layout");
    std::fs::rename(&account, &aside).unwrap();
    std::fs::remove_dir(&root).unwrap();
    std::fs::rename(&aside, &root).unwrap();
    assert!(root.join("snob-profile.json").is_file());

    let seen = instagram
        .received_requests()
        .await
        .unwrap_or_default()
        .len();
    let out = snob(tmp.path(), &instagram, &["whoami"], None);
    assert!(out.status.success(), "{}", said(&out));
    assert!(account.join("snob-profile.json").is_file());
    assert!(!root.join("snob-profile.json").exists());
    let cookies = first_page_load_carried(&instagram, seen).await;
    assert!(
        cookies.contains("datr=device-1"),
        "the moved profile kept its device: {cookies}"
    );
}
