//! Requests sent from inside a browser page, rather than by this process.
//!
//! **Why this exists.** Everything else in this crate reaches Instagram through
//! `reqwest`: this process's own TLS handshake, its own HTTP/2 settings, the
//! cookies it was handed at login and never updates, and a set of headers
//! computed to agree with a User-Agent this process is not. Every one of those
//! is a way for Instagram to tell that the session a browser created is being
//! used by something that is not that browser — a textbook stolen-session
//! signature. A request sent by `fetch()` from an open instagram.com tab has
//! none of them: the handshake, the cookie jar, the rotating `rur` and
//! `csrftoken`, the client hints and the `Referer` are the browser's own,
//! because the browser is the one sending it.
//!
//! So this crate defines only the shape of such a request and the one place a
//! client picks it up. Launching and driving the browser is `snob-cli`'s
//! (`headless.rs`), which already owns the DevTools pipe for the login; this
//! crate still compiles no browser code at all.
//!
//! The pacing, the budgets, the cooldowns and the classification of every
//! answer are untouched: a request from the page is paid for, charged and read
//! exactly as a `reqwest` one is. Only the last hop changes hands.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use snob_core::session::Session;

/// One request for the page to send.
#[derive(Debug, Clone)]
pub struct PageRequest {
    /// `GET` or `POST`. Nothing else is sent from here either.
    pub method: &'static str,
    /// Absolute, on the page's own origin.
    pub url: String,
    /// Only the headers the site's own script adds. Everything a browser adds
    /// by itself — `Cookie`, `User-Agent`, the client hints, `Sec-Fetch-*`,
    /// `Accept-Encoding`, `Origin` — is the browser's to add, and it does.
    pub headers: Vec<(String, String)>,
    /// The page the request is made from, absolute. `fetch()` sends it as the
    /// `Referer`.
    pub referrer: String,
    /// A form-encoded body, for the two writes.
    pub body: Option<String>,
    /// A navigation rather than a fetch: the tab goes to `url` and the answer
    /// is the document it lands on. Only the page fetch that finds a write's
    /// tokens is one.
    pub navigate: bool,
    /// The most bytes of body worth reading.
    pub cap: u64,
    /// How long the page may take before giving up.
    pub timeout_ms: u64,
}

/// What came back.
#[derive(Debug, Clone, Default)]
pub struct PageResponse {
    pub status: u16,
    /// Lowercase names. `Set-Cookie` is never among them — the browser keeps
    /// it, which is the point.
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// Where the request ended up, after any redirect the browser followed.
    pub url: String,
    pub redirected: bool,
    /// The body was longer than [`PageRequest::cap`] and was not kept.
    pub too_large: bool,
}

impl PageResponse {
    /// A header, by lowercase name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Why a page handed back no answer. Three different things, read three
/// different ways — which is why this is not a string.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PageError {
    /// The request got no answer: the network, or the page's own timeout.
    /// Worth a retry, as the same failure through `reqwest` is.
    #[error("{0}")]
    Unreachable(String),
    /// A write, and the browser holds no CSRF token to send it with.
    #[error("the browser holds no CSRF token to write with")]
    NoCsrfToken,
    /// The browser itself: it would not start, went away, or stopped
    /// answering its protocol.
    #[error("{0}")]
    Browser(String),
}

impl From<PageError> for crate::error::IgError {
    fn from(error: PageError) -> Self {
        match error {
            PageError::Unreachable(why) => Self::Unreachable(why),
            PageError::NoCsrfToken => Self::NoCsrfToken,
            PageError::Browser(why) => Self::Browser(why),
        }
    }
}

/// What sending returns: boxed, because the one implementation is behind a
/// trait object and lives in another crate.
pub type PageFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PageResponse, PageError>> + Send + 'a>>;

/// A browser tab on Instagram that sends what it is given.
pub trait Page: Send + Sync {
    fn send(&self, request: PageRequest) -> PageFuture<'_>;
}

/// Builds the page a client for this session sends through.
///
/// Called once per client; an implementation shares one browser between them,
/// because two browsers on one profile cannot both run.
pub type PageFactory = Arc<dyn Fn(&Session) -> Arc<dyn Page> + Send + Sync>;

/// Where every client built after this is set sends its requests from.
///
/// A process-global for the reason [`super::SANDBOX_BASE`] and `http::TRUST`
/// are: three separate places build a client, one of them inside this crate
/// with no path from the binary's arguments, and the answer is the same for
/// the whole process. Set once from `main`, before any client exists.
static FACTORY: OnceLock<PageFactory> = OnceLock::new();

/// Sends every request of every client built from now on from a browser page.
///
/// `Err` hands the factory back on a second call.
pub fn send_every_request_from(factory: PageFactory) -> Result<(), PageFactory> {
    FACTORY.set(factory)
}

/// The page a client for `session` should use, when one was set up.
pub(crate) fn page_for(session: &Session) -> Option<Arc<dyn Page>> {
    FACTORY.get().map(|factory| factory(session))
}

/// Whether a client pointed somewhere other than Instagram uses the page too.
///
/// Off by default, so a test server is reached the way the rest of the suite
/// reaches it. A testing build turns it on to drive the page path end to end
/// against a fake Instagram served locally.
#[cfg(feature = "testing")]
static OFF_INSTAGRAM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Uses the page even for a client that is not pointed at Instagram.
#[cfg(feature = "testing")]
pub fn use_the_page_off_instagram() {
    OFF_INSTAGRAM.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn used_off_instagram() -> bool {
    #[cfg(feature = "testing")]
    {
        OFF_INSTAGRAM.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(feature = "testing"))]
    {
        false
    }
}
