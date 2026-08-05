//! A string that happens to be a credential.
//!
//! Wrapping it buys two things that discipline was buying before, and
//! discipline only holds until somebody new touches the file:
//!
//! - **It cannot be printed by accident.** `Debug` says `<hidden>`, so a stray
//!   `debug!(?session)` or a `{:?}` in an error message writes nothing useful
//!   to a log. Both `Session` and `BrowserCookies` used to hand-write a `Debug`
//!   impl to get this; now the field carries it and a new field is safe by
//!   construction rather than by remembering.
//! - **It clears itself when dropped.** The bytes do not sit in freed memory
//!   waiting for a core dump, a swap file or a hibernation image to pick them
//!   up.
//!
//! Reading the value is spelled [`Secret::expose`] on purpose. It should be
//! slightly uncomfortable to type, and it should be greppable: every place the
//! credential is actually looked at is one `expose` in the codebase.
//!
//! **What this does not do.** `String` may reallocate as it grows, and a
//! reallocation leaves the old buffer behind untouched — so this only clears
//! what it still owns. Everything here is built once from a value that arrives
//! whole and is never appended to, which is what makes that acceptable. Nor
//! does it follow copies made elsewhere: the moment the cookie is formatted
//! into an HTTP header, the copy inside the HTTP client is beyond reach.

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Serialized as the bare string it wraps, so the stored format is unchanged.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Hands over the plaintext.
    ///
    /// Named to be visible in a search: every call is a place the credential is
    /// genuinely needed, and the list should stay short enough to read.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Never the value. This is the whole point of the type.
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<hidden>")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_value() {
        let secret = Secret::new("71234567890%3AAbCdEfGhIjKl%3A20");
        assert_eq!(format!("{secret:?}"), "<hidden>");
        assert!(!format!("{secret:?}").contains("AbCdEfGh"));
    }

    /// Even nested inside something else, which is how it actually appears.
    #[test]
    fn debug_stays_hidden_inside_a_container() {
        let held = Some(Secret::new("AbCdEfGh"));
        assert!(!format!("{held:?}").contains("AbCdEfGh"));

        let pair = vec![Secret::new("one"), Secret::new("two")];
        assert_eq!(format!("{pair:?}"), "[<hidden>, <hidden>]");
    }

    /// The stored format must not change: it serializes as the bare string,
    /// so a session written before this type existed still reads back.
    #[test]
    fn it_serializes_as_the_plain_string() {
        let secret = Secret::new("value");
        assert_eq!(serde_json::to_string(&secret).unwrap(), r#""value""#);

        let back: Secret = serde_json::from_str(r#""value""#).unwrap();
        assert_eq!(back.expose(), "value");
    }

    #[test]
    fn the_value_is_still_readable_when_asked_for() {
        let secret = Secret::from("abc");
        assert_eq!(secret.expose(), "abc");
        assert!(!secret.is_empty());
        assert!(Secret::new("").is_empty());
    }
}
