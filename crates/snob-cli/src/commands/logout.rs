use anyhow::Result;
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;

use crate::cli::LogoutArgs;
use crate::exit::ExitCode;
use crate::ui;

pub fn run(args: LogoutArgs, store: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    let had_session = store.load().unwrap_or(None).is_some();
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
