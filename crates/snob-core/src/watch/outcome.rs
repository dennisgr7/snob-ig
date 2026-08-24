//! What a run came to, and the one place its tokens are spelled.
//!
//! The vocabulary is shared between three things that must agree: the exit
//! status a script reads, the `outcome` column `watch_runs` keeps, and the
//! `"outcome"` field `--json` prints. It lives here because the store is the
//! boundary the column crosses and the store cannot name `snob-cli`'s
//! `ExitCode` — which is what left the column typed `Option<String>`, written
//! from `as_str()` at one end and compared against string literals at the
//! other.

/// What one run of the monitor came to.
///
/// The same six outcomes `ExitCode` names, without the numbers: a run's outcome
/// is a fact about the run, while which integer the process exits with is a
/// presentation decision and stays in `snob-cli`. `ExitCode::as_str` is this
/// type's `as_str`, reached through `ExitCode::outcome`, so there is one
/// spelling of each token in the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Ok,
    Error,
    /// No session stored, or the one there no longer works.
    NoSession,
    /// Instagram wants the account verified.
    Challenge,
    /// Instagram is throttling requests.
    RateLimited,
    /// The user interrupted the run.
    Interrupted,
}

impl RunOutcome {
    /// Every outcome, so anything that has to walk them cannot walk a shorter
    /// list. `ExitCode::ALL` and `secrets::Kind::ALL` are here for the same
    /// reason.
    pub const ALL: [RunOutcome; 6] = [
        Self::Ok,
        Self::Error,
        Self::NoSession,
        Self::Challenge,
        Self::RateLimited,
        Self::Interrupted,
    ];

    /// The stable token for this outcome.
    ///
    /// The same vocabulary as the table in the README, so a caller reading the
    /// JSON, a caller reading the database and a caller reading `$?` are told
    /// the same thing by the same name. It exists so nothing has to invent
    /// tokens inline, which is how two spellings of one condition get shipped.
    ///
    /// **Frozen**: this string feeds `watch_runs.outcome`, whose CHECK
    /// constraint accepts these six values and nothing else.
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

    /// The outcome a stored token names, or `None` for a token this build does
    /// not write.
    ///
    /// Derived from [`RunOutcome::as_str`] rather than written as a second
    /// match, so the two cannot disagree at all.
    ///
    /// `None` rather than a default, because a token this build does not
    /// recognize came from a newer one, and guessing at what it meant is how a
    /// probe learns to lie. What to do about it is the caller's decision, and
    /// [`RecordedOutcome`] is where that decision is made explicit rather than
    /// left to a comparison that quietly matches nothing.
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|outcome| outcome.as_str() == token)
    }
}

impl std::fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An outcome as a stored row carries it: one this build knows, or the token
/// whatever wrote the row used.
///
/// The unknown half is a variant rather than an absence because the two are
/// different answers. `NULL` means the run recorded no outcome at all; a token
/// nothing here recognizes means a newer build recorded one this one cannot
/// read, and the difference decides what `watch::status::health` should say.
/// Kept as the string it was, so `--json` prints back what is really in the
/// column and a person reading the line is told the word rather than "unknown".
///
/// **Nothing conservative is guessed here.** An unknown token is not `Ok` and
/// is not any of the outcomes that lift by themselves; it is a run this build
/// cannot explain, which is the direction a probe should be wrong in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedOutcome {
    Known(RunOutcome),
    Unknown(String),
}

impl RecordedOutcome {
    /// Reads a stored token. The parse the store does once, at the row.
    pub fn from_token(token: &str) -> Self {
        match RunOutcome::from_token(token) {
            Some(outcome) => Self::Known(outcome),
            None => Self::Unknown(token.to_string()),
        }
    }

    /// The token, whichever half this is. What the column holds and what the
    /// JSON prints.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Known(outcome) => outcome.as_str(),
            Self::Unknown(token) => token,
        }
    }

    /// The outcome this build understands, or `None` for a token it does not.
    pub fn known(&self) -> Option<RunOutcome> {
        match self {
            Self::Known(outcome) => Some(*outcome),
            Self::Unknown(_) => None,
        }
    }
}

impl From<RunOutcome> for RecordedOutcome {
    fn from(outcome: RunOutcome) -> Self {
        Self::Known(outcome)
    }
}

/// So a caller can ask "is this row `ok`?" without unwrapping the half it does
/// not care about — and, unlike the string comparison this replaces, an unknown
/// token is unequal to every outcome rather than to every *spelling*.
impl PartialEq<RunOutcome> for RecordedOutcome {
    fn eq(&self, other: &RunOutcome) -> bool {
        self.known() == Some(*other)
    }
}

impl std::fmt::Display for RecordedOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The token an outcome writes is the token that reads back as it.
    ///
    /// Walked over `ALL` and counted, because a variant dropped from that list
    /// quietly narrows every caller that walks it.
    #[test]
    fn every_outcome_reads_back_from_the_token_it_writes() {
        assert_eq!(RunOutcome::ALL.len(), 6, "an outcome was added or dropped");
        for outcome in RunOutcome::ALL {
            assert_eq!(
                RunOutcome::from_token(outcome.as_str()),
                Some(outcome),
                "{outcome:?} writes {:?} and does not read back from it",
                outcome.as_str()
            );
        }
    }

    /// Two outcomes sharing a token would read back as one of them, and the
    /// round trip above would not notice.
    #[test]
    fn no_two_outcomes_are_spelled_the_same() {
        let mut tokens: Vec<&str> = RunOutcome::ALL.iter().map(|o| o.as_str()).collect();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), RunOutcome::ALL.len(), "a token is shared");
    }

    /// A token this build does not write is kept as itself, and matches no
    /// outcome at all.
    #[test]
    fn a_token_from_another_build_stays_the_token_it_was() {
        let read = RecordedOutcome::from_token("rate-limited");
        assert_eq!(read, RecordedOutcome::Unknown("rate-limited".into()));
        assert_eq!(read.known(), None);
        assert_ne!(read, RunOutcome::RateLimited);
        assert_ne!(read, RunOutcome::Ok);
        assert_eq!(
            read.as_str(),
            "rate-limited",
            "what is printed back is what is really in the column"
        );
    }

    #[test]
    fn a_token_this_build_writes_reads_back_as_the_outcome() {
        let read = RecordedOutcome::from_token("rate_limited");
        assert_eq!(read.known(), Some(RunOutcome::RateLimited));
        assert_eq!(read, RunOutcome::RateLimited);
        assert_eq!(read.to_string(), "rate_limited");
    }
}
