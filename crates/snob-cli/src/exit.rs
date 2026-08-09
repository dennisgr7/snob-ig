//! Exit codes.
//!
//! Defined up front because the v2 service will need to tell "log in again"
//! apart from "wait a while" without parsing message text.

use snob_core::model::StopReason;
use snob_ig::error::IgError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Ok = 0,
    Error = 1,
    /// No session stored, or the one there no longer works.
    NoSession = 3,
    /// Instagram wants the account verified.
    Challenge = 4,
    /// Instagram is throttling requests.
    RateLimited = 5,
    /// The user interrupted the run. 128 + SIGINT, the shell convention.
    Interrupted = 130,
}

impl ExitCode {
    /// The stable token for this code, for machine-readable output.
    ///
    /// The same vocabulary as the table in the README, so a caller reading the
    /// JSON and a caller reading `$?` are told the same thing by the same name.
    /// It exists so nothing has to invent tokens inline, which is how two
    /// spellings of one condition get shipped.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::NoSession => "no_session",
            Self::Challenge => "challenge",
            Self::RateLimited => "rate_limited",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn from_ig_error(e: &IgError) -> Self {
        match e {
            IgError::SessionExpired | IgError::UserAgentMismatch => Self::NoSession,
            IgError::Challenge { .. } | IgError::Checkpoint { .. } => Self::Challenge,
            IgError::RateLimited | IgError::FeedbackRequired => Self::RateLimited,
            _ => Self::Error,
        }
    }

    /// The code for a result that had to be refused because a walk stopped
    /// early for this reason. `PageLimit` maps to `Error` here, unlike in a
    /// plain list: the cap was asked for, but the refused result is still not
    /// delivered.
    pub fn from_stop_reason(reason: StopReason) -> Self {
        match reason {
            StopReason::Canceled => Self::Interrupted,
            StopReason::RateLimit => Self::RateLimited,
            StopReason::SessionInvalid => Self::NoSession,
            StopReason::Completed
            | StopReason::PageLimit
            | StopReason::Truncated
            | StopReason::Network => Self::Error,
        }
    }
}

impl ExitCode {
    /// Digs the code out of an error chain.
    ///
    /// `anyhow` wraps as it goes, so by the time an error reaches `main` the
    /// thing that knew what happened is several layers down. Four places did
    /// this walk by hand, three of them in tests, and a test that reimplements
    /// the lookup is a test that can pass while the real one is broken.
    pub fn from_chain(error: &anyhow::Error) -> Option<Self> {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<ExitError>())
            .map(|e| e.code)
    }
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(code: ExitCode) -> Self {
        std::process::ExitCode::from(code as u8)
    }
}

/// An error that already knows its exit code.
///
/// The walker reports throttling, cancellation and session death as a
/// [`StopReason`] inside an `Ok`, so by the time a command refuses a result
/// there is no [`IgError`] left in the chain for `main` to map. This carries
/// the code instead, keeping "wait a while" and "log in again" tellable apart.
#[derive(Debug)]
pub struct ExitError {
    pub code: ExitCode,
    message: String,
    /// What to do about it, kept apart from what happened.
    ///
    /// They used to be one string with a newline between them, which left the
    /// printer no way to tell the sentence describing the failure from the one
    /// telling the user what to try — so both came out under `error:` and the
    /// advice read as more of the complaint.
    hint: Option<String>,
}

impl ExitError {
    pub fn new(code: ExitCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    /// Adds the advice. A builder rather than a third argument to `new`,
    /// because most of the places that construct one of these have no advice
    /// to give and should not have to say so.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

impl std::fmt::Display for ExitError {
    /// The failure alone. The advice is [`ExitError::hint`], and the printer
    /// puts it back — but anything that only has a `Display`, like an `anyhow`
    /// chain being formatted somewhere else, still reads a complete sentence.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_result_keeps_the_documented_codes() {
        assert_eq!(
            ExitCode::from_stop_reason(StopReason::Canceled),
            ExitCode::Interrupted
        );
        assert_eq!(
            ExitCode::from_stop_reason(StopReason::RateLimit),
            ExitCode::RateLimited
        );
        assert_eq!(
            ExitCode::from_stop_reason(StopReason::SessionInvalid),
            ExitCode::NoSession
        );
        assert_eq!(
            ExitCode::from_stop_reason(StopReason::PageLimit),
            ExitCode::Error
        );
    }

    #[test]
    fn the_code_survives_an_anyhow_chain() {
        let error: anyhow::Error =
            ExitError::new(ExitCode::RateLimited, "refused for the test").into();
        assert_eq!(ExitCode::from_chain(&error), Some(ExitCode::RateLimited));

        // Wrapped in context, which is what really happens on the way up.
        let wrapped = error.context("while doing something else");
        assert_eq!(ExitCode::from_chain(&wrapped), Some(ExitCode::RateLimited));
    }

    /// An error that never carried one has none to give, and `main` falls back
    /// to the generic code rather than inventing a specific one.
    #[test]
    fn an_ordinary_error_carries_no_code() {
        assert_eq!(ExitCode::from_chain(&anyhow::anyhow!("plain")), None);
    }
}
