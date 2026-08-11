//! Removes everything the tool has stored on this computer.
//!
//! This exists because no package manager can do it. `winget uninstall`,
//! `brew uninstall` and `apt remove` take the binary away and stop there: the
//! session, the database and the browser profile live in the user's own
//! directories, which the package never owned and therefore never deletes. The
//! first of those is a working Instagram credential, and leaving one behind on
//! a machine whose owner has just decided to get rid of the tool is the failure
//! this command exists to prevent.
//!
//! **The binary itself is not removed.** A running process cannot reliably
//! delete its own executable on Windows, and a binary that deleted itself out
//! from under `winget` or `apt` would leave that manager reporting a version
//! that is no longer there. The command says where the file is and lets
//! whatever installed it take it away.

use std::path::{Path, PathBuf};

use anyhow::Result;
use snob_core::paths::{self, AppPaths};
use snob_core::secrets::SecretStore;

use crate::cli::PurgeArgs;
use crate::exit::{ExitCode, ExitError};
use crate::ui;

/// What a purge would remove, worked out before anything is deleted.
///
/// Built whole and shown before the question is asked. This is the only command
/// in the tool that destroys data the user cannot get back, and "yes" to a list
/// of paths is a different answer from "yes" to a warning.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Whether there is a stored session to delete.
    pub session: bool,
    /// Directories to remove, whole.
    pub directories: Vec<PathBuf>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        !self.session && self.directories.is_empty()
    }

    /// One line per thing that will go, for the user to read before answering.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.session {
            // Deliberately not named after one backend. The session is deleted
            // from the keyring *and* from the fallback file whichever of them
            // this run would have written to, so naming either would be a
            // half-truth in the one message that has to be exact.
            lines.push("the stored session, from the system keyring and from disk".to_string());
        }
        lines.extend(self.directories.iter().map(|d| d.display().to_string()));
        lines
    }
}

/// Looks at the machine and reports what there is to remove.
pub fn survey(store: &SecretStore, app_paths: &AppPaths) -> Plan {
    Plan {
        // `Ok(None)` is the only answer that means there is nothing there. A
        // session too corrupt to parse is still a session on this disk, and
        // this command's promise is that afterwards there is none.
        session: !matches!(store.load(), Ok(None)),
        directories: app_paths
            .owned_dirs()
            .into_iter()
            .filter(|dir| dir.exists())
            .filter(|dir| {
                let safe = paths::is_safe_to_remove(dir);
                if !safe {
                    ui::warn(&format!(
                        "not removing {}: it is not where this tool's data belongs",
                        dir.display()
                    ));
                }
                safe
            })
            .collect(),
    }
}

/// Something that would not go, and why.
///
/// Collected rather than returned as an error at the first failure. The likely
/// one is a browser profile still locked by a browser that has not quit, and
/// letting that end the run would leave the credential — the item that actually
/// matters — sitting in the keyring.
#[derive(Debug)]
pub struct Failure {
    pub what: String,
    pub why: String,
    /// Whether this is the credential rather than a directory of stored data.
    ///
    /// A purge that leaves a browser profile behind has failed at housekeeping;
    /// one that leaves the session behind has failed at the only thing it
    /// exists for. The two do not deserve the same closing sentence, and a
    /// field says which is which without anyone matching on [`Failure::what`].
    pub credential: bool,
}

/// Deletes what the plan describes and returns whatever refused to go.
///
/// The session goes first, on purpose: it is the only item here that is a live
/// credential, and it must not be held hostage by a directory that turns out to
/// be locked.
pub fn execute(plan: &Plan, store: &SecretStore) -> Vec<Failure> {
    let mut failures = Vec::new();

    if plan.session
        && let Err(e) = store.delete()
    {
        failures.push(Failure {
            what: "the stored session".to_string(),
            why: e.to_string(),
            credential: true,
        });
    }

    for dir in &plan.directories {
        if let Err(e) = std::fs::remove_dir_all(dir) {
            failures.push(Failure {
                what: dir.display().to_string(),
                why: e.to_string(),
                credential: false,
            });
        }
    }

    for dir in &plan.directories {
        remove_if_empty(dir.parent());
    }

    failures
}

/// Removes a parent that its contents leaving has left empty.
///
/// On Windows the configuration directory and the one earlier versions used are
/// siblings inside a single `snob-ig` folder, so removing both leaves that
/// folder behind: empty, and named after a tool the user has just finished
/// removing. `remove_dir` succeeds only on an empty directory, which is what
/// makes this safe to point at a parent at all.
///
/// It has to be **our** folder, though, and that is the part the Windows
/// reasoning hid. Elsewhere the layout is flat: on Linux the parents are
/// `~/.local/share` and `~/.config`, on macOS `~/Library/Application Support`.
/// Those belong to the platform rather than to snob, they are routinely empty
/// on a minimal container or server image, and removing one is outside what
/// this command was asked to do.
fn remove_if_empty(dir: Option<&Path>) {
    if let Some(dir) = dir
        && dir.file_name().is_some_and(|name| name == "snob-ig")
        && paths::is_safe_to_remove(dir)
    {
        let _ = std::fs::remove_dir(dir);
    }
}

pub fn run(args: PurgeArgs, store: SecretStore, app_paths: &AppPaths) -> Result<ExitCode> {
    let plan = survey(&store, app_paths);

    if plan.is_empty() {
        println!("There is nothing of snob's stored on this computer.");
        report_the_binary();
        return Ok(ExitCode::Ok);
    }

    println!("This will delete, from this computer:");
    for line in plan.lines() {
        println!("  {line}");
    }

    if args.dry_run {
        println!("\nNothing was deleted (--dry-run).");
        return Ok(ExitCode::Ok);
    }

    // With no terminal, `confirm` keeps its default without asking anything —
    // and the default here is no. So an uninstall script used to be shown the
    // whole deletion plan, told "Nothing was deleted.", and given exit 0, then
    // carry on to `apt remove` with the session cookie still in the keyring.
    // Success is the one thing that must not be reported there: this is the
    // command whose entire purpose is that a live credential does not outlive
    // the tool.
    //
    // `--yes` remains the way to do it unattended, and it has to be typed on
    // purpose, which is the right way round for the only command here that
    // destroys anything.
    if !args.yes && !ui::is_interactive() {
        ui::info(
            "There is no terminal to confirm at, so nothing was deleted.\n\
             To delete it unattended, run \"snob purge --yes\".",
        );
        return Err(ExitError::new(
            ExitCode::Error,
            "nothing was deleted: there was no terminal to confirm at",
        )
        .into());
    }

    if !args.yes && !ui::confirm("\nDelete all of it?", false)? {
        println!("Nothing was deleted.");
        return Ok(ExitCode::Ok);
    }

    let failures = execute(&plan, &store);

    if failures.is_empty() {
        println!("Done.");
        // Said rather than implied. Removing a file unlinks it; on any modern
        // filesystem the bytes may survive in a journal, a shadow copy, a
        // snapshot, or — on flash — in a block the drive has not yet erased.
        // No program running as an ordinary user can promise otherwise, and a
        // command called `purge` is exactly where someone would assume it had.
        ui::info(
            "snob's files are gone from this computer. Whether the underlying bytes can still\n\
             be recovered from the disk is not something any program can decide; full-disk\n\
             encryption is what makes a deletion final.",
        );
    }
    for failure in &failures {
        ui::warn(&format!(
            "could not remove {}: {}",
            failure.what, failure.why
        ));
    }

    // The credential is the point of this command, so a refusal there is not
    // one more line in the warning list. It also decides which closing sentence
    // is true: "the session is still active on Instagram" says the only thing
    // left is Instagram's side, which is the false half of the message when the
    // copy on this computer is what would not go.
    let session_survived = failures.iter().any(|f| f.credential);
    if session_survived {
        ui::warn(
            "the stored session is still on this computer. Removing it is what this command \
             exists for, so treat the rest of this run as not done.",
        );
    } else if plan.session {
        // What logout says, for the same reason: nothing was closed on
        // Instagram's side, because closing it would be a write.
        ui::info(
            "\nThe session is still active on Instagram. To really close it, use\n\
             \"Active sessions\" in the app's settings.",
        );
    }

    report_the_binary();

    if failures.is_empty() {
        Ok(ExitCode::Ok)
    } else {
        // A non-zero exit matters more than the wording here: whoever scripted
        // an uninstall needs to find out that part of it did not happen.
        Err(ExitError::new(ExitCode::Error, "some of it could not be removed").into())
    }
}

/// Says where the executable is, since this command deliberately leaves it.
///
/// The path rather than a command to run: which one is right depends on how the
/// binary was installed, which this cannot know, and naming the wrong package
/// manager is worse than naming none.
fn report_the_binary() {
    let location = match std::env::current_exe() {
        Ok(path) => format!(
            "\nThe binary itself is still installed, at {}.",
            path.display()
        ),
        Err(_) => "\nThe binary itself is still installed.".to_string(),
    };
    ui::info(&format!(
        "{location}\nRemove it with whatever installed it, or delete that file."
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_plan_lists_nothing() {
        assert!(Plan::default().is_empty());
        assert!(Plan::default().lines().is_empty());
    }

    /// The session is one line whatever backend holds it, and the directories
    /// follow it verbatim.
    #[test]
    fn the_listing_covers_the_session_and_every_directory() {
        let plan = Plan {
            session: true,
            directories: vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")],
        };
        let lines = plan.lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("session"));
        assert!(lines[1].contains("one"));
        assert!(lines[2].contains("two"));
    }

    /// Nothing to remove is not a failure: someone uninstalling a tool that
    /// never got as far as storing anything should be told so, not errored at.
    #[test]
    fn a_plan_with_no_session_and_no_directories_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let store = SecretStore::new(paths, true).with_service("snob-ig-test-purge-empty");

        let failures = execute(&Plan::default(), &store);
        assert!(failures.is_empty());
    }
}
