//! The interactive highlights browser: the tray as a list of folders, and
//! inside each one the story browser's own moves.
//!
//! One loop over two levels rather than two loops, because the terminal
//! machinery — the raw-mode session, the diffing screen, the scratch
//! directory — is per *session*, and leaving a folder must not tear any of it
//! down. Everything terminal-shaped is `ui::stories`' and `ui::browser`'s:
//! the drawing split, the inline-not-alternate-screen decision and the
//! not-a-framework decision are argued there and in AGENTS.md, and this file
//! adds none of its own.
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
use console::{Term, style};
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_store::paths::AppPaths;

use crate::commands::highlights::{Tray, items_of_entry};
use crate::commands::stories::{Saved, Story, save_story};
use crate::exit::{ExitCode, ExitError};
use crate::report;
use crate::ui::browser::input::{Action, Next, Session, TICK, next, page};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::browser::screen::Screen;
use crate::ui::browser::viewport::Viewport;

/// Rows given up to everything that is not a list entry: the heading, the
/// blank line under it, and the footer. The story browser's number, because
/// it is the story browser's layout.
const CHROME_ROWS: usize = 3;

/// Where the browser is, and what the arrow keys therefore move.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Tray,
    /// Inside one entry, by its index into the tray.
    Inside(usize),
}

/// Everything remembered about one entry across the session.
struct Folder {
    /// `None` until first opened; kept after, so walking out and back in
    /// costs nothing.
    items: Option<Vec<Story>>,
    /// What the system viewer was handed, per item, so Enter twice is one
    /// request. Sized with `items`.
    opened: Vec<Option<PathBuf>>,
    /// The row the selection was on when the user walked out.
    selected: usize,
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
    let term = Term::stderr();
    if !term.is_term() {
        return Err(
            ExitError::new(ExitCode::Error, "--interactive needs a terminal to draw on")
                .with_hint("--no-interactive prints the listing; --download saves without one")
                .into(),
        );
    }

    // Before this session's own directory is made, so that a run which never
    // gets that far still tidies up after the ones before it.
    snob_store::paths::sweep_old_scratch(&paths.stories_root(), ABANDONED_AFTER);
    let scratch = Scratch::new(paths.story_scratch())?;

    let _session = Session::enter().map_err(|e| {
        ExitError::new(
            ExitCode::Error,
            format!("the terminal would not go into raw mode: {e}"),
        )
        .with_hint("--no-interactive prints the listing; --download saves without one")
    })?;

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
    let mut screen = Screen::new();
    let mut view = Viewport::new(1);

    term.hide_cursor().ok();

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

    let outcome = loop {
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

        view.height = Screen::usable_rows(&term)
            .saturating_sub(CHROME_ROWS)
            .max(1);
        view.follow(selected, total);
        let rows = match level {
            Level::Tray => tray_frame(tray, &folders, selected, &view, &note, &term),
            Level::Inside(index) => items_frame(tray, index, &folders[index], &view, &note, &term),
        };
        screen.draw(&term, &rows)?;
        note.clear();

        let event = match next(TICK) {
            Ok(event) => event,
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
            Next::Tick | Next::Resized => continue,
            Next::Do(action) => action,
        };

        let level_before = level;
        match action {
            Action::Up => selected = selected.saturating_sub(1),
            Action::Down => selected = (selected + 1).min(total.saturating_sub(1)),
            Action::PageUp => selected = selected.saturating_sub(page(&view)),
            Action::PageDown => selected = (selected + page(&view)).min(total.saturating_sub(1)),
            Action::First => selected = 0,
            Action::Last => selected = total.saturating_sub(1),
            Action::Open => match level {
                Level::Tray => match walk_in(client, tray, selected, &mut folders).await {
                    Ok(true) => level = Level::Inside(selected),
                    Ok(false) => note = empty_note(selected),
                    Err(e) => note = format!("Could not open it: {e}"),
                },
                Level::Inside(index) => {
                    let folder = &mut folders[index];
                    let items = folder.items.as_deref().unwrap_or_default();
                    note = match crate::ui::stories::open(
                        client,
                        &stem_of(tray, index),
                        items,
                        selected,
                        &scratch,
                        &mut folder.opened,
                    )
                    .await
                    {
                        Ok(path) => format!("Opened {}", path.display()),
                        Err(e) => format!("Could not open it: {e}"),
                    };
                }
            },
            Action::Download => match level {
                Level::Tray => {
                    note = keep_folder(client, tray, selected, &mut folders).await;
                }
                Level::Inside(index) => {
                    let folder = &mut folders[index];
                    let items = folder.items.as_deref().unwrap_or_default();
                    note = match crate::ui::stories::keep(
                        client,
                        &stem_of(tray, index),
                        items,
                        selected,
                        &mut folder.opened,
                    )
                    .await
                    {
                        Ok(path) => format!("Saved {}", path.display()),
                        Err(e) => format!("Could not save it: {e}"),
                    };
                }
            },
            Action::Back => {
                // At the tray there is nowhere further out that is not
                // leaving, and leaving is q's job alone.
                if let Level::Inside(_) = level {
                    level = Level::Tray;
                }
            }
            Action::Redraw => screen.invalidate(),
            Action::Quit => break ExitCode::Ok,
            Action::Interrupt => break ExitCode::Interrupted,
            Action::None => {}
        }
        // The write-back. Skipped when the arm changed levels: `selected`
        // still belongs to the level the keys were read at.
        if level == level_before {
            match level {
                Level::Tray => tray_selected = selected,
                Level::Inside(index) => folders[index].selected = selected,
            }
        }
    };

    screen.finish(&term).ok();
    term.show_cursor().ok();
    Ok(outcome)
}

/// `someone-2` for the entry at `index` — the command's own stem, so the
/// browser and `-d` write the very same names. One definition on the command
/// side would be nicer still, but the command counts from one and this file
/// from zero, and a function is where that difference is written once.
fn stem_of(tray: &Tray, index: usize) -> String {
    format!("{}-{}", printable(&tray.username), index + 1)
}

/// Fetches an entry's items on first opening; the session keeps them after.
///
/// `Ok(false)` is a folder with nothing in it — deleted since the tray was
/// fetched, or genuinely empty — which is not worth walking into.
async fn walk_in(
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
/// browser is holding. The note line is the browser's one place to speak,
/// and it gets the tally.
async fn keep_folder(
    client: &IgClient,
    tray: &Tray,
    index: usize,
    folders: &mut [Folder],
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
    if failed == 0 {
        format!("Saved {kept} of highlight {} here", index + 1)
    } else {
        format!(
            "Saved {kept} of {total} from highlight {}; {failed} failed",
            index + 1
        )
    }
}

/// The note for a folder with nothing to show.
fn empty_note(index: usize) -> String {
    format!("Highlight {} is empty", index + 1)
}

/// The tray as rows to draw. Touches no terminal; `term` answers one
/// question, whether there is color.
fn tray_frame(
    tray: &Tray,
    folders: &[Folder],
    selected: usize,
    view: &Viewport,
    note: &str,
    term: &Term,
) -> Vec<String> {
    let total = tray.entries.len();
    let mut rows = Vec::with_capacity(view.height + CHROME_ROWS);

    let position = if total > view.height {
        format!("  [{}/{total}]", selected + 1)
    } else {
        String::new()
    };
    rows.push(format!(
        "{}{}",
        style(format!(
            "Highlights of @{} - {total} kept",
            printable(&tray.username)
        ))
        .bold(),
        style(position).dim()
    ));
    rows.push(String::new());

    for index in view.range(total) {
        let entry = &tray.entries[index];
        // The count the session has seen beats the count the tray declared,
        // and the tray's number is honestly a declaration: "9 declared"
        // against "9 items" once the folder has been opened.
        let count = match folders[index].items.as_ref() {
            Some(items) => match items.len() {
                1 => "1 item".to_string(),
                n => format!("{n} items"),
            },
            None => match entry.declared_items {
                Some(1) => "1 item".to_string(),
                Some(n) => format!("{n} items"),
                None => "-".to_string(),
            },
        };
        let mut body = format!(
            "{:>2}. {:<24} {count}",
            index + 1,
            if entry.title.is_empty() {
                "-"
            } else {
                &entry.title
            },
        );
        if let Some(at) = entry.updated_at {
            body.push_str(&format!(", updated {}", report::dated(at)));
        }
        let is_selected = index == selected;
        let marker = if is_selected { ">" } else { " " };
        let body = if is_selected && term.features().colors_supported() {
            style(body).reverse().to_string()
        } else {
            body
        };
        rows.push(format!("{marker} {body}"));
    }

    rows.push(footer(
        "up/down: move | enter: open | d: save all of it | q: quit",
        view,
        total,
        note,
    ));
    rows
}

/// One folder's items as rows to draw.
fn items_frame(
    tray: &Tray,
    index: usize,
    folder: &Folder,
    view: &Viewport,
    note: &str,
    term: &Term,
) -> Vec<String> {
    let items = folder.items.as_deref().unwrap_or_default();
    let total = items.len();
    let selected = folder.selected;
    let entry = &tray.entries[index];
    let mut rows = Vec::with_capacity(view.height + CHROME_ROWS);

    let position = if total > view.height {
        format!("  [{}/{total}]", selected + 1)
    } else {
        String::new()
    };
    let name = if entry.title.is_empty() {
        format!("highlight {}", index + 1)
    } else {
        format!("\"{}\"", entry.title)
    };
    rows.push(format!(
        "{}{}",
        style(format!(
            "@{} - {name} - {total} {}",
            printable(&tray.username),
            if total == 1 { "item" } else { "items" }
        ))
        .bold(),
        style(position).dim()
    ));
    rows.push(String::new());

    for row in view.range(total) {
        let story = &items[row];
        let body = format!(
            "{:>2}. {:<7} {}{}",
            row + 1,
            crate::commands::stories::kind_label(story),
            report::dated(story.taken_at),
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
        let is_selected = row == selected;
        let marker = if is_selected { ">" } else { " " };
        let body = if is_selected && term.features().colors_supported() {
            style(body).reverse().to_string()
        } else {
            body
        };
        rows.push(format!("{marker} {body}"));
    }

    rows.push(footer(
        "up/down: move | enter: open | d: download | left: back | q: quit",
        view,
        total,
        note,
    ));
    rows
}

/// The last row: the note when there is one, the hint when there is not.
fn footer(hint: &str, view: &Viewport, total: usize, note: &str) -> String {
    if !note.is_empty() {
        return style(note.to_string()).yellow().to_string();
    }
    let mut hint = String::from(hint);
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
}
