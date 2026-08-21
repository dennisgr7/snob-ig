//! The interactive story list: arrow keys, Enter to look, D to keep.
//!
//! **It draws with `console` and reads keys with `crossterm`**, and the split
//! is the whole design. `console` is already here, it measures display columns
//! and it knows what a terminal supports, so it keeps the drawing. Reading is
//! the half it cannot do: `Term::read_key` has no timeout, reports no resize,
//! parses no modified key and enables no bracketed paste -- and the last two of
//! those were not gaps but defects, because `ESC[1;2D` from Shift+Left ended up
//! reaching this list as a `D` and downloading a story. `ui::browser::input`
//! has the details next to the code that closes them, and it costs no new crate
//! on Windows or macOS, because `comfy-table` was compiling `crossterm` already.
//!
//! What is deliberately not taken is the rest of a terminal-UI framework.
//! `ratatui` answers the same problem for **106,496 bytes and 27 crates** in
//! this binary, against **18,432 bytes and none**, and what the difference buys
//! is a cell buffer and a layout solver that a list of rows of text does not
//! use. AGENTS.md carries both numbers and the case that would reverse them.
//!
//! Everything is drawn on **standard error**, and the list stays inline rather
//! than taking the alternate screen. Standard output belongs to the listing, so
//! `snob stories someone --interactive` can still be run beside a redirect
//! without a full-screen interface landing in the file -- the same reasoning
//! `ui::confirm` gives about asking questions on the right stream. Inline
//! rather than full-screen because the last thing this prints, `Saved
//! ./someone-3.jpg`, is the answer the user came for, and the alternate screen
//! takes it away on exit along with the terminal's own search and selection.
//!
//! What it does **not** do is render the picture in the terminal. That decision
//! was re-examined in August 2026 against what terminals actually support now,
//! and it survived; the numbers and the argument are in AGENTS.md so that it
//! does not have to be re-examined every year.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use console::{Term, style};
use snob_core::model::printable;
use snob_core::paths::AppPaths;
use snob_ig::client::IgClient;

use crate::commands::stories::{Stories, bytes_of, default_name, extension_of};
use crate::exit::{ExitCode, ExitError};
use crate::ui::browser::input::{Action, Next, Session, TICK, next, page};
use crate::ui::browser::screen::Screen;
use crate::ui::browser::viewport::Viewport;

/// Rows the list gives up to everything that is not a story: the heading, the
/// blank line under it, and the footer.
const CHROME_ROWS: usize = 3;

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

    // Taken before the cursor is hidden and dropped after it is shown, so that
    // an early return cannot leave a terminal in raw mode with no cursor.
    let _session = Session::enter().map_err(|e| {
        ExitError::new(
            ExitCode::Error,
            format!("the terminal would not go into raw mode: {e}"),
        )
        .with_hint("without one, use --download <number> or --all")
    })?;

    let mut selected = 0usize;
    let mut opened: Vec<Option<PathBuf>> = vec![None; stories.items.len()];
    let mut note = String::new();
    let mut screen = Screen::new();
    let mut view = Viewport::new(1);

    term.hide_cursor().ok();
    let outcome = loop {
        // Recomputed from the terminal's *current* size on every frame rather
        // than updated when a resize is reported. Nothing is then ever laid out
        // against a size that has gone, so a resize event that was coalesced,
        // delivered late or missed entirely cannot leave the list wrong.
        view.height = Screen::usable_rows(&term)
            .saturating_sub(CHROME_ROWS)
            .max(1);
        view.follow(selected, stories.items.len());
        screen.draw(&term, &frame(stories, selected, &view, &note, &term))?;
        note.clear();

        let event = match next(TICK) {
            Ok(event) => event,
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
        };

        let action = match event {
            // Both mean the same thing here: go round, re-read the size, and
            // draw. An unchanged frame writes nothing, so an idle browser
            // waiting for a resize sends no bytes at all.
            Next::Tick | Next::Resized => continue,
            Next::Do(action) => action,
        };

        match action {
            Action::Up => selected = selected.saturating_sub(1),
            Action::Down => selected = (selected + 1).min(stories.items.len() - 1),
            Action::PageUp => selected = selected.saturating_sub(page(&view)),
            Action::PageDown => {
                selected = (selected + page(&view)).min(stories.items.len() - 1);
            }
            Action::First => selected = 0,
            Action::Last => selected = stories.items.len() - 1,
            Action::Open => {
                note = match open(client, stories, selected, &scratch, &mut opened).await {
                    Ok(path) => format!("Opened {}", path.display()),
                    Err(e) => format!("Could not open it: {e}"),
                };
            }
            Action::Download => {
                note = match keep(client, stories, selected, &mut opened).await {
                    Ok(path) => format!("Saved {}", path.display()),
                    Err(e) => format!("Could not save it: {e}"),
                };
            }
            Action::Redraw => screen.invalidate(),
            Action::Quit => break ExitCode::Ok,
            // Raw mode is what makes this reachable. Outside it, Ctrl+C either
            // raises `SIGINT` or fires the console control handler, and the
            // browser never hears about it -- which is what used to happen, and
            // what the arm this replaces claimed it was catching.
            Action::Interrupt => break ExitCode::Interrupted,
            Action::None => {}
        }
    };

    screen.finish(&term).ok();
    term.show_cursor().ok();
    Ok(outcome)
}

/// Turns the list into the rows to draw, and touches no terminal.
///
/// Plain `String`s rather than writes, so that the renderer can diff them and
/// so that what a frame says is a value somebody could assert on. `term` is
/// borrowed for one question -- whether there is color -- and for nothing else.
fn frame(
    stories: &Stories,
    selected: usize,
    view: &Viewport,
    note: &str,
    term: &Term,
) -> Vec<String> {
    let total = stories.items.len();
    let mut rows = Vec::with_capacity(view.height + CHROME_ROWS);

    // Only when some of the list is off screen. On a list that fits, a counter
    // is one more thing to read that says nothing.
    let position = if total > view.height {
        format!("  [{}/{total}]", selected + 1)
    } else {
        String::new()
    };
    rows.push(format!(
        "{}{}",
        style(format!(
            "Stories of @{} - {total} up",
            printable(&stories.username)
        ))
        .bold(),
        style(position).dim()
    ));
    rows.push(String::new());

    for index in view.range(total) {
        let story = &stories.items[index];
        let body = format!(
            "{:>2}. {:<7} {}{}",
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
        );
        let is_selected = index == selected;
        // Two markers, and both earn their place. Reverse video is the one that
        // is visible from across a desk, and it is reverse video rather than a
        // color because there is no color that is legible on every background:
        // the user already told their terminal what its foreground and
        // background are, and this borrows them. The `>` is what is left when
        // there is no styling at all, which `console` decides from `NO_COLOR`,
        // `TERM` and whether anyone is attending.
        let marker = if is_selected { ">" } else { " " };
        let body = if is_selected && term.features().colors_supported() {
            style(body).reverse().to_string()
        } else {
            body
        };
        rows.push(format!("{marker} {body}"));
    }

    rows.push(if note.is_empty() {
        let mut hint = String::from("up/down: move | enter: open | d: download | q: quit");
        if view.more_above() || view.more_below(total) {
            hint.push_str("   ");
            hint.push(if view.more_above() { '\u{2191}' } else { ' ' });
            hint.push(if view.more_below(total) {
                '\u{2193}'
            } else {
                ' '
            });
        }
        style(hint).dim().to_string()
    } else {
        style(note.to_string()).yellow().to_string()
    });
    rows
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
