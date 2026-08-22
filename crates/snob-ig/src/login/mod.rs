//! Obtaining the Instagram session.
//!
//! There are two ways, and neither is a programmatic login: the tool never sees
//! or asks for the password. Either the user copies the cookie out of the
//! browser's developer tools, or a browser we launch hands it over through its
//! debugging protocol.
//!
//! The `sessionid` cookie is marked `HttpOnly`, so JavaScript cannot read it.
//! That detail rules out the console-snippet shortcut and is the reason the two
//! ways are the ones they are.

use snob_core::secret::Secret;
use snob_core::session::{Session, SessionError, SessionOrigin};
use snob_core::{EpochMs, Pk};
use thiserror::Error;

use crate::client::IgClient;
use crate::error::IgError;
use crate::pace::Pacer;

#[derive(Debug, Error)]
pub enum LoginError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Instagram(#[from] IgError),
    #[error(
        "the browser handed over cookies for two different accounts: the sessionid belongs to {from_sessionid} and ds_user_id says {from_cookie}"
    )]
    MismatchedAccount {
        from_sessionid: Pk,
        from_cookie: String,
    },
}

impl LoginError {
    /// The Instagram error behind this one, if there is one.
    ///
    /// Needed explicitly because `#[error(transparent)]` delegates `source()`
    /// to the inner error rather than exposing it, so walking the cause chain
    /// does not find it.
    pub fn as_instagram(&self) -> Option<&IgError> {
        match self {
            Self::Instagram(e) => Some(e),
            Self::Session(_) | Self::MismatchedAccount { .. } => None,
        }
    }
}

/// The cookies a browser hands over for `instagram.com`.
///
/// Only `sessionid` is required. The rest travel with every request the browser
/// makes, and sending them too keeps our requests looking like the ones the
/// session was created by.
///
/// The credential fields are [`Secret`], so a derived `Debug` is safe: they
/// print as `<hidden>` and clear themselves when dropped.
#[derive(Debug, Default, Clone)]
pub struct BrowserCookies {
    pub sessionid: Secret,
    pub ds_user_id: Option<String>,
    pub csrftoken: Option<Secret>,
    pub mid: Option<String>,
    pub ig_did: Option<String>,
}

/// Result of validating a freshly obtained session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationOutcome {
    /// Instagram answered correctly.
    Confirmed,
    /// It could not be confirmed because of a transient problem, but the
    /// session is stored anyway.
    Unconfirmed,
    /// Nothing was asked because the account is in cooldown. Storing an
    /// unchecked session beats spending the one request the cooldown exists to
    /// prevent — and a fresh session is the usual reason someone is here.
    Skipped { until_ms: EpochMs },
}

/// Builds a session from what the user pasted.
pub fn session_from_paste(sessionid: &str, user_agent: &str) -> Result<Session, LoginError> {
    Ok(Session::from_sessionid(
        sessionid,
        user_agent,
        SessionOrigin::Paste,
    )?)
}

/// Builds a session from the cookies a browser handed over.
///
/// The account id is still taken from the `sessionid`, exactly as with a paste,
/// so there is one authority on whose session this is. The `ds_user_id` cookie
/// is only used to cross-check it: when a browser is signed into several
/// accounts they can disagree, and storing a session that quietly acts as
/// somebody else is worse than refusing to store one at all.
pub fn session_from_cookies(
    cookies: &BrowserCookies,
    user_agent: &str,
) -> Result<Session, LoginError> {
    let mut session = Session::from_sessionid(
        cookies.sessionid.expose(),
        user_agent,
        SessionOrigin::Browser,
    )?;

    if let Some(from_cookie) = &cookies.ds_user_id
        && from_cookie.trim() != session.ds_user_id.to_string()
    {
        return Err(LoginError::MismatchedAccount {
            from_sessionid: session.ds_user_id,
            from_cookie: from_cookie.trim().to_string(),
        });
    }

    session.csrftoken = cookies.csrftoken.clone();
    session.mid = cookies.mid.clone();
    session.ig_did = cookies.ig_did.clone();
    Ok(session)
}

/// Checks against Instagram that the session works and, along the way, fills in
/// the username if it can.
///
/// On throttling or a network failure the session is taken as good but
/// unconfirmed: the cookie is almost certainly fine and Instagram is merely
/// throttling. Forcing a repeat login over a transient error makes the
/// throttling worse on top of being annoying. On an expired session, a
/// mismatched User-Agent or a pending check, the error propagates and nothing
/// is stored.
pub async fn validate(
    session: &mut Session,
    pacer: Pacer,
) -> Result<ValidationOutcome, LoginError> {
    if let Some(until_ms) = pacer.cooldown()? {
        return Ok(ValidationOutcome::Skipped { until_ms });
    }

    let client = IgClient::new(session.clone(), pacer)?;

    match client.validate().await {
        Ok(()) => {
            session.mark_validated();
        }
        Err(e) if e.invalidates_session() => return Err(e.into()),
        Err(e) if e.is_login_tolerable() => {
            tracing::warn!(error = %e, "could not confirm the session");
            return Ok(ValidationOutcome::Unconfirmed);
        }
        Err(e) => return Err(e.into()),
    }

    // The username is cosmetic: failing to get it does not invalidate anything.
    // A push-back on it is not cosmetic, though. `classify_and_record` has
    // just written a cooldown, so the next command will refuse for half an
    // hour -- and at `debug` the person had been told their login succeeded
    // and nothing else. The session is still good; the warning is about what
    // Instagram said on the way.
    if session.username.is_none() {
        match client.resolve_username(session.ds_user_id).await {
            Ok(name) => session.username = name,
            Err(e) if e.is_push_back() => {
                tracing::warn!(error = %e, "the session works, but Instagram pushed back on the follow-up request; the account is in cooldown");
            }
            Err(e) => tracing::debug!(error = %e, "could not resolve the username"),
        }
    }

    Ok(ValidationOutcome::Confirmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

    #[test]
    fn a_pasted_session_remembers_where_it_came_from() {
        let s = session_from_paste("42%3AAbCd%3A20", UA).unwrap();
        assert_eq!(s.origin, SessionOrigin::Paste);
        assert_eq!(s.ds_user_id, Pk::new(42));
        assert!(s.validated_at.is_none());
    }

    fn cookies() -> BrowserCookies {
        BrowserCookies {
            sessionid: "42%3AAbCd%3A20".into(),
            ds_user_id: Some("42".into()),
            csrftoken: Some("tok".into()),
            mid: Some("mid".into()),
            ig_did: Some("did".into()),
        }
    }

    #[test]
    fn a_browser_session_carries_every_cookie_it_was_given() {
        let s = session_from_cookies(&cookies(), UA).unwrap();
        assert_eq!(s.origin, SessionOrigin::Browser);
        assert_eq!(s.ds_user_id, Pk::new(42));
        assert_eq!(s.csrftoken.as_ref().map(Secret::expose), Some("tok"));
        assert_eq!(
            s.cookie_header().as_str(),
            "sessionid=42%3AAbCd%3A20; ds_user_id=42; csrftoken=tok; mid=mid; ig_did=did"
        );
    }

    /// A browser signed into two accounts can hand over a mismatched pair.
    /// Storing it would mean acting as somebody else without saying so.
    #[test]
    fn cookies_describing_two_accounts_are_refused() {
        let mismatched = BrowserCookies {
            ds_user_id: Some("99".into()),
            ..cookies()
        };
        assert!(matches!(
            session_from_cookies(&mismatched, UA),
            Err(LoginError::MismatchedAccount { .. })
        ));
    }

    /// The cookie is a cross-check, not a source. Without it the sessionid
    /// still says whose session this is.
    #[test]
    fn a_missing_ds_user_id_is_not_a_problem() {
        let bare = BrowserCookies {
            sessionid: "42%3AAbCd%3A20".into(),
            ..BrowserCookies::default()
        };
        assert_eq!(
            session_from_cookies(&bare, UA).unwrap().ds_user_id,
            Pk::new(42)
        );
    }

    #[test]
    fn an_unreadable_sessionid_is_rejected_before_touching_the_network() {
        assert!(matches!(
            session_from_paste("not-a-sessionid", UA),
            Err(LoginError::Session(_))
        ));
    }
}
