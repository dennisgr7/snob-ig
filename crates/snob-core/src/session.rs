//! The Instagram session the tool reuses.
//!
//! The two credential fields are [`Secret`], which hides itself from `Debug`
//! and clears itself when dropped, so a stray `debug!(?session)` writes nothing
//! useful and the cookie does not linger in freed memory. Anything added here
//! that is itself a credential belongs in the same type.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::Pk;
use crate::secret::Secret;

pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// What the Windows Credential Manager accepts, in bytes.
///
/// `CRED_MAX_CREDENTIAL_BLOB_SIZE` is 5*512, and the keyring backend writes the
/// secret as UTF-16 and checks the length of **that**, so this is a byte
/// ceiling rather than a character one. Above it, the session falls back to the
/// bare essentials rather than failing to store at all.
pub const MAX_KEYRING_SECRET_BYTES: usize = 5 * 512;

/// How much room a string takes in the Windows Credential Manager.
///
/// Two bytes per UTF-16 unit, which is **not** the same as two per character: a
/// character outside the Basic Multilingual Plane is two units, so four bytes.
/// Counting characters instead let a value pass this check and then be refused
/// by Windows with an error that explains nothing.
pub fn keyring_bytes(text: &str) -> usize {
    text.encode_utf16().count() * 2
}

/// Longest accepted User-Agent. Real ones run to about 130 characters.
const MAX_USER_AGENT_CHARS: usize = 512;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("the sessionid is empty")]
    EmptySessionId,
    #[error("unrecognized sessionid format: it should start with your numeric account id")]
    MalformedSessionId,
    #[error("the User-Agent is empty")]
    EmptyUserAgent,
    #[error("that does not look like a browser User-Agent: it should start with \"Mozilla/5.0\"")]
    SuspiciousUserAgent,
    #[error("the User-Agent is too long ({0} characters)")]
    UserAgentTooLong(usize),
    #[error("the stored session uses format {found}, and this version understands {expected}")]
    UnsupportedSchema { found: u32, expected: u32 },
}

/// Where the session came from. Worth storing because it changes the diagnosis
/// when something fails: a `useragent mismatch` on a pasted session almost
/// always means the User-Agent was copied wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    Browser,
    Paste,
}

impl SessionOrigin {
    /// The stable token, shared by the serialized form and machine-readable
    /// output. Kept separate from `Display` so that rewording the human text
    /// cannot break a JSON contract.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Paste => "paste",
        }
    }
}

impl std::fmt::Display for SessionOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Browser => "browser",
            Self::Paste => "manual paste",
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub schema_version: u32,
    pub ds_user_id: Pk,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub sessionid: Secret,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csrftoken: Option<Secret>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ig_did: Option<String>,
    pub user_agent: String,
    /// The user gave this User-Agent explicitly, so nothing may rewrite it.
    ///
    /// Without this flag the refresh below could not tell "we worked this out
    /// from the installed browser" from "the user knows better than we do", and
    /// it would quietly overwrite the second.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user_agent_pinned: bool,
    /// When the installed browser's version was last compared against the
    /// User-Agent. Absent means never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent_checked_at: Option<i64>,
    /// Which browser the User-Agent describes, when one was picked.
    ///
    /// A machine with Chrome and Edge on it has two answers, and the session
    /// belongs to exactly one of them. Without this, the refresh follows
    /// whichever comes first in the preference order and can hand a session
    /// created in Edge the version number of Chrome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    pub origin: SessionOrigin,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validated_at: Option<i64>,
}

impl Session {
    /// Builds a session from the `sessionid` and the User-Agent of the browser
    /// that produced it. The account id is not asked for: it is encoded at the
    /// start of the `sessionid` itself.
    pub fn from_sessionid(
        raw_sessionid: &str,
        user_agent: &str,
        origin: SessionOrigin,
    ) -> Result<Self, SessionError> {
        let sessionid = clean_sessionid(raw_sessionid)?;
        let ds_user_id = extract_pk(&sessionid)?;
        let user_agent = validate_user_agent(user_agent)?;

        Ok(Self {
            schema_version: SESSION_SCHEMA_VERSION,
            ds_user_id,
            username: None,
            sessionid: Secret::new(sessionid),
            csrftoken: None,
            mid: None,
            ig_did: None,
            user_agent,
            user_agent_pinned: false,
            user_agent_checked_at: None,
            browser: None,
            origin,
            created_at: chrono::Utc::now().timestamp(),
            validated_at: None,
        })
    }

    /// Value of the `Cookie` header. The `sessionid` goes exactly as the
    /// browser handed it over, undecoded: the `%3A` travels literally.
    ///
    /// Built into one buffer reserved up front, and handed back in a
    /// [`Zeroizing`] wrapper. This is called on **every** request, so the
    /// obvious spelling — a `Vec` of `format!`s joined at the end — left three
    /// plaintext copies of the live cookie in freed memory per request, which
    /// over a walk of a few thousand accounts is a few hundred of them. That is
    /// exactly the core-dump, swap and hibernation exposure [`Secret`] exists
    /// to prevent; its module doc waives only the copy inside the HTTP client,
    /// not the ones this function makes itself.
    ///
    /// Reserving the capacity is the half that matters. A `String` that grows
    /// leaves each outgrown buffer behind untouched, and `Zeroizing` can only
    /// clear the one it still owns.
    pub fn cookie_header(&self) -> Zeroizing<String> {
        let mut out = String::with_capacity(512);

        out.push_str("sessionid=");
        out.push_str(self.sessionid.expose());
        // The account id is not a credential — it is the first field of the
        // sessionid in plain sight — so a small allocation for it costs
        // nothing worth avoiding.
        out.push_str("; ds_user_id=");
        out.push_str(&self.ds_user_id.to_string());

        // Empty values are left out rather than sent blank. A cookie that is
        // present with no value is not the same as an absent one to Instagram's
        // session and CSRF checks, and it is a shape no browser produces.
        let optional = [
            ("csrftoken", self.csrftoken.as_ref().map(Secret::expose)),
            ("mid", self.mid.as_deref()),
            ("ig_did", self.ig_did.as_deref()),
        ];
        for (name, value) in optional {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                out.push_str("; ");
                out.push_str(name);
                out.push('=');
                out.push_str(value);
            }
        }

        Zeroizing::new(out)
    }

    pub fn check_schema(&self) -> Result<(), SessionError> {
        if self.schema_version > SESSION_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedSchema {
                found: self.schema_version,
                expected: SESSION_SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    pub fn mark_validated(&mut self) {
        self.validated_at = Some(chrono::Utc::now().timestamp());
    }

    /// Reduced version for when the store cannot take the full record. Keeps
    /// only what the session cannot work without.
    pub fn minimal(&self) -> Self {
        Self {
            username: None,
            csrftoken: None,
            mid: None,
            ig_did: None,
            ..self.clone()
        }
    }
}

/// Still hand-written, but only to keep the field list explicit. The two
/// credentials hide themselves: see [`Secret`].
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("schema_version", &self.schema_version)
            .field("ds_user_id", &self.ds_user_id)
            .field("username", &self.username)
            .field("sessionid", &self.sessionid)
            .field("csrftoken", &self.csrftoken)
            .field("mid", &self.mid)
            .field("ig_did", &self.ig_did)
            .field("user_agent", &self.user_agent)
            .field("origin", &self.origin)
            .field("created_at", &self.created_at)
            .field("validated_at", &self.validated_at)
            .finish()
    }
}

/// Tolerates what people actually paste: whitespace, the whole `sessionid=…`
/// pair, and surrounding quotes.
fn clean_sessionid(raw: &str) -> Result<String, SessionError> {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix("sessionid=") {
        s = rest.trim();
    }
    s = s.trim_matches(['"', '\'']).trim();
    s = s.trim_end_matches(';').trim();

    if s.is_empty() {
        return Err(SessionError::EmptySessionId);
    }
    Ok(s.to_string())
}

/// The `sessionid` starts with the account's numeric id, separated by a colon
/// that usually arrives escaped as `%3A`.
fn extract_pk(sessionid: &str) -> Result<Pk, SessionError> {
    let normalized = sessionid.replace("%3A", ":").replace("%3a", ":");
    let head = normalized
        .split(':')
        .next()
        .ok_or(SessionError::MalformedSessionId)?;

    if head.is_empty() || head.len() == normalized.len() {
        return Err(SessionError::MalformedSessionId);
    }

    head.parse::<Pk>()
        .map_err(|_| SessionError::MalformedSessionId)
}

fn validate_user_agent(ua: &str) -> Result<String, SessionError> {
    let ua = ua.trim().trim_matches(['"', '\'']).trim();
    if ua.is_empty() {
        return Err(SessionError::EmptyUserAgent);
    }
    if ua.chars().count() > MAX_USER_AGENT_CHARS {
        return Err(SessionError::UserAgentTooLong(ua.chars().count()));
    }
    if !ua.starts_with("Mozilla/5.0") {
        return Err(SessionError::SuspiciousUserAgent);
    }
    Ok(ua.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
    const SID: &str = "71234567890%3AAbCdEfGhIjKl%3A20%3AAYc123";

    fn session() -> Session {
        Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap()
    }

    #[test]
    fn the_account_id_is_taken_from_the_sessionid() {
        assert_eq!(session().ds_user_id, 71234567890);
    }

    #[test]
    fn an_unescaped_colon_is_accepted() {
        let s = Session::from_sessionid("71234567890:AbCd:20", UA, SessionOrigin::Paste).unwrap();
        assert_eq!(s.ds_user_id, 71234567890);
    }

    #[test]
    fn it_tolerates_how_people_paste() {
        for input in [
            "  71234567890%3AAbCd%3A20  ",
            "sessionid=71234567890%3AAbCd%3A20",
            "\"71234567890%3AAbCd%3A20\"",
            "71234567890%3AAbCd%3A20;",
        ] {
            let s = Session::from_sessionid(input, UA, SessionOrigin::Paste)
                .unwrap_or_else(|e| panic!("\"{input}\" should be accepted, but failed: {e}"));
            assert_eq!(s.ds_user_id, 71234567890);
            assert_eq!(s.sessionid.expose(), "71234567890%3AAbCd%3A20");
        }
    }

    #[test]
    fn an_invalid_sessionid_is_rejected() {
        for input in ["", "   ", "noseparator", "%3Anoprefix", "abc%3Adef"] {
            assert!(
                Session::from_sessionid(input, UA, SessionOrigin::Paste).is_err(),
                "\"{input}\" should be rejected"
            );
        }
    }

    #[test]
    fn a_user_agent_that_is_not_a_browser_is_rejected() {
        assert!(matches!(
            Session::from_sessionid(SID, "curl/8.0", SessionOrigin::Paste),
            Err(SessionError::SuspiciousUserAgent)
        ));
        assert!(matches!(
            Session::from_sessionid(SID, "  ", SessionOrigin::Paste),
            Err(SessionError::EmptyUserAgent)
        ));
        let long = format!("Mozilla/5.0 {}", "x".repeat(600));
        assert!(matches!(
            Session::from_sessionid(SID, &long, SessionOrigin::Paste),
            Err(SessionError::UserAgentTooLong(_))
        ));
    }

    #[test]
    fn the_sessionid_travels_undecoded_in_the_cookie() {
        let header = session().cookie_header();
        assert!(header.contains("sessionid=71234567890%3AAbCdEfGhIjKl%3A20%3AAYc123"));
        assert!(header.contains("ds_user_id=71234567890"));
    }

    #[test]
    fn the_cookie_omits_absent_fields() {
        let header = session().cookie_header();
        assert!(!header.contains("csrftoken"));
        assert!(!header.contains("mid="));
    }

    /// A cookie sent with an empty value is worse than one not sent at all:
    /// Instagram's session and CSRF checks read it as present-but-blank, which
    /// is a shape no browser produces. Absent fields are already left out
    /// above; this pins that a field that exists but is empty is too, so a
    /// later `unwrap_or_default` cannot quietly start emitting `mid=`.
    #[test]
    fn no_cookie_is_ever_sent_with_an_empty_value() {
        let mut s = session();
        s.csrftoken = Some(Secret::new(""));
        s.mid = Some(String::new());
        s.ig_did = Some(String::new());

        let header = s.cookie_header();
        for pair in header.split("; ") {
            let (name, value) = pair.split_once('=').expect("every cookie is a pair");
            assert!(!value.is_empty(), "{name} went out empty: {}", &*header);
        }
    }

    #[test]
    fn debug_does_not_leak_the_credential() {
        let dump = format!("{:?}", session());
        assert!(
            !dump.contains("AbCdEfGhIjKl"),
            "Debug leaked the sessionid: {dump}"
        );
        assert!(dump.contains("<hidden>"));
    }

    #[test]
    fn serialization_round_trip() {
        let original = session();
        let json = serde_json::to_string(&original).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ds_user_id, original.ds_user_id);
        assert_eq!(back.sessionid.expose(), original.sessionid.expose());
        assert_eq!(back.user_agent, original.user_agent);
        assert_eq!(back.origin, original.origin);
    }

    /// The machine-readable token must not follow the human wording around.
    #[test]
    fn the_origin_token_is_independent_of_its_display_text() {
        assert_eq!(SessionOrigin::Paste.as_str(), "paste");
        assert_eq!(SessionOrigin::Paste.to_string(), "manual paste");
        assert_eq!(
            serde_json::to_string(&SessionOrigin::Paste).unwrap(),
            r#""paste""#
        );
    }

    /// Every optional field set at once, because the ceiling only matters for
    /// the fullest session there can be. A test that leaves the newest fields
    /// out stops measuring what it claims to measure the moment one is added.
    #[test]
    fn a_realistic_session_fits_the_windows_keyring() {
        let mut s = session();
        s.username = Some("a_fairly_long_username".into());
        s.csrftoken = Some("x".repeat(32).into());
        s.mid = Some("y".repeat(28));
        s.ig_did = Some("Z".repeat(36));
        s.user_agent_pinned = true;
        s.user_agent_checked_at = Some(1_722_700_000);
        s.browser = Some("Chrome".into());
        s.validated_at = Some(1_722_700_000);
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            keyring_bytes(&json) < MAX_KEYRING_SECRET_BYTES,
            "it takes {} bytes, over the Windows ceiling of {MAX_KEYRING_SECRET_BYTES}",
            keyring_bytes(&json)
        );
    }

    /// Windows measures the secret in UTF-16 bytes, not in characters. A
    /// character outside the Basic Multilingual Plane is four bytes, so
    /// counting characters let a value pass this check and then be refused by
    /// Windows with an error that explains nothing.
    #[test]
    fn the_ceiling_is_measured_the_way_windows_measures_it() {
        assert_eq!(keyring_bytes("abc"), 6);
        // Two UTF-16 units, one character.
        assert_eq!("\u{1f600}".chars().count(), 1);
        assert_eq!(keyring_bytes("\u{1f600}"), 4);

        // A string of exactly the ceiling in characters would have been
        // accepted by the old check and refused by Windows.
        let past_the_ceiling = "\u{1f600}".repeat(MAX_KEYRING_SECRET_BYTES / 4 + 1);
        assert!(past_the_ceiling.chars().count() < MAX_KEYRING_SECRET_BYTES);
        assert!(keyring_bytes(&past_the_ceiling) > MAX_KEYRING_SECRET_BYTES);
    }

    #[test]
    fn a_newer_format_than_we_understand_is_rejected() {
        let mut s = session();
        s.schema_version = SESSION_SCHEMA_VERSION + 1;
        assert!(matches!(
            s.check_schema(),
            Err(SessionError::UnsupportedSchema { .. })
        ));
    }
}
