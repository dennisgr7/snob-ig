use anyhow::Result;
use snob_store::paths::{self, AppPaths};
use snob_store::secrets::SecretStore;

use crate::cli::LogoutArgs;
use crate::exit::ExitCode;
use crate::ui;

pub fn run(args: LogoutArgs, store: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    // Read through the store, which owns what counts as a stored session and why
    // — a corrupt credential is still a credential. `purge::survey` asks the same
    // question the same way.
    let had_session = store.something_is_stored();
    // Not `?`. A keyring that refuses must not take the browser profile with
    // it: that profile holds a logged-in session too, and `purge` already
    // refuses to let one locked item hold the rest back for exactly this
    // reason. The refusal is carried to the end and returned there.
    let removal = store.delete();

    match (&removal, had_session) {
        (Ok(()), true) => {
            crate::ui::say!("Session deleted.");
            // Worth saying: nothing was closed on Instagram's side, because
            // that would be a write and snob does not write.
            ui::info(
                "The session is still active on Instagram. To really close it, use\n\
                 \"Active sessions\" in the app's settings.",
            );
        }
        (Ok(()), false) => crate::ui::say!("There was no session stored."),
        // Nothing is claimed here. What refused says so itself, printed by
        // `main`, and "the session is still active on Instagram" would read as
        // though the local copy were the part that had gone.
        (Err(_), _) => {}
    }

    let profile = paths.browser_profile();
    let mut profile_failure = None;
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
        // Collected rather than `?`-ed, for the same reason `removal` above is:
        // one refusal must not decide the other. A browser still open on the
        // profile makes this fail, and with `?` here the early return jumped
        // over `removal?` at the end — so a user whose keyring had *also*
        // refused was told about the locked directory and nothing at all about
        // the session, which was still in the keyring. `purge::execute`
        // already collects its failures for exactly this.
        (true, true) => match std::fs::remove_dir_all(&profile) {
            Ok(()) => crate::ui::say!("Browser profile deleted."),
            Err(e) => profile_failure = Some(e),
        },
        (true, false) => crate::ui::say!("There is no browser profile to delete."),
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

    // After the profile, so that one refusal does not decide the other. There
    // is no documented exit code for "a local delete was refused", and neither
    // 3 nor 5 would be true, so this becomes the generic failure.
    //
    // The credential goes first when both refused: a browser profile left
    // behind is a housekeeping failure, and a session left in the keyring is a
    // failure at the only thing this command exists for. The other is still
    // said out loud rather than swallowed.
    if removal.is_err()
        && let Some(io) = &profile_failure
    {
        ui::warn(&format!(
            "{} could not be removed either: {io}",
            profile.display()
        ));
    }
    removal?;
    if let Some(e) = profile_failure {
        return Err(
            anyhow::Error::new(e).context(format!("{} could not be removed", profile.display()))
        );
    }
    Ok(ExitCode::Ok)
}
