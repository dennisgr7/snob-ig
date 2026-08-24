//! The interactive highlights browser: the tray as a list of folders, and
//! inside each one the story browser's own moves.
//!
//! One loop over two levels rather than two loops, because the terminal
//! machinery — the `Tui` guard and the scratch directory — is per *session*,
//! and leaving a folder must not tear any of it down. Everything
//! terminal-shaped is `ui::stories`' and `ui::tui`'s: the guard, the chrome,
//! the receipt contract for what the alternate screen takes away, and the
//! items view itself, which is `stories::draw_items_view` so the two
//! browsers cannot drift apart. This file adds none of its own.
//!
//! What the levels change is only what a row is and what the three verbs do.
//! At the tray a row is a folder: Enter walks in, D keeps everything in it,
//! and the items arrive on the first walk-in and are kept for the session —
//! reopening a folder is free, like reopening a story. Inside, a row is a
//! story in all but expiry: Enter hands it to the system viewer, D keeps it
//! under the very name `snob highlights someone 2 -d 3` would write, and
//! Left or Backspace walks back out with the folder's selection remembered.
//!
//! Fetching inside the loop blocks the keys, deliberately: the story browser
//! already sits on the CDN for megabytes between keystrokes, one `reels_media`
//! request is smaller than that, and a browser that queues keystrokes against
//! a request in flight answers them against a list the user cannot see yet.

use std::path::PathBuf;

use anyhow::Result;
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Cell, HighlightSpacing, Row, Table, TableState};
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_store::paths::AppPaths;

use crate::commands::highlights::{Entry, Tray, items_of_entry};
use crate::commands::stories::{Saved, Story, save_story};
use crate::exit::{ExitCode, ExitError};
use crate::report;
use crate::ui::browser::input::{Action, Next, TICK, next, page, watching_cancel_keys};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::tui;

/// Where the browser is, and what the arrow keys therefore move.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Tray,
    /// Inside one entry, by its index into the tray.
    Inside(usize),
}

/// Everything remembered about one entry across the session.
pub(crate) struct Folder {
    /// `None` until first opened; kept after, so walking out and back in
    /// costs nothing.
    pub(crate) items: Option<Vec<Story>>,
    /// What the system viewer was handed, per item, so Enter twice is one
    /// request. Sized with `items`.
    pub(crate) opened: Vec<Option<PathBuf>>,
    /// The row the selection was on when the user walked out.
    pub(crate) selected: usize,
}

/// Drives the two lists until the user leaves them.
///
/// `start` is an entry to open before the first frame — `snob highlights
/// someone 2 -i` — already checked against the tray by the caller.
pub async fn browse(
    client: &IgClient,
    tray: &Tray,
    start: Option<usize>,
    paths: &AppPaths,
) -> Result<ExitCode> {
    // Before this session's own directory is made, so that a run which never
    // gets that far still tidies up after the ones before it.
    snob_store::paths::sweep_old_scratch(&paths.stories_root(), ABANDONED_AFTER);
    let scratch = Scratch::new(paths.story_scratch())?;

    // Both refusals -- no terminal, raw mode refused -- live in
    // `tui::claim_fullscreen`, with this view's hint on each.
    let mut tui =
        tui::claim_fullscreen("--no-interactive prints the listing; --download saves without one")?;

    let colors = tui::colors_enabled();
    let mut folders: Vec<Folder> = tray
        .entries
        .iter()
        .map(|_| Folder {
            items: None,
            opened: Vec::new(),
            selected: 0,
        })
        .collect();
    let mut level = Level::Tray;
    let mut tray_selected = start.unwrap_or(0);
    let mut note = String::new();
    let mut receipts: Vec<String> = Vec::new();
    let mut list = TableState::default();
    let mut page_rows = 1usize;

    // `-i` on a numbered entry opens it before the first frame, and a folder
    // that cannot be opened leaves the user at the tray with the reason in
    // the note line rather than exiting an interface that just appeared.
    if let Some(index) = start {
        match walk_in(client, tray, index, &mut folders).await {
            Ok(true) => level = Level::Inside(index),
            Ok(false) => note = empty_note(index),
            Err(e) => note = format!("Could not open it: {e}"),
        }
    }

    // The loop runs inside a block so that every way out — a draw that
    // fails included — passes through the guard's drop and the receipts:
    // an error that returned straight through `?` took every "Saved ..."
    // line down with the alternate screen.
    let outcome: Result<ExitCode> = async {
        loop {
            let total = match level {
                Level::Tray => tray.entries.len(),
                Level::Inside(index) => folders[index].items.as_ref().map_or(0, Vec::len),
            };
            // A copy, moved by the arms below and written back only while the
            // level it belongs to is still the one on screen -- walking into a
            // folder must not write the tray's row over the folder's.
            let mut selected = match level {
                Level::Tray => tray_selected,
                Level::Inside(index) => folders[index].selected,
            };

            list.select(Some(selected));
            tui.terminal.draw(|frame| match level {
                Level::Tray => {
                    draw_tray(
                        frame,
                        tray,
                        &folders,
                        &mut list,
                        &note,
                        colors,
                        &mut page_rows,
                    );
                }
                Level::Inside(index) => {
                    draw_items(
                        frame,
                        tray,
                        index,
                        &folders[index],
                        &mut list,
                        &note,
                        colors,
                        &mut page_rows,
                    );
                }
            })?;

            let event = match next(TICK) {
                Ok(event) => event,
                Err(e) => {
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

            let level_before = level;
            match action {
                Action::Up => selected = selected.saturating_sub(1),
                Action::Down => selected = (selected + 1).min(total.saturating_sub(1)),
                Action::PageUp => selected = selected.saturating_sub(page(page_rows)),
                Action::PageDown => {
                    selected = (selected + page(page_rows)).min(total.saturating_sub(1));
                }
                Action::First => selected = 0,
                Action::Last => selected = total.saturating_sub(1),
                Action::Open => match level {
                    Level::Tray => {
                        let (result, stopped) = watching_cancel_keys(
                            client.pacer().cancel_token(),
                            walk_in(client, tray, selected, &mut folders),
                        )
                        .await;
                        match result {
                            Ok(true) => level = Level::Inside(selected),
                            Ok(false) => note = empty_note(selected),
                            Err(e) => note = format!("Could not open it: {e}"),
                        }
                        if let Some(code) = stopped.leave() {
                            break Ok(code);
                        }
                    }
                    Level::Inside(index) => {
                        let folder = &mut folders[index];
                        let items = folder.items.as_deref().unwrap_or_default();
                        let (result, stopped) = watching_cancel_keys(
                            client.pacer().cancel_token(),
                            crate::ui::stories::open(
                                client,
                                &stem_of(tray, index),
                                items,
                                selected,
                                &scratch,
                                &mut folder.opened,
                            ),
                        )
                        .await;
                        note = match result {
                            Ok(path) => format!("Opened {}", path.display()),
                            Err(e) => format!("Could not open it: {e}"),
                        };
                        if let Some(code) = stopped.leave() {
                            break Ok(code);
                        }
                    }
                },
                Action::Download => match level {
                    Level::Tray => {
                        let (folder_note, stopped) = watching_cancel_keys(
                            client.pacer().cancel_token(),
                            keep_folder(client, tray, selected, &mut folders, &mut receipts),
                        )
                        .await;
                        note = folder_note;
                        if let Some(code) = stopped.leave() {
                            break Ok(code);
                        }
                    }
                    Level::Inside(index) => {
                        let folder = &mut folders[index];
                        let items = folder.items.as_deref().unwrap_or_default();
                        let (result, stopped) = watching_cancel_keys(
                            client.pacer().cancel_token(),
                            crate::ui::stories::keep(
                                client,
                                &stem_of(tray, index),
                                items,
                                selected,
                                &mut folder.opened,
                            ),
                        )
                        .await;
                        note = match result {
                            Ok(path) => {
                                let line = format!("Saved {}", path.display());
                                receipts.push(line.clone());
                                line
                            }
                            Err(e) => format!("Could not save it: {e}"),
                        };
                        if let Some(code) = stopped.leave() {
                            break Ok(code);
                        }
                    }
                },
                Action::Back => {
                    // At the tray there is nowhere further out that is not
                    // leaving, and leaving is q's job alone.
                    if let Level::Inside(_) = level {
                        level = Level::Tray;
                    }
                }
                Action::Redraw => tui.terminal.clear()?,
                Action::Quit => break Ok(ExitCode::Ok),
                Action::Interrupt => break Ok(ExitCode::Interrupted),
                Action::None => {}
            }
            // The write-back. Skipped when the arm changed levels: `selected`
            // still belongs to the level the keys were read at. The scroll offset
            // starts over with the level, and the first draw pulls the remembered
            // selection back into view.
            if level == level_before {
                match level {
                    Level::Tray => tray_selected = selected,
                    Level::Inside(index) => folders[index].selected = selected,
                }
            } else {
                list = TableState::default();
            }
        }
    }
    .await;

    drop(tui);
    tui::print_receipts(&receipts);
    outcome
}

/// `someone-2` for the entry at `index` — the command's own stem, so the
/// browser and `-d` write the very same names. One definition on the command
/// side would be nicer still, but the command counts from one and this file
/// from zero, and a function is where that difference is written once.
pub(crate) fn stem_of(tray: &Tray, index: usize) -> String {
    format!("{}-{}", printable(&tray.username), index + 1)
}

/// Fetches an entry's items on first opening; the session keeps them after.
///
/// `Ok(false)` is a folder with nothing in it — deleted since the tray was
/// fetched, or genuinely empty — which is not worth walking into.
pub(crate) async fn walk_in(
    client: &IgClient,
    tray: &Tray,
    index: usize,
    folders: &mut [Folder],
) -> Result<bool> {
    if folders[index].items.is_none() {
        let items = items_of_entry(client, &tray.entries[index]).await?;
        folders[index].opened = vec![None; items.len()];
        folders[index].selected = 0;
        folders[index].items = Some(items);
    }
    Ok(folders[index]
        .items
        .as_ref()
        .is_some_and(|items| !items.is_empty()))
}

/// D on a folder: everything in it, into the working directory, one at a
/// time.
///
/// One at a time rather than through `download_many`, which reports each
/// file on standard error as it lands — lines that would tear the frame this
/// browser is holding. The note line is the browser's one place to speak, and
/// it gets the tally; when anything was saved, the tally also joins the
/// receipts, because the alternate screen takes the note away on exit.
pub(crate) async fn keep_folder(
    client: &IgClient,
    tray: &Tray,
    index: usize,
    folders: &mut [Folder],
    receipts: &mut Vec<String>,
) -> String {
    match walk_in(client, tray, index, folders).await {
        Ok(true) => {}
        Ok(false) => return empty_note(index),
        Err(e) => return format!("Could not fetch it: {e}"),
    }
    let items = folders[index].items.as_deref().unwrap_or_default();
    let stem = stem_of(tray, index);
    let total = items.len();
    let mut kept = 0usize;
    let mut failed = 0usize;
    for number in 1..=total {
        match save_story(client, &stem, items, number, std::path::Path::new(".")).await {
            Ok(Saved::Now(_) | Saved::Already(_)) => kept += 1,
            Err(_) => failed += 1,
        }
    }
    let note = if failed == 0 {
        format!("Saved {kept} of highlight {} here", index + 1)
    } else {
        format!(
            "Saved {kept} of {total} from highlight {}; {failed} failed",
            index + 1
        )
    };
    if kept > 0 {
        receipts.push(note.clone());
    }
    note
}

/// The note for a folder with nothing to show.
pub(crate) fn empty_note(index: usize) -> String {
    format!("Highlight {} is empty", index + 1)
}

/// Draws the tray: folders in named columns, because a count and a date do
/// not explain themselves the way a title does.
fn draw_tray(
    frame: &mut Frame<'_>,
    tray: &Tray,
    folders: &[Folder],
    list: &mut TableState,
    note: &str,
    colors: bool,
    page_rows: &mut usize,
) {
    let total = tray.entries.len();
    let selected = list.selected().unwrap_or(0);
    let area = frame.area();

    let mut block = tui::view_block(
        format!("Highlights · @{} · {total} kept", printable(&tray.username)),
        colors,
        tui::list_padding(area),
    );
    let inner = block.inner(area);
    // One row of the interior belongs to the header.
    let viewport = (inner.height as usize).saturating_sub(1).max(1);
    *page_rows = viewport;
    let fits = total <= viewport;
    if !fits {
        block = block.title_top(tui::position_line(selected, total, colors));
    }
    block = block.title_bottom(if note.is_empty() {
        tui::hint_line(
            "↑↓ move · enter open · d save all of it · q quit".into(),
            colors,
        )
    } else {
        tui::outcome_line(note.to_string(), colors)
    });
    frame.render_widget(block, area);

    *list.offset_mut() = tui::scrolled_offset(list.offset(), selected, total, viewport, 2);
    // Never narrower than its own header, or the word "highlight" clips.
    let title_w = tray
        .entries
        .iter()
        .map(|e| console::measure_text_width(&e.title))
        .max()
        .unwrap_or(1)
        .clamp(9, 28) as u16;
    frame.render_stateful_widget(
        Table::new(
            tray.entries
                .iter()
                .zip(folders)
                .enumerate()
                .map(|(index, (entry, folder))| tray_row(index, entry, folder, colors)),
            [
                Constraint::Length(3),
                Constraint::Length(title_w),
                Constraint::Length(8),
                Constraint::Length(14),
            ],
        )
        .header(tui::header_row(
            &["", "highlight", "items", "updated"],
            colors,
        ))
        .column_spacing(2)
        .row_highlight_style(tui::selection())
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always),
        inner,
        list,
    );
    if !fits {
        tui::scrollbar(frame, area, total, selected, viewport as u16);
    }
}

/// Draws one folder's items, under the folder's own name, through the one
/// items view in `ui::stories` — minus the column a highlight item does not
/// have: a kept story does not expire, so there is no `left`.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn draw_items(
    frame: &mut Frame<'_>,
    tray: &Tray,
    index: usize,
    folder: &Folder,
    list: &mut TableState,
    note: &str,
    colors: bool,
    page_rows: &mut usize,
) {
    let items = folder.items.as_deref().unwrap_or_default();
    let entry = &tray.entries[index];
    let name = if entry.title.is_empty() {
        format!("highlight {}", index + 1)
    } else {
        format!("\"{}\"", entry.title)
    };
    crate::ui::stories::draw_items_view(
        frame,
        format!(
            "@{} · {name} · {} {}",
            printable(&tray.username),
            items.len(),
            if items.len() == 1 { "item" } else { "items" }
        ),
        "↑↓ move · enter open · d download · ← back · q quit",
        items,
        false,
        list,
        note,
        colors,
        page_rows,
    );
}

/// One folder as a table row: number dim, title, the best count known
/// right-aligned under its name, and when it last grew, dim as an aside.
///
/// The count the session has seen beats the count the tray declared, and
/// the tray's number is honestly a declaration until the folder has been
/// opened.
fn tray_row(index: usize, entry: &Entry, folder: &Folder, colors: bool) -> Row<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let count = match folder.items.as_ref() {
        Some(items) => items.len().to_string(),
        None => match entry.declared_items {
            Some(n) => n.to_string(),
            None => "-".to_string(),
        },
    };
    let updated = entry.updated_at.map(report::dated).unwrap_or_default();
    let number = Line::from(format!("{}.", index + 1)).right_aligned();
    let title = if entry.title.is_empty() {
        "-".to_string()
    } else {
        entry.title.clone()
    };
    let count = Line::from(count).right_aligned();
    if !colors {
        return Row::new(vec![
            Cell::from(number),
            Cell::from(title),
            Cell::from(count),
            Cell::from(updated),
        ]);
    }
    Row::new(vec![
        Cell::from(number.style(dim)),
        Cell::from(title),
        Cell::from(count),
        Cell::from(updated).style(dim),
    ])
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use snob_core::Epoch;

    use super::*;
    use crate::commands::stories::Kind;

    fn entry(title: &str, declared: Option<u64>) -> Entry {
        Entry {
            id: "highlight:1".into(),
            title: title.into(),
            declared_items: declared,
            created_at: None,
            updated_at: None,
        }
    }

    fn folder(items: Option<usize>) -> Folder {
        Folder {
            items: items.map(|n| {
                (0..n)
                    .map(|_| Story {
                        kind: Kind::Photo,
                        taken_at: Epoch::new(1_700_000_000),
                        expiring_at: None,
                        url: None,
                        mentions: Vec::new(),
                    })
                    .collect()
            }),
            opened: Vec::new(),
            selected: 0,
        }
    }

    fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    /// At 60x8 the padding is waived: border, header, data rows. The count
    /// column carries the number the session has seen, not the declaration.
    #[test]
    fn the_tray_prefers_the_count_the_session_has_seen() {
        let tray = Tray {
            username: "someone".into(),
            entries: vec![entry("trip", Some(9)), entry("", Some(3))],
        };
        let folders = vec![folder(Some(2)), folder(None)];
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        let mut list = TableState::default();
        list.select(Some(0));
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| {
                draw_tray(frame, &tray, &folders, &mut list, "", false, &mut page_rows);
            })
            .unwrap();
        assert!(row(&terminal, 0).contains("Highlights · @someone · 2 kept"));
        let header = row(&terminal, 1);
        assert!(header.contains("highlight"), "{header}");
        assert!(header.contains("items"), "{header}");
        // Opened once this session: the real count, not the declaration.
        let first = row(&terminal, 2);
        assert!(first.contains("1."), "{first}");
        assert!(first.contains("trip"), "{first}");
        assert!(first.contains('2'), "{first}");
        // Never opened, no title: the declaration and a dash.
        let second = row(&terminal, 3);
        assert!(second.contains("2."), "{second}");
        assert!(second.contains('-'), "{second}");
        assert!(second.contains('3'), "{second}");
        assert!(row(&terminal, 7).contains("save all of it"));
    }

    #[test]
    fn a_folder_draws_under_its_own_name() {
        let tray = Tray {
            username: "someone".into(),
            entries: vec![entry("trip", Some(1))],
        };
        let opened = folder(Some(1));
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        let mut list = TableState::default();
        list.select(Some(0));
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| {
                draw_items(
                    frame,
                    &tray,
                    0,
                    &opened,
                    &mut list,
                    "",
                    false,
                    &mut page_rows,
                );
            })
            .unwrap();
        assert!(row(&terminal, 0).contains("@someone · \"trip\" · 1 item"));
        assert!(row(&terminal, 1).contains("taken"), "the header names it");
        let first = row(&terminal, 2);
        assert!(first.contains("1."), "{first}");
        assert!(first.contains("photo"), "{first}");
        assert!(row(&terminal, 7).contains("← back"));
    }
}
