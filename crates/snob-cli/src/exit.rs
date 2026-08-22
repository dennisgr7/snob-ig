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
    /// Every code, so anything that has to walk them cannot walk a shorter
    /// list. `secrets::Kind::ALL` is here for the same reason.
    pub const ALL: [ExitCode; 6] = [
        Self::Ok,
        Self::Error,
        Self::NoSession,
        Self::Challenge,
        Self::RateLimited,
        Self::Interrupted,
    ];

    /// The code a stored token names, or `None` for a token this build does not
    /// write.
    ///
    /// The other direction of [`ExitCode::as_str`], and it exists because
    /// `watch_runs.outcome` is read back. `watch::status::health` matched the
    /// literals `"ok"`, `"rate_limited"` and `"interrupted"` inline — which is
    /// precisely what `as_str`'s own doc says this vocabulary exists to stop.
    /// Respell one there and every recorded cooldown falls through to the
    /// failing arm, so `status` exits 1 for a monitor that will resume on its
    /// own; and the fixture those tests build their rows from spelled the same
    /// literals, so the suite would have moved with the defect rather than
    /// caught it.
    ///
    /// Derived from [`ExitCode::as_str`] rather than written as a second match,
    /// so the two cannot disagree at all: there is one spelling of each token in
    /// the program.
    ///
    /// `None` rather than a default, because a token this build does not
    /// recognize came from a newer one, and guessing at what it meant is how a
    /// probe learns to lie. What to do about it is the caller's decision.
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|code| code.as_str() == token)
    }

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

    /// The code for what Instagram said, when a walk stopped because it said
    /// something.
    ///
    /// It has to agree with [`ExitCode::from_stop_reason`], which is the other
    /// road to the same question: the walker records a coarse [`StopReason`] and
    /// keeps the error beside it, and whichever of the two a command happens to
    /// read must not change the answer.
    pub fn from_ig_error(e: &IgError) -> Self {
        match e {
            IgError::SessionExpired | IgError::UserAgentMismatch => Self::NoSession,
            IgError::Challenge { .. } | IgError::Checkpoint { .. } => Self::Challenge,
            // `InCooldown` is the backstop in `Pacer::clear` answering, and it
            // exits the same way the eight explicit gates do. They all reach
            // this code through `report::refuse_in_cooldown` and its neighbors;
            // a run that got past them and was stopped here is the same outcome
            // and must not be told apart by a script reading the code.
            IgError::RateLimited | IgError::FeedbackRequired | IgError::InCooldown { .. } => {
                Self::RateLimited
            }
            // Ctrl+C during the budget's owed wait comes back through the
            // client rather than through the token, so it arrives here as an
            // error — and it is still the user stopping. Without this arm it
            // fell to the generic code, and a walk interrupted while the budget
            // was rationing exited 1 while `from_stop_reason` said 130 for the
            // very same event.
            IgError::Canceled => Self::Interrupted,
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

/// The exit code for a failed run, from whatever in the chain knows it.
///
/// An error that already knows its code wins: it was set by whoever refused
/// the result, which is more specific than anything reconstructed from an
/// Instagram error further down. Here rather than in `main`, because the
/// JSON rendering of a failure names the same code and the two must agree.
pub fn exit_code_for(error: &anyhow::Error) -> ExitCode {
    if let Some(code) = ExitCode::from_chain(error) {
        return code;
    }

    error
        .chain()
        .find_map(|cause| {
            cause
                .downcast_ref::<IgError>()
                .or_else(|| {
                    cause
                        .downcast_ref::<snob_ig::login::LoginError>()
                        .and_then(|e| e.as_instagram())
                })
                .map(ExitCode::from_ig_error)
        })
        .unwrap_or(ExitCode::Error)
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
    #[must_use]
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

    /// The token a code writes is the token that reads back as that code.
    ///
    /// `as_str` exists "so nothing has to invent tokens inline"; `from_token` is
    /// the direction that was missing, and `watch::status::health` had invented
    /// three literals for want of it. Walked over `ALL` and counted, because a
    /// variant dropped from that list quietly narrows every caller that walks it
    /// -- and this is the caller where it is cheapest to notice.
    #[test]
    fn every_exit_code_reads_back_from_the_token_it_writes() {
        assert_eq!(ExitCode::ALL.len(), 6, "a code was added or dropped");
        for code in ExitCode::ALL {
            assert_eq!(
                ExitCode::from_token(code.as_str()),
                Some(code),
                "{code:?} writes {:?} and does not read back from it",
                code.as_str()
            );
        }
        assert_eq!(
            ExitCode::from_token("rate-limited"),
            None,
            "a spelling this build does not write is not a code it knows"
        );
    }

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

    /// The two roads to the same event have to arrive at the same code.
    ///
    /// A walk stopped by Ctrl+C is recorded as `StopReason::Canceled`, which
    /// maps to 130; but a Ctrl+C during the request budget's owed wait comes
    /// back as `IgError::Canceled` instead, and that fell through to the
    /// generic 1. The same key press, two exit codes, depending on whether the
    /// tool happened to be sleeping at the time.
    #[test]
    fn a_cancellation_is_the_users_code_whichever_path_it_arrives_by() {
        assert_eq!(
            ExitCode::from_ig_error(&IgError::Canceled),
            ExitCode::Interrupted
        );
        assert_eq!(
            ExitCode::from_stop_reason(StopReason::Canceled),
            ExitCode::Interrupted
        );
    }

    /// The rest of the mapping, so a later arm cannot be added over one of
    /// these by accident.
    #[test]
    fn what_instagram_said_decides_the_code() {
        assert_eq!(
            ExitCode::from_ig_error(&IgError::SessionExpired),
            ExitCode::NoSession
        );
        assert_eq!(
            ExitCode::from_ig_error(&IgError::Checkpoint { url: None }),
            ExitCode::Challenge
        );
        assert_eq!(
            ExitCode::from_ig_error(&IgError::RateLimited),
            ExitCode::RateLimited
        );
        assert_eq!(
            ExitCode::from_ig_error(&IgError::Decode("not json".into())),
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
