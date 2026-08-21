//! The interactive story list: arrow keys, Enter to look, D to keep.
//!
//! **Built on `console`, which is already in the tree, and not on a TUI
//! framework.** `ratatui` and `crossterm` would be the obvious answer and are
//! the wrong one here: this is one list, one highlighted row and five keys, and
//! `ui.rs` next door already reads keys through `console::Term` for the login
//! menu. The section of `AGENTS.md` that measures this binary in bytes argues
//! about half a megabyte at a time; spending that on a widget toolkit for a
//! list this program can draw in twenty lines would be the wrong trade in a
//! project whose first rule is to prefer the option that adds nothing.
//!
//! Everything is drawn on **standard error**. Standard output belongs to the
//! listing, so `snob stories someone --interactive` can still be run beside a
//! redirect without a full-screen interface landing in the file — and the same
//! reasoning `ui::confirm` gives about asking questions on the right stream.
//!
//! What it does **not** do is render the picture in the terminal. Instagram's
//! own client does, and so does the project this design was studied against,
//! through the kitty and sixel protocols. It is a real feature and it is a
//! different one: it needs a terminal that speaks a protocol most do not, and
//! the fallback is a block-character approximation of somebody's photograph.
//! Handing the file to the viewer the user already has shows them the actual
//! image, on every platform, for no dependencies.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use console::{Key, Term};
use snob_core::model::printable;
use snob_core::paths::AppPaths;
use snob_ig::client::IgClient;

use crate::commands::stories::{Stories, bytes_of, default_name, extension_of};
use crate::exit::{ExitCode, ExitError};

/// Drives the list until the user leaves it.
///
/// Downloads are made once and kept: moving up and down a list of ten stories
/// and opening three of them twice is three requests to the CDN, not six.
pub async fn browse(client: &IgClient, stories: &Stories, paths: &AppPaths) -> Result<ExitCode> {
    let term = Term::stderr();
    if !term.is_term() {
        return Err(
            ExitError::new(ExitCode::Error, "--interactive needs a terminal to draw on")
                .with_hint("without one, use --download <number> or --all")
                .into(),
        );
    }

    // Before this session's own directory is made, so that a run which never
    // gets that far still tidies up after the ones before it. See
    // `ABANDONED_AFTER`, and `AppPaths::story_scratch` for where these live.
    snob_core::paths::sweep_old_scratch(&paths.stories_root(), ABANDONED_AFTER);

    let scratch = Scratch::new(paths.story_scratch())?;

    let mut selected = 0usize;
    let mut opened: Vec<Option<PathBuf>> = vec![None; stories.items.len()];
    let mut note = String::new();

    term.hide_cursor().ok();
    let outcome = loop {
        draw(&term, stories, selected, &note)?;
        note.clear();

        // `read_key` on a stream that is not a terminal answers `Key::Unknown`
        // immediately, which would spin this loop. The guard above is what
        // stops that being reachable, and `Unknown` is treated as a key nobody
        // pressed rather than as a reason to redraw for ever.
        match term.read_key() {
            Ok(Key::ArrowUp) | Ok(Key::Char('k')) => selected = selected.saturating_sub(1),
            Ok(Key::ArrowDown) | Ok(Key::Char('j')) => {
                selected = (selected + 1).min(stories.items.len() - 1);
            }
            Ok(Key::Enter) => {
                note = match open(client, stories, selected, &scratch, &mut opened).await {
                    Ok(path) => format!("Opened {}", path.display()),
                    Err(e) => format!("Could not open it: {e}"),
                };
            }
            Ok(Key::Char('d')) | Ok(Key::Char('D')) => {
                note = match keep(client, stories, selected, &mut opened).await {
                    Ok(path) => format!("Saved {}", path.display()),
                    Err(e) => format!("Could not save it: {e}"),
                };
            }
            Ok(Key::Escape) | Ok(Key::Char('q')) => break ExitCode::Ok,
            // Ctrl+C inside a raw read does not raise a signal, so the handler
            // installed for the rest of the program never sees it. Leaving is
            // what it means here, and the exit code says who decided.
            Ok(Key::CtrlC) => break ExitCode::Interrupted,
            Ok(_) => {}
            // Said on the way out rather than written into `note`, which the
            // next redraw would have shown and there is no next redraw.
            Err(e) => {
                term.show_cursor().ok();
                return Err(ExitError::new(
                    ExitCode::Error,
                    format!("the keyboard could not be read: {e}"),
                )
                .into());
            }
        }
    };
    term.show_cursor().ok();
    term.write_line("").ok();
    Ok(outcome)
}

/// Redraws the whole list in place.
///
/// The whole thing every time rather than only the two rows that changed: the
/// list is at most a couple of dozen lines, a terminal redraws that faster than
/// anybody can press a key again, and partial redraws are how a list gets out
/// of step with what is on screen.
fn draw(term: &Term, stories: &Stories, selected: usize, note: &str) -> Result<()> {
    // +3 for the heading, the blank line and the footer; +1 more when there is
    // something to say. Clearing exactly what was written is what keeps this
    // from scrolling the terminal away.
    let drawn = stories.items.len() + 4;
    term.clear_last_lines(drawn.min(term.size().0 as usize))
        .ok();

    term.write_line(&format!(
        "Stories of @{} - {} up",
        printable(&stories.username),
        stories.items.len()
    ))?;
    term.write_line("")?;

    for (index, story) in stories.items.iter().enumerate() {
        let marker = if index == selected { ">" } else { " " };
        term.write_line(&format!(
            "{marker} {:>2}. {:<7} {}{}",
            index + 1,
            crate::commands::stories::kind_label(story),
            crate::commands::stories::posted_and_left(story),
            if story.mentions.is_empty() {
                String::new()
            } else {
                format!(
                    "  {}",
                    story
                        .mentions
                        .iter()
                        .map(|m| format!("@{m}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            }
        ))?;
    }

    term.write_line("")?;
    term.write_line(if note.is_empty() {
        "up/down: move | enter: open | d: download | q: quit"
    } else {
        note
    })?;
    Ok(())
}

/// Writes the story into the scratch directory and hands it to the system
/// viewer.
///
/// The file, not the URL. Handing the CDN address to the browser is what the
/// project this was studied against does, and it puts a signed link to somebody
/// else's story in a browser history — and it opens a browser to look at a
/// picture, which is not what the user asked for.
async fn open(
    client: &IgClient,
    stories: &Stories,
    index: usize,
    scratch: &Scratch,
    cache: &mut [Option<PathBuf>],
) -> Result<PathBuf> {
    if let Some(existing) = cache[index].clone() {
        // Still there: a viewer may have been closed, but nothing deletes
        // these until the session ends.
        if existing.is_file() {
            opener::open(&existing).context("the system viewer would not start")?;
            return Ok(existing);
        }
    }

    let bytes = bytes_of(client, &stories.items[index]).await?;
    let path = scratch.dir().join(format!(
        "{}-{}.{}",
        printable(&stories.username),
        index + 1,
        extension_of(&bytes)
    ));
    std::fs::write(&path, &bytes).with_context(|| format!("could not write {}", path.display()))?;
    opener::open(&path).context("the system viewer would not start")?;
    cache[index] = Some(path.clone());
    Ok(path)
}

/// Saves the story where the user is working, rather than in the scratch
/// directory that gets deleted.
async fn keep(
    client: &IgClient,
    stories: &Stories,
    index: usize,
    cache: &mut [Option<PathBuf>],
) -> Result<PathBuf> {
    // Reuse what was already fetched to look at it. Pressing Enter and then D
    // on the same story is one request, not two.
    let bytes = match cache[index].as_ref().filter(|p| p.is_file()) {
        Some(path) => std::fs::read(path)?,
        None => bytes_of(client, &stories.items[index]).await?,
    };

    let path = default_name(
        Path::new("."),
        &stories.username,
        index + 1,
        extension_of(&bytes),
    )?;
    std::fs::write(&path, &bytes).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// How long a scratch directory has to be untouched before a later run treats
/// it as abandoned.
///
/// A browsing session is minutes. Six hours is far past anything that could
/// still be live, and being generous costs nothing: the only thing waiting
/// buys is that a run started this morning and left open over lunch does not
/// have its files pulled out from under it by a run started after it.
const ABANDONED_AFTER: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

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
/// not the POSIX-semantics disposition — only waiting. That is what
/// [`sweep`] is for, and it is the only mechanism here that does not depend on
/// something having gone right.
struct Scratch(PathBuf);

impl Scratch {
    fn new(dir: PathBuf) -> Result<Self> {
        snob_core::paths::create_private_dir(&dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
        Ok(Self(dir))
    }

    fn dir(&self) -> &Path {
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
