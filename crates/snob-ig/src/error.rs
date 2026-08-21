//! Client errors and classification of Instagram's responses.
//!
//! Instagram returns uninformative HTTP status codes and puts the real cause in
//! the body. Telling them apart matters because the right response differs in
//! each case: retry after a wait, ask the user to log in again, or stop
//! altogether.

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IgError {
    #[error("the session has expired; run \"snob login\" again")]
    SessionExpired,

    #[error(
        "Instagram rejected the session because the User-Agent does not match the browser that created it"
    )]
    UserAgentMismatch,

    #[error("Instagram is asking for a security check{}", url_suffix(.url))]
    Challenge { url: Option<String> },

    #[error("Instagram has flagged the account with a checkpoint{}", url_suffix(.url))]
    Checkpoint { url: Option<String> },

    #[error("Instagram is throttling requests; this needs a wait before retrying")]
    RateLimited,

    #[error("Instagram has temporarily blocked this action")]
    FeedbackRequired,

    #[error("{}", missing_message(.what))]
    NotFound { what: Option<String> },

    #[error("Instagram answered {status}: {body}")]
    Unexpected { status: u16, body: String },

    #[error("the download exceeds {limit} bytes, so it is not a profile picture")]
    TooLarge { limit: usize },

    #[error("could not parse Instagram's response: {0}")]
    Decode(String),

    #[error("could not consult the request budget: {0}")]
    Budget(String),

    #[error("canceled")]
    Canceled,

    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),
}

fn url_suffix(url: &Option<String>) -> String {
    match url {
        Some(u) => format!(". Open it in a browser to clear it: {u}"),
        None => ". Open Instagram in a browser or the app to clear it".into(),
    }
}

/// A 404 names what was being looked for whenever the caller knows it. Only
/// the endpoints that ask about one account can fill it in.
///
/// Filtered, for the reason [`body_excerpt`] gives: this sentence is printed to a
/// terminal by `report::print_error`, and the name in it is whatever was typed on
/// the command line. `snob followers $'gh\e[2K\e[A'` reaches this arm, so without
/// the filter the refusal erased the line above itself — the same hole every
/// sibling refusal closes at the point it names an account.
fn missing_message(what: &Option<String>) -> String {
    match what {
        Some(name) => format!(
            "the account \"{}\" does not exist",
            snob_core::model::printable(name)
        ),
        None => "Instagram answered 404: what was asked for does not exist".into(),
    }
}

/// How long the account should be left alone after this error, and under what
/// name it gets recorded.
///
/// Lives here, next to [`Reaction`], because every caller that makes a request
/// needs the same answer: the walker reaching it through a stop reason and a
/// single-request command reaching it directly. Two copies of this table is one
/// copy too many — the second would be the one passing a length of zero.
pub fn cooldown_for(error: &IgError) -> Option<(&'static str, std::time::Duration)> {
    use snob_core::store::rate_budget::{
        action_block_cooldown, challenge_cooldown, rate_limit_cooldown,
    };

    match error {
        IgError::FeedbackRequired => Some(("feedback_required", action_block_cooldown())),
        IgError::RateLimited => Some(("rate_limit", rate_limit_cooldown())),
        // The rule says a challenge is a hard stop **and** a cooldown, and only
        // the first half was implemented: nothing was written down, so the next
        // run walked straight back into an account Instagram had just flagged.
        //
        // Its own length, not the action block's, because it is the one cause
        // waiting does not fix — the user clears it by opening the link. Long
        // enough that an unattended rerun cannot knock again, short enough that
        // someone who dealt with it in a minute is not locked out for the day.
        IgError::Challenge { .. } | IgError::Checkpoint { .. } => {
            Some(("challenge", challenge_cooldown()))
        }
        _ => None,
    }
}

/// What the list engine should do about an error.
///
/// It is the single authority on that decision. Retry decisions are not made by
/// combining the individual predicates below: each answers a different question,
/// and mixing them is how a 429 ends up being retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    /// Retry with a growing wait.
    Retry,
    /// Stop and put the account in cooldown.
    Cooldown,
    /// Stop, because retrying within this run cannot help.
    ///
    /// It used to say "without a cooldown", and that stopped being true when a
    /// challenge started recording one. The two questions are separate:
    /// [`Reaction`] answers what the walk in progress should do, and
    /// [`cooldown_for`] answers whether the account is left alone afterwards.
    /// A challenge says yes to both — waiting is not what clears it, but
    /// walking back in before the user has cleared it is exactly what turns a
    /// checkpoint into something longer.
    Abort,
}

impl IgError {
    /// The address that clears a security check, when Instagram gave one.
    ///
    /// Lives here rather than in the command that reports it, next to the
    /// validation that decides whether the address may be repeated at all: the
    /// two variants that can carry one are the two this has to know about, and a
    /// copy of the list in another crate is one that keeps compiling after a
    /// third variant gains a URL and simply stops mentioning it.
    pub fn challenge_url(&self) -> Option<&str> {
        match self {
            Self::Challenge { url } | Self::Checkpoint { url } => url.as_deref(),
            _ => None,
        }
    }

    /// What to do with this error during a walk.
    pub fn reaction(&self) -> Reaction {
        match self {
            // **A refused redirect is not a network failure**, and treating it
            // as one was expensive in the one currency this crate is careful
            // with. `api_policy` refuses a hop that would take an API call off
            // instagram.com by calling `attempt.error(..)`, which reqwest hands
            // back as an ordinary `reqwest::Error` -- so it landed here as
            // `Network`, whose reaction is `Retry`, and the pager sent the same
            // request three more times, each one charged and each one carrying
            // the session cookie, when retrying could not possibly help.
            // Measured at 13 requests on the wire against 5 charged.
            //
            // `is_redirect()` is true exactly for a policy refusal, which is
            // what makes this a one-line question rather than a guess about the
            // message.
            Self::Network(e) if e.is_redirect() => Reaction::Abort,
            Self::Network(_) => Reaction::Retry,
            // A 5xx is the server's problem, not ours.
            Self::Unexpected { status, .. } if *status >= 500 => Reaction::Retry,
            Self::RateLimited | Self::FeedbackRequired => Reaction::Cooldown,
            // `Decode` lands here on purpose: when the body is not the expected
            // JSON it is usually a firewall page, and retrying against a
            // firewall is exactly how you end up in a loop.
            _ => Reaction::Abort,
        }
    }

    /// Whether, **while logging in**, the session is worth storing anyway
    /// rather than making the user repeat the whole process.
    ///
    /// Not a retry criterion: during a walk, throttling is a hard stop. That is
    /// what [`IgError::reaction`] is for.
    pub fn is_login_tolerable(&self) -> bool {
        matches!(self, Self::RateLimited | Self::Network(_))
    }

    /// Whether it invalidates the stored session, and so must not be persisted.
    pub fn invalidates_session(&self) -> bool {
        matches!(
            self,
            Self::SessionExpired
                | Self::UserAgentMismatch
                | Self::Challenge { .. }
                | Self::Checkpoint { .. }
        )
    }
}

/// Shape of Instagram's error bodies. Every field is optional because the
/// response varies by endpoint and by version.
#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    error_type: Option<String>,
    #[serde(default)]
    checkpoint_url: Option<String>,
    #[serde(default)]
    challenge: Option<ChallengeBody>,
    #[serde(default)]
    spam: Option<bool>,
    /// When true, the problem is the session even though the message talks
    /// about waiting a few minutes. Verified against the live API: an invalid
    /// cookie returns exactly that.
    #[serde(default)]
    require_login: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ChallengeBody {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    api_path: Option<String>,
}

/// Whether a body that arrived with a successful status is really a failure.
///
/// Instagram answers `{"status":"fail"}` under a 200 in some cases, so the
/// status code alone is not enough. It has to be **parsed**, not searched:
/// looking for the two substrings anywhere in the text meant that a follower
/// whose full name was `fail` turned a perfectly good page into a stopped walk,
/// and every good answer already carries `"status":"ok"` so the first half of
/// that test was always true.
///
/// Three fields rather than one, because this is the gate in front of
/// [`classify`] and therefore in front of the cooldown. `status` alone meant a
/// 200 carrying `spam: true` without it went to the deserializer instead: the
/// run stopped, which is safe, but nothing was written down, so the next run
/// walked straight back into the same wall. All three are typed fields, so the
/// follower named `fail` stays fixed.
///
/// A body that is not JSON is not a declared failure. Deciding what it is
/// instead is the deserializer's job, and it says so more precisely.
pub fn declares_failure(body: &str) -> bool {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        spam: Option<bool>,
        #[serde(default)]
        require_login: Option<bool>,
    }

    let Ok(envelope) = serde_json::from_str::<Envelope>(body) else {
        return false;
    };
    envelope.status.as_deref() == Some("fail")
        || envelope.spam == Some(true)
        || envelope.require_login == Some(true)
}

/// Translates an Instagram error response into the matching error.
///
/// A pure function: it takes the status code and the raw body. It is the base
/// the rate control is built on, so being testable without touching the network
/// matters.
pub fn classify(status: u16, body: &str) -> IgError {
    let parsed: ErrorBody = serde_json::from_str(body).unwrap_or_default();
    let message = parsed.message.as_deref().unwrap_or("").to_ascii_lowercase();

    // Checked before the message: Instagram returns "Please wait a few minutes"
    // with `require_login: true` when the cookie is no good. Without this
    // branch, a mistyped sessionid would look like transient throttling and we
    // would end up storing a session that does not work.
    if parsed.require_login == Some(true) {
        return IgError::SessionExpired;
    }

    if parsed.spam == Some(true) {
        return IgError::RateLimited;
    }

    if message.contains("useragent mismatch") {
        return IgError::UserAgentMismatch;
    }
    if message.contains("login_required") {
        return IgError::SessionExpired;
    }
    if message.contains("checkpoint_required") {
        return IgError::Checkpoint {
            url: parsed.checkpoint_url.and_then(checked_url),
        };
    }
    if message.contains("challenge_required")
        || parsed.error_type.as_deref() == Some("checkpoint_challenge_required")
    {
        let url = parsed.challenge.as_ref().and_then(|c| {
            c.url
                .clone()
                .and_then(checked_url)
                .or_else(|| c.api_path.clone().and_then(checked_url))
        });
        return IgError::Challenge { url };
    }
    if message.contains("feedback_required") {
        return IgError::FeedbackRequired;
    }
    if message.contains("please wait a few minutes") || message.contains("wait a few minutes") {
        return IgError::RateLimited;
    }

    match status {
        429 => IgError::RateLimited,
        // A 401 or 403 with no recognizable message is almost always a session
        // that no longer works.
        401 | 403 => IgError::SessionExpired,
        // Instagram serves a whole HTML page for a name nobody owns. Without
        // this branch the answer to a typo is three hundred characters of
        // markup.
        404 => IgError::NotFound { what: None },
        _ => IgError::Unexpected {
            status,
            body: body_excerpt(body),
        },
    }
}

/// What an unexpected body contributes to the message. An HTML page is a
/// firewall or an error page: naming it says as much as spilling it over the
/// terminal would.
///
/// What is left goes through the same filter a username does. This excerpt is
/// printed to a terminal by `main`, and a response body is no more trustworthy
/// than a profile field — less, when the thing answering is a captive portal
/// rather than Instagram.
pub(crate) fn body_excerpt(body: &str) -> String {
    if body.trim_start().starts_with('<') {
        return "(an HTML page, not the API's JSON)".into();
    }
    snob_core::model::printable(&body.chars().take(300).collect::<String>())
}

/// Hosts a security check can legitimately live on.
const CHALLENGE_HOSTS: [&str; 3] = ["instagram.com", "www.instagram.com", "i.instagram.com"];

/// The address the user is told to open, if the body named one we would follow
/// ourselves.
///
/// The tool's own words are "Open it in a browser to clear it", so whatever
/// comes back here carries this program's authority. Instagram naming its own
/// challenge page is the only case that is worth anything, and it is also the
/// only case that is safe: a body that names somewhere else gets the generic
/// wording instead, which loses a convenience rather than sending someone to a
/// login form that is not Instagram's.
fn checked_url(value: String) -> Option<String> {
    let absolute = if value.starts_with("http") {
        value
    } else if value.starts_with('/') {
        format!("https://www.instagram.com{value}")
    } else {
        return None;
    };

    let parsed = url::Url::parse(&absolute).ok()?;
    let host = parsed.host_str()?;
    // The **parsed** address is what comes back, not the string that was
    // checked. They are not the same text: the parser skips interior tab,
    // carriage return and newline and percent-encodes C0 controls in the path,
    // so `/challenge/x\x1b[2K\x1b[A` and a host split by a newline both parse
    // to `www.instagram.com`, pass this check, and used to be handed back with
    // their control bytes intact — into a sentence that carries this program's
    // authority, telling somebody to open a link. Erasing the lines above it
    // and offering a different address is the whole attack, and it needs
    // nothing more than a body.
    //
    // Beyond the injection: an address that was validated and an address that
    // is displayed should not be able to differ at all.
    (parsed.scheme() == "https" && CHALLENGE_HOSTS.contains(&host)).then(|| parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expired_session() {
        let e = classify(403, r#"{"message":"login_required","status":"fail"}"#);
        assert!(matches!(e, IgError::SessionExpired));
        assert!(e.invalidates_session());
        assert!(!e.is_login_tolerable());
    }

    #[test]
    fn an_expired_session_with_the_extended_body() {
        let body = r#"{"message":"login_required","error_title":"You've Been Logged Out","error_body":"Please log back in.","logout_reason":8,"status":"fail"}"#;
        assert!(matches!(classify(403, body), IgError::SessionExpired));
    }

    #[test]
    fn a_mismatched_user_agent() {
        let e = classify(401, r#"{"message":"useragent mismatch","status":"fail"}"#);
        assert!(matches!(e, IgError::UserAgentMismatch));
        // Through the function that decides it, rather than a predicate nothing
        // consulted: `reaction` is what the retry loop asks.
        assert_eq!(e.reaction(), Reaction::Abort);
    }

    /// Regression: during a walk, throttling is **never** retried, however
    /// tolerable it may be while logging in. Two different questions, and
    /// confusing them is how you end up hammering Instagram.
    #[test]
    fn throttling_is_never_retried() {
        assert_eq!(IgError::RateLimited.reaction(), Reaction::Cooldown);
        assert!(IgError::RateLimited.is_login_tolerable());
    }

    #[test]
    fn server_errors_are_retried() {
        let e = IgError::Unexpected {
            status: 503,
            body: String::new(),
        };
        assert_eq!(e.reaction(), Reaction::Retry);
    }

    #[test]
    fn an_unreadable_response_is_not_retried() {
        // It is usually a firewall page; pushing on is how you enter a loop.
        assert_eq!(IgError::Decode("oops".into()).reaction(), Reaction::Abort);
    }

    #[test]
    fn a_dead_session_is_not_retried() {
        assert_eq!(IgError::SessionExpired.reaction(), Reaction::Abort);
        assert_eq!(IgError::Challenge { url: None }.reaction(), Reaction::Abort);
    }

    #[test]
    fn a_challenge_with_a_url() {
        let body = r#"{"message":"challenge_required","error_type":"checkpoint_challenge_required","challenge":{"api_path":"/challenge/424242/Ch4LLeNGe1/","url":"https://i.instagram.com/challenge/424242/Ch4LLeNGe1/","native_flow":true,"lock":true,"logout":false},"status":"fail"}"#;
        match classify(400, body) {
            IgError::Challenge { url } => {
                assert_eq!(
                    url.as_deref(),
                    Some("https://i.instagram.com/challenge/424242/Ch4LLeNGe1/")
                );
            }
            other => panic!("expected a challenge, got {other:?}"),
        }
    }

    #[test]
    fn a_checkpoint_with_a_relative_path_is_made_absolute() {
        let body = r#"{"message":"checkpoint_required","checkpoint_url":"/challenge/424242/Ch4LLeNGe2/","lock":false,"status":"fail"}"#;
        match classify(400, body) {
            IgError::Checkpoint { url } => {
                assert_eq!(
                    url.as_deref(),
                    Some("https://www.instagram.com/challenge/424242/Ch4LLeNGe2/")
                );
            }
            other => panic!("expected a checkpoint, got {other:?}"),
        }
    }

    /// Instagram's real answer (July 2026) to an invalid cookie. The message
    /// misleads: it talks about waiting, but `require_login` gives away that
    /// the session is the problem.
    #[test]
    fn an_invalid_cookie_is_not_mistaken_for_throttling() {
        let body = r#"{"message":"Please wait a few minutes before you try again.","require_login":true,"igweb_rollout":true,"status":"fail"}"#;
        let e = classify(401, body);
        assert!(
            matches!(e, IgError::SessionExpired),
            "it was classified as {e:?}"
        );
        assert!(e.invalidates_session());
    }

    /// Regression: this used to be two `contains` over the raw text, so any
    /// account whose name happened to be "fail" stopped the walk. Every good
    /// answer carries `"status":"ok"`, which made the other half always true.
    #[test]
    fn an_account_named_fail_does_not_look_like_one() {
        let page = r#"{"users":[{"pk":1,"username":"fail","full_name":"fail"}],"status":"ok"}"#;
        assert!(!declares_failure(page));

        let quoted =
            r#"{"users":[{"pk":1,"username":"a","full_name":"say \"fail\" again"}],"status":"ok"}"#;
        assert!(!declares_failure(quoted));
    }

    #[test]
    fn a_declared_failure_is_recognized() {
        assert!(declares_failure(
            r#"{"message":"","spam":true,"status":"fail"}"#
        ));
        // Order does not matter: it is a field, not a position.
        assert!(declares_failure(r#"{"status":"fail","message":"x"}"#));
    }

    /// This gate is what stands in front of the cooldown. A throttling answer
    /// that skips it stops the run — which is safe — without writing anything
    /// down, so the next run walks straight back into the same wall.
    #[test]
    fn throttling_without_the_status_field_is_still_a_failure() {
        assert!(declares_failure(r#"{"spam":true}"#));
        assert!(declares_failure(r#"{"require_login":true,"message":"x"}"#));

        // And the flags being false is not a failure.
        assert!(!declares_failure(r#"{"spam":false,"status":"ok"}"#));
    }

    /// A body with no status, or one that is not JSON at all, is not a
    /// declared failure. What it is instead is the deserializer's to say.
    #[test]
    fn a_body_without_a_status_is_not_a_failure() {
        assert!(!declares_failure(r#"{"users":[]}"#));
        assert!(!declares_failure("<html>a firewall page</html>"));
        assert!(!declares_failure(""));
    }

    #[test]
    fn the_spam_flag_means_throttling() {
        let e = classify(400, r#"{"message":"","spam":true,"status":"fail"}"#);
        assert!(matches!(e, IgError::RateLimited));
    }

    #[test]
    fn wait_a_few_minutes_means_throttling() {
        let body =
            r#"{"message":"Please wait a few minutes before you try again.","status":"fail"}"#;
        let e = classify(401, body);
        assert!(matches!(e, IgError::RateLimited));
        assert!(e.is_login_tolerable());
        assert!(!e.invalidates_session());
    }

    /// The three causes that leave a mark, and the shape of each. A challenge
    /// is the one waiting does not fix, so it gets its own much shorter
    /// length rather than the action block's.
    #[test]
    fn every_pushback_records_a_cooldown_of_its_own_size() {
        let length = |e: &IgError| cooldown_for(e).map(|(_, d)| d);

        let throttle = length(&IgError::RateLimited).unwrap();
        let block = length(&IgError::FeedbackRequired).unwrap();
        let challenge = length(&IgError::Challenge { url: None }).unwrap();
        let checkpoint = length(&IgError::Checkpoint { url: None }).unwrap();

        assert_eq!(challenge, checkpoint, "both mean the same thing");
        assert!(
            challenge < throttle,
            "waiting is not what clears a challenge"
        );
        assert!(throttle < block);

        // A dead session is not push-back and must not put the account in
        // cooldown: logging in again is what fixes it.
        assert_eq!(length(&IgError::SessionExpired), None);
        assert_eq!(length(&IgError::Decode("x".into())), None);
    }

    #[test]
    fn feedback_required_stops_the_run() {
        let e = classify(400, r#"{"message":"feedback_required","status":"fail"}"#);
        assert!(matches!(e, IgError::FeedbackRequired));
        assert_eq!(e.reaction(), Reaction::Cooldown);
    }

    #[test]
    fn a_429_means_throttling_even_with_no_body() {
        assert!(matches!(classify(429, ""), IgError::RateLimited));
    }

    #[test]
    fn a_body_that_is_not_json_does_not_blow_up() {
        let e = classify(500, "<html>internal error</html>");
        assert!(matches!(e, IgError::Unexpected { status: 500, .. }));
    }

    #[test]
    fn an_unexpected_body_is_truncated() {
        let long = "x".repeat(5000);
        match classify(500, &long) {
            IgError::Unexpected { body, .. } => assert!(body.chars().count() <= 300),
            other => panic!("expected Unexpected, got {other:?}"),
        }
    }

    /// A username nobody owns answers 404 with a whole web page. What the user
    /// needs to read is that the account is not there.
    #[test]
    fn a_404_means_the_thing_does_not_exist() {
        let e = classify(
            404,
            "<!DOCTYPE html><html><body>Page Not Found</body></html>",
        );
        assert!(matches!(e, IgError::NotFound { what: None }), "{e:?}");
        assert_eq!(e.reaction(), Reaction::Abort);
        let message = e.to_string();
        assert!(!message.contains("<html"), "{message}");
        assert!(message.contains("does not exist"), "{message}");
    }

    #[test]
    fn an_unexpected_html_body_is_named_rather_than_dumped() {
        match classify(500, "<!DOCTYPE html><html>a firewall page</html>") {
            IgError::Unexpected { body, .. } => {
                assert!(!body.contains('<'), "{body}");
                assert!(body.contains("HTML"), "{body}");
            }
            other => panic!("expected Unexpected, got {other:?}"),
        }
    }

    /// An address that passed the check cannot still be carrying what the check
    /// was for.
    ///
    /// `checked_url` validated the parsed URL and returned the raw string, and
    /// those are not the same text: the parser skips interior tab, CR and LF
    /// and percent-encodes C0 controls in the path, so both of these reach
    /// `www.instagram.com`, satisfy the host check, and used to come back with
    /// their control bytes intact — into the one sentence that tells somebody
    /// to open a link on this program's authority.
    #[test]
    fn an_offered_challenge_url_cannot_carry_control_characters() {
        for hostile in [
            // Written as the JSON escapes they arrive as: a raw control
            // character inside a JSON string is not JSON, so a body carrying
            // one never reaches this check at all. These do.
            "/challenge/424242/Ch4LLeNGe1/\\u001b[2K\\u001b[A",
            "https://www.instagram.com\\u000a/challenge/x",
            "https://www.\\u0009instagram.com/challenge/x",
        ] {
            let body = format!(
                r#"{{"message":"challenge_required","challenge":{{"url":"{hostile}"}},"status":"fail"}}"#
            );
            let IgError::Challenge { url } = classify(400, &body) else {
                panic!("expected a challenge for {hostile:?}");
            };
            let Some(url) = url else { continue };
            assert!(
                !url.chars().any(|c| c.is_control()),
                "{url:?} was offered with control characters in it"
            );
            // And the whole rendered sentence, which is what reaches the
            // terminal.
            let message = IgError::Challenge { url: Some(url) }.to_string();
            assert!(
                !message.chars().any(|c| c.is_control()),
                "{message:?} reaches a terminal"
            );
        }
    }

    /// The message says "Open it in a browser to clear it", so the address
    /// carries this tool's authority. A body that names somewhere else loses
    /// the convenience rather than sending someone to a login form that is not
    /// Instagram's.
    #[test]
    fn a_challenge_url_that_is_not_instagram_is_not_offered() {
        for elsewhere in [
            "https://evil.test/challenge/",
            "http://www.instagram.com/challenge/",
            "https://www.instagram.com.evil.test/challenge/",
            "https://evil.test/#www.instagram.com",
            "javascript:alert(1)",
        ] {
            let body = format!(
                r#"{{"message":"challenge_required","challenge":{{"url":"{elsewhere}"}},"status":"fail"}}"#
            );
            match classify(400, &body) {
                IgError::Challenge { url } => {
                    assert_eq!(url, None, "{elsewhere} should not have been offered")
                }
                other => panic!("expected a challenge, got {other:?}"),
            }
        }

        // The generic wording still tells the user what to do.
        let message = IgError::Challenge { url: None }.to_string();
        assert!(message.contains("Open Instagram in a browser"), "{message}");
    }

    /// A response body is no more trustworthy than a profile field, and this
    /// excerpt is printed to a terminal.
    #[test]
    fn an_unexpected_body_cannot_drive_the_terminal() {
        let hostile = format!("{esc}[2K{esc}[A gone", esc = '\x1b');
        match classify(500, &hostile) {
            IgError::Unexpected { body, .. } => assert!(!body.contains('\x1b'), "{body:?}"),
            other => panic!("expected Unexpected, got {other:?}"),
        }
    }

    #[test]
    fn a_404_names_the_account_when_the_caller_knows_it() {
        let e = IgError::NotFound {
            what: Some("someone".into()),
        };
        assert_eq!(e.to_string(), "the account \"someone\" does not exist");
    }
}
