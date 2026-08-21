//! Ctrl+C handling.

use std::sync::OnceLock;

use snob_ig::pace::CancelToken;

/// The process's cancellation token. See [`install`].
static TOKEN: OnceLock<CancelToken> = OnceLock::new();

/// Installs the handler and returns the cancellation token.
///
/// The double-press pattern is required because `tokio::signal::ctrl_c()`
/// **permanently** disables the process default: from the first call onwards, a
/// Ctrl+C no longer kills the program. If the orderly stop ever got stuck, the
/// user would have no way out.
///
/// **Installed once per process, not once per call.** Interrupting is a
/// property of the process, and this used to spawn a fresh listener and hand
/// back a fresh token every time it was asked. One command per process made
/// that indistinguishable from correct. `snob watch` is not one command per
/// process: it opens an `App` per tick — so that each tick picks up a rotated
/// session and holds no SQLite connection while it sleeps — and the old
/// behavior would have left one listener per tick alive, thousands of them
/// after a week, with the signal going to whichever won the race and canceling
/// a token that nothing was watching. The walk would have carried on.
pub fn install() -> CancelToken {
    TOKEN
        .get_or_init(|| {
            let token = CancelToken::default();
            let copy = token.clone();

            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_err() {
                    return;
                }
                eprintln!(
                    "\nStopping and saving what has been fetched... (Ctrl+C again to quit now)"
                );
                copy.cancel();

                if tokio::signal::ctrl_c().await.is_ok() {
                    eprintln!("\nForced exit.");
                    // Nothing below this runs a destructor, so a browser started
                    // for a login would otherwise be left alive with its
                    // debugging port open, and a cursor hidden by the login menu
                    // would stay hidden for the rest of the user's shell session.
                    crate::cdp::kill_launched();
                    crate::ui::restore_terminal();
                    std::process::exit(130);
                }
            });

            token
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two runs in one process share one token, so a Ctrl+C reaches whatever is
    /// running now rather than a token from a tick that finished hours ago.
    #[tokio::test]
    async fn installing_twice_hands_back_the_same_token() {
        let first = install();
        let second = install();

        assert!(!second.is_canceled());
        first.cancel();
        assert!(
            second.is_canceled(),
            "the second caller must be watching the token the handler cancels"
        );
    }
}
