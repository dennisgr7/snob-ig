//! Ctrl+C handling.

use snob_ig::pace::CancelToken;

/// Installs the handler and returns the cancellation token.
///
/// The double-press pattern is required because `tokio::signal::ctrl_c()`
/// **permanently** disables the process default: from the first call onwards, a
/// Ctrl+C no longer kills the program. If the orderly stop ever got stuck, the
/// user would have no way out.
pub fn install() -> CancelToken {
    let token = CancelToken::default();
    let copy = token.clone();

    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        eprintln!("\nStopping and saving what has been fetched... (Ctrl+C again to quit now)");
        copy.cancel();

        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nForced exit.");
            // Nothing below this runs a destructor, so a browser started for a
            // login would otherwise be left alive with its debugging port open,
            // and a cursor hidden by the login menu would stay hidden for the
            // rest of the user's shell session.
            crate::cdp::kill_launched();
            crate::ui::restore_terminal();
            std::process::exit(130);
        }
    });

    token
}
