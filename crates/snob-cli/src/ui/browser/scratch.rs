//! The directory a browsing session opens media from, and its lifecycle.
//!
//! Shared by the story and the highlights browsers, which have the same needs:
//! a private place to put the file a system viewer is about to be handed, gone
//! when the session ends, swept when a session before this one did not get to
//! end.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// How long a scratch directory has to be untouched before a later run treats
/// it as abandoned.
///
/// A browsing session is minutes. Six hours is far past anything that could
/// still be live, and being generous costs nothing: the only thing waiting
/// buys is that a run started this morning and left open over lunch does not
/// have its files pulled out from under it by a run started after it.
pub const ABANDONED_AFTER: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// The scratch directory, removed when the browsing session ends.
///
/// A type rather than two calls, so that leaving through an error removes it
/// too.
///
/// **What it can and cannot promise, measured on Windows 11 in August 2026
/// rather than assumed.** This used to say that an image viewer holds the file
/// open so Windows will not delete it, and that turned out to be false for the
/// viewers people actually have: Photos with the picture on screen and Media
/// Player with the video playing hold no handle at all — Restart Manager
/// reports nobody, and the delete succeeds with the window still up. So on the
/// ordinary path the file really is gone when the session ends, on all three
/// platforms. On Unix it was never in doubt: `unlink` succeeds regardless and
/// a viewer that already has it open keeps working.
///
/// The case that cannot be fixed is a viewer that opens the file without
/// `FILE_SHARE_DELETE`. Nothing deletes underneath that, not `DeleteFileW` and
/// not the POSIX-semantics disposition — only waiting. That is what the sweep
/// at the start of the next session is for, and it is the only mechanism here
/// that does not depend on something having gone right.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(dir: PathBuf) -> Result<Self> {
        // Fresh, not adopted: the name carries the process id, which anybody
        // on the machine can guess ahead of time. The function says what a
        // planted entry under that name used to be able to do.
        snob_store::paths::create_fresh_private_dir(&dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
        Ok(Self(dir))
    }

    pub fn dir(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Silent by design: a file a greedy viewer is still holding is not an
        // error the user can do anything about, and a warning printed every
        // time somebody looked at a story would train them to ignore warnings.
        // The sweep at the start of the next run is what actually answers it.
        //
        // **This does not run on a panic.** The release profile is
        // `panic = "abort"`, so no destructor does. That is another reason the
        // sweep exists rather than being a belt-and-braces extra.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
