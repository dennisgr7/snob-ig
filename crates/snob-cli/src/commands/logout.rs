use anyhow::Result;
use snob_core::paths::{self, AppPaths};
use snob_core::secrets::SecretStore;

use crate::cli::LogoutArgs;
use crate::exit::ExitCode;
use crate::ui;

pub fn run(args: LogoutArgs, store: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    // Anything but a clean "nothing there" counts as a session. A stored
    // credential too corrupt to parse is still a credential on the disk, and
    // reporting "there was no session" while deleting one also skips the line
    // about it still being live on Instagram — which is exactly when the user
    // needs it. `purge::survey` already reads it this way and has a test
    // forbidding the other.
    let had_session = !matches!(store.load(), Ok(None));
    store.delete()?;

    if had_session {
        println!("Session deleted.");
        // Worth saying: nothing was closed on Instagram's side, because that
        // would be a write and snob does not write.
        ui::info(
            "The session is still active on Instagram. To really close it, use\n\
             \"Active sessions\" in the app's settings.",
        );
    } else {
        println!("There was no session stored.");
    }

    let profile = paths.browser_profile();
    match (args.purge_profile, profile.exists()) {
        // The flag was typed on purpose, so it is not second-guessed: asking
        // for confirmation would also make it a no-op in a script, where
        // `confirm` answers with its default and nothing gets deleted.
        // Guarded like every other recursive delete in the tool. The path
        // comes from `directories` rather than from anything typed, but that
        // is exactly the case the guard is for: a `ProjectDirs` that resolved
        // oddly is what turns "remove the browser profile" into something far
        // worse, and `purge` treats this check as mandatory.
        (true, true) if !paths::is_safe_to_remove(&profile) => {
            ui::warn(&format!(
                "{} is too close to the root to remove; delete it by hand",
                profile.display()
            ));
        }
        (true, true) => {
            std::fs::remove_dir_all(&profile)?;
            println!("Browser profile deleted.");
        }
        (true, false) => println!("There is no browser profile to delete."),
        // Deleting the stored session leaves the browser one behind, and
        // someone who just ran logout reasonably believes the credential is
        // gone from their machine. It is not.
        (false, true) => ui::info(&format!(
            "A browser profile at {} still holds a logged-in session.\n\
             Run \"snob logout --purge-profile\" to remove that too.",
            profile.display()
        )),
        (false, false) => {}
    }

    Ok(ExitCode::Ok)
}
