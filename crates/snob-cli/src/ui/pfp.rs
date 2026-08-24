//! The profile-picture viewer: one row, two verbs.
//!
//! `snob pfp` used to have one move — download into the working directory —
//! and the browser rule made the second one worth having: at a terminal a
//! person usually wants to *look* at the full-size picture, and only
//! sometimes to keep it. So the row takes the story browser's verbs exactly:
//! Enter writes the bytes into the session's scratch directory and hands the
//! file to the system viewer, D saves them here under the name the static
//! command writes. The bytes are already in hand when this opens — the fetch
//! happened before, where the cancel check and the cooldown gate live — so
//! nothing in this loop touches the network.
//!
//! Everything terminal-shaped is `ui::tui`'s and `ui::browser::input`'s: the
//! guard, the chrome, and the receipt said after the alternate screen has
//! taken the frame away. A list of one is still drawn as a list,
//! deliberately: the row is where the picture's size and kind are said, and
//! the frame is the same shape a reader already knows from `stories` and
//! `highlights`.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use console::Term;
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Cell, HighlightSpacing, Row, Table, TableState};
use snob_core::model::printable;
use snob_store::paths::AppPaths;

use crate::commands::pfp::Picture;
use crate::exit::{ExitCode, ExitError};
use crate::output;
use crate::ui::browser::input::{Action, Next, TICK, next};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::tui::{self, Tui};

/// Drives the row until the user leaves it.
pub(crate) fn browse(picture: &Picture, paths: &AppPaths) -> Result<ExitCode> {
    let term = Term::stderr();
    if !term.is_term() {
        return Err(
            ExitError::new(ExitCode::Error, "--interactive needs a terminal to draw on")
                .with_hint("--no-interactive downloads the picture; -o says where")
                .into(),
        );
    }

    // Before this session's own directory is made, so that a run which never
    // gets that far still tidies up after the ones before it.
    snob_store::paths::sweep_old_scratch(&paths.stories_root(), ABANDONED_AFTER);
    let scratch = Scratch::new(paths.story_scratch())?;

    let mut tui = Tui::fullscreen().map_err(|e| {
        ExitError::new(
            ExitCode::Error,
            format!("the terminal would not go into raw mode: {e}"),
        )
        .with_hint("--no-interactive downloads the picture; -o says where")
    })?;

    let colors = tui::colors_enabled();
    let mut note = String::new();
    let mut receipts: Vec<String> = Vec::new();
    // Enter twice is one write: the scratch file is kept for the session.
    let mut opened: Option<PathBuf> = None;

    let outcome = loop {
        tui.terminal
            .draw(|frame| draw(frame, picture, &note, colors))?;

        let event = match next(TICK) {
            Ok(event) => event,
            Err(e) => {
                drop(tui);
                tui::print_receipts(&receipts);
                return Err(ExitError::new(
                    ExitCode::Error,
                    format!("the keyboard could not be read: {e}"),
                )
                .into());
            }
        };
        let action = match event {
            Next::Tick | Next::Resized => continue,
            Next::Do(action) => action,
        };
        note.clear();

        match action {
            Action::Open => {
                note = match open(picture, &scratch, &mut opened) {
                    Ok(path) => format!("Opened {}", path.display()),
                    Err(e) => format!("Could not open it: {e}"),
                };
            }
            Action::Download => {
                note = match keep(picture) {
                    Ok(path) => {
                        let line = format!("Saved {}", path.display());
                        receipts.push(line.clone());
                        line
                    }
                    Err(e) => format!("Could not save it: {e}"),
                };
            }
            Action::Redraw => tui.terminal.clear()?,
            Action::Quit | Action::Back => break ExitCode::Ok,
            Action::Interrupt => break ExitCode::Interrupted,
            // One row: there is nowhere for the selection to move.
            _ => {}
        }
    };

    drop(tui);
    tui::print_receipts(&receipts);
    Ok(outcome)
}

/// Draws the one row inside the shared chrome. The row is always the
/// selection — there is nothing else to select.
fn draw(frame: &mut Frame<'_>, picture: &Picture, note: &str, colors: bool) {
    let area = frame.area();
    let block = tui::view_block(
        format!("Profile picture · @{}", printable(&picture.username)),
        colors,
        tui::list_padding(area),
    )
    .title_bottom(if note.is_empty() {
        tui::hint_line("enter open · d save here · q quit".into(), colors)
    } else {
        tui::outcome_line(note.to_string(), colors)
    });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut list = TableState::default();
    list.select(Some(0));
    frame.render_stateful_widget(
        Table::new(
            [picture_row(picture, colors)],
            [
                Constraint::Length(4),
                Constraint::Length(
                    console::measure_text_width(&picture.source.label()).max(9) as u16
                ),
                Constraint::Length(10),
            ],
        )
        .column_spacing(2)
        .row_highlight_style(tui::selection())
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always),
        inner,
        &mut list,
    );
}

/// What the picture is: kind, where it came from, and what a save costs —
/// the cost dim, because it is the aside.
fn picture_row(picture: &Picture, colors: bool) -> Row<'static> {
    let size = format!("({})", human_size(picture.bytes.len()));
    let mut cells = vec![
        Cell::from(picture.extension().to_string()),
        Cell::from(picture.source.label()),
    ];
    cells.push(if colors {
        Cell::from(size).style(Style::new().add_modifier(Modifier::DIM))
    } else {
        Cell::from(size)
    });
    Row::new(cells)
}

/// Writes the picture into the scratch directory and hands it to the system
/// viewer. The file, not the URL, for the story browser's reasons.
pub(crate) fn open(
    picture: &Picture,
    scratch: &Scratch,
    opened: &mut Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(existing) = opened.clone()
        && existing.is_file()
    {
        opener::open(&existing).context("the system viewer would not start")?;
        return Ok(existing);
    }
    let name = output::default_path(scratch.dir(), &picture.username, picture.extension())?;
    let path = scratch.dir().join(name);
    output::create_new(&path)?
        .write_all(&picture.bytes)
        .with_context(|| format!("could not write {}", path.display()))?;
    opener::open(&path).context("the system viewer would not start")?;
    *opened = Some(path.clone());
    Ok(path)
}

/// Saves the picture where the user is working — the very name and refusal
/// `snob pfp someone` gives: created, never written over.
pub(crate) fn keep(picture: &Picture) -> Result<PathBuf> {
    let path = output::default_path(
        std::path::Path::new("."),
        &picture.username,
        picture.extension(),
    )?;
    output::create_new(&path)?
        .write_all(&picture.bytes)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// "243 KB" / "1.2 MB": enough to know what a save costs, nothing to audit.
fn human_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn the_size_reads_in_kilobytes_until_a_megabyte() {
        assert_eq!(human_size(1), "1 KB");
        assert_eq!(human_size(243 * 1024), "243 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(1024 * 1024 * 3 / 2), "1.5 MB");
    }

    #[test]
    fn the_frame_says_what_the_picture_is_and_how_to_take_it() {
        let picture = Picture::for_tests("someone", vec![0u8; 243 * 1024]);
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        terminal
            .draw(|frame| draw(frame, &picture, "", false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        assert!(row(0).contains("Profile picture · @someone"));
        assert!(row(1).contains("(243 KB)"));
        assert!(row(1).contains("> "));
        assert!(row(5).contains("d save here"));
    }
}
