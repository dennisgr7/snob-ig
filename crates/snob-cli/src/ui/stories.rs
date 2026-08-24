//! The interactive story list: arrow keys, Enter to look, D to keep.
//!
//! **It draws with `ratatui` and reads keys through `ui::browser::input`.**
//! Reading is not ratatui's half -- ratatui only writes -- and the input
//! module closes two real defects (`ESC[1;2D` from Shift+Left reaching a list
//! as a `D` that downloads, an unbracketed paste pressing its own letters)
//! with the reasoning next to the code. Drawing goes through `ui::tui`: one
//! guard for the terminal modes, the chrome every view shares, and ratatui's
//! cell diff underneath, so an unchanged frame writes nothing and an idle
//! browser waking on its timer sends no bytes. AGENTS.md carries the decision
//! record for the framework, including what its adoption measured.
//!
//! Everything is drawn on **standard error**, on the **alternate screen**.
//! Standard output belongs to the listing, so `snob stories someone
//! --interactive` beside a redirect still leaves the file empty -- the same
//! reasoning `ui::confirm` gives about asking questions on the right stream.
//! The alternate screen takes the frame away on exit, so the answer the user
//! came for is said again on the real screen: every `Saved ./someone-3.jpg`
//! is collected while the browser runs and printed as a receipt after the
//! guard drops. `ui::tui` carries that contract.
//!
//! What it does **not** do is render the picture in the terminal. That decision
//! was re-examined in August 2026 against what terminals actually support now,
//! and it survived; the numbers and the argument are in AGENTS.md so that it
//! does not have to be re-examined every year.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Cell, HighlightSpacing, Row, Table, TableState};
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_store::paths::AppPaths;

use crate::commands::stories::{Stories, Story, bytes_of, default_name, extension_of};
use crate::exit::{ExitCode, ExitError};
use crate::output;
use crate::ui::browser::input::{Action, Next, TICK, next, page, watching_cancel_keys};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::tui;

/// Drives the list until the user leaves it.
///
/// Downloads are made once and kept: moving up and down a list of ten stories
/// and opening three of them twice is three requests to the CDN, not six.
pub async fn browse(client: &IgClient, stories: &Stories, paths: &AppPaths) -> Result<ExitCode> {
    // Before this session's own directory is made, so that a run which never
    // gets that far still tidies up after the ones before it. See
    // `ABANDONED_AFTER`, and `AppPaths::story_scratch` for where these live.
    snob_store::paths::sweep_old_scratch(&paths.stories_root(), ABANDONED_AFTER);

    let scratch = Scratch::new(paths.story_scratch())?;

    // Both refusals -- no terminal, raw mode refused -- live in
    // `tui::claim_fullscreen`, with this view's hint on each.
    let mut tui =
        tui::claim_fullscreen("--no-interactive prints the listing; --download saves without one")?;

    let colors = tui::colors_enabled();
    let mut selected = 0usize;
    let mut list = TableState::default();
    let mut opened: Vec<Option<PathBuf>> = vec![None; stories.items.len()];
    let mut note = String::new();
    let mut receipts: Vec<String> = Vec::new();
    let mut page_rows = 1usize;

    // The loop runs inside a block so that every way out — a draw that
    // fails included — passes through the guard's drop and the receipts:
    // an error that returned straight through `?` took every "Saved ..."
    // line down with the alternate screen.
    let outcome: Result<ExitCode> = async {
        loop {
            // The selection is this loop's; the state keeps only the scroll
            // offset, which is what makes the list follow the selection with two
            // rows of margin. Layout is against the terminal's *current* size on
            // every draw, so a resize event that was coalesced, delivered late or
            // missed entirely cannot leave the list wrong.
            list.select(Some(selected));
            tui.terminal.draw(|frame| {
                draw(frame, stories, &mut list, &note, colors, &mut page_rows);
            })?;

            let event = match next(TICK) {
                Ok(event) => event,
                // Said on the way out rather than written into `note`, which the
                // next redraw would have shown and there is no next redraw.
                Err(e) => {
                    return Err(ExitError::new(
                        ExitCode::Error,
                        format!("the keyboard could not be read: {e}"),
                    )
                    .into());
                }
            };

            let action = match event {
                // Both mean the same thing here: go round and draw. The next draw
                // reads the new size, and an unchanged frame writes nothing, so an
                // idle browser waiting for a resize sends no bytes at all.
                Next::Tick | Next::Resized => continue,
                Next::Do(action) => action,
            };
            // A note stays up until the user does something else, not until the
            // next timer tick wipes it.
            note.clear();

            match action {
                Action::Up => selected = selected.saturating_sub(1),
                Action::Down => selected = (selected + 1).min(stories.items.len() - 1),
                Action::PageUp => selected = selected.saturating_sub(page(page_rows)),
                Action::PageDown => {
                    selected = (selected + page(page_rows)).min(stories.items.len() - 1);
                }
                Action::First => selected = 0,
                Action::Last => selected = stories.items.len() - 1,
                Action::Open => {
                    let (result, stopped) = watching_cancel_keys(
                        client.pacer().cancel_token(),
                        open(
                            client,
                            &stories.username,
                            &stories.items,
                            selected,
                            &scratch,
                            &mut opened,
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
                Action::Download => {
                    let (result, stopped) = watching_cancel_keys(
                        client.pacer().cancel_token(),
                        keep(
                            client,
                            &stories.username,
                            &stories.items,
                            selected,
                            &mut opened,
                        ),
                    )
                    .await;
                    note = match result {
                        // Twice on purpose: the note is for now, the receipt is
                        // for after the alternate screen has taken the note away.
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
                // A flat list has no level to go up to.
                Action::Back => {}
                // For after anything else has written to the terminal behind the
                // renderer's back: a diffing renderer is only ever as right as its
                // belief about what is on screen, and on Windows ConPTY coalesces
                // positioned writes into fragments no renderer can predict.
                Action::Redraw => tui.terminal.clear()?,
                Action::Quit => break Ok(ExitCode::Ok),
                // Raw mode is what makes this reachable. Outside it, Ctrl+C either
                // raises `SIGINT` or fires the console control handler, and the
                // browser never hears about it -- which is what used to happen, and
                // what the arm this replaces claimed it was catching.
                Action::Interrupt => break Ok(ExitCode::Interrupted),
                Action::None => {}
            }
        }
    }
    .await;

    drop(tui);
    tui::print_receipts(&receipts);
    outcome
}

/// Draws one frame: the shared chrome, the table, and the scrollbar when the
/// table does not fit.
///
/// `page_rows` is written with how many rows the list got, which is what Page
/// Up and Page Down move by -- the keyboard arms cannot see the layout, so the
/// draw leaves the one number they need behind.
fn draw(
    frame: &mut Frame<'_>,
    stories: &Stories,
    list: &mut TableState,
    note: &str,
    colors: bool,
    page_rows: &mut usize,
) {
    draw_items_view(
        frame,
        format!(
            "Stories · @{} · {} up",
            printable(&stories.username),
            stories.items.len()
        ),
        "↑↓ move · enter open · d download · q quit",
        &stories.items,
        true,
        list,
        note,
        colors,
        page_rows,
    );
}

/// The one items view, drawn under whatever title and hints its owner gives
/// it. The story browser, the inside of a highlight folder, and the profile
/// card's sub-views are all this function, so they cannot drift apart.
///
/// `with_left` is what varies: a live story says what is left of it, a kept
/// one only when it was taken.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
pub(crate) fn draw_items_view(
    frame: &mut Frame<'_>,
    title: String,
    hint: &str,
    items: &[Story],
    with_left: bool,
    list: &mut TableState,
    note: &str,
    colors: bool,
    page_rows: &mut usize,
) {
    let total = items.len();
    let selected = list.selected().unwrap_or(0);
    let area = frame.area();

    let mut block = tui::view_block(title, colors, tui::list_padding(area));
    let inner = block.inner(area);
    // One row of the interior belongs to the header.
    let viewport = (inner.height as usize).saturating_sub(1).max(1);
    *page_rows = viewport;
    // Only when some of the list is off screen. On a list that fits, a counter
    // is one more thing to read that says nothing.
    let fits = total <= viewport;
    if !fits {
        block = block.title_top(tui::position_line(selected, total, colors));
    }
    block = block.title_bottom(if note.is_empty() {
        tui::hint_line(hint.to_string(), colors)
    } else {
        tui::outcome_line(note.to_string(), colors)
    });
    frame.render_widget(block, area);

    *list.offset_mut() = tui::scrolled_offset(list.offset(), selected, total, viewport, 2);
    let mut widths = story_widths(items, with_left);
    let header: &[&str] = if with_left {
        &["", "kind", "posted", "left", "mentions"]
    } else {
        // A kept story's date is `report::dated`, not the story browser's
        // wording, so its column is re-measured.
        widths[2] = Constraint::Length(
            items
                .iter()
                .map(|s| crate::report::dated(s.taken_at).len())
                .max()
                .unwrap_or(6)
                .max(5) as u16,
        );
        &["", "kind", "taken", "mentions"]
    };
    let rows = items.iter().enumerate().map(|(index, story)| {
        if with_left {
            story_row(index, story, colors)
        } else {
            item_row(
                index,
                crate::commands::stories::kind_label(story),
                crate::report::dated(story.taken_at),
                None,
                &story.mentions,
                colors,
            )
        }
    });
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .header(tui::header_row(header, colors))
            .column_spacing(2)
            .row_highlight_style(tui::selection())
            // What is left of the selection when there is no styling at all.
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always),
        inner,
        list,
    );
    if !fits {
        tui::scrollbar(frame, area, total, selected, viewport as u16);
    }
}

/// The columns a list of story items needs, measured from the items rather
/// than guessed: a `photo` and an `unknown` are different widths, and a date
/// column sized for August is wrong in September. Only `Length` -- the spare
/// width stays unused on the right, keeping a rail on the left instead of
/// scattering three facts across a two-hundred-column terminal.
pub(crate) fn story_widths(items: &[Story], with_left: bool) -> Vec<Constraint> {
    let kind = items
        .iter()
        .map(|s| crate::commands::stories::kind_label(s).len())
        .max()
        .unwrap_or(5) as u16;
    let posted = items
        .iter()
        .map(|s| crate::commands::stories::posted_of(s).len())
        .max()
        .unwrap_or(6) as u16;
    let mut widths = vec![
        Constraint::Length(3),
        Constraint::Length(kind.max(4)),
        Constraint::Length(posted.max(6)),
    ];
    if with_left {
        let left = items
            .iter()
            .map(|s| crate::commands::stories::left_of(s).len())
            .max()
            .unwrap_or(4) as u16;
        widths.push(Constraint::Length(left.max(4)));
    }
    widths.push(Constraint::Fill(1));
    widths
}

/// One story as a table row: number dim and right-aligned, kind, when it was
/// posted, what is left of it, mentions in cyan.
pub(crate) fn story_row(index: usize, story: &Story, colors: bool) -> Row<'static> {
    item_row(
        index,
        crate::commands::stories::kind_label(story),
        crate::commands::stories::posted_of(story),
        Some(crate::commands::stories::left_of(story)),
        &story.mentions,
        colors,
    )
}

/// The row every view that lists story items draws -- this browser, the
/// highlight folders, the profile card's sub-views -- so they cannot drift
/// apart. What varies between them is only the time columns: a story says
/// when it was posted and what is left of it, a highlight item only when it
/// was taken (`left` is `None` and the column does not exist).
pub(crate) fn item_row(
    index: usize,
    kind: &str,
    posted: String,
    left: Option<String>,
    mentions: &[String],
    colors: bool,
) -> Row<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let number = Line::from(format!("{}.", index + 1)).right_aligned();
    let mut cells = vec![
        if colors {
            Cell::from(number.style(dim))
        } else {
            Cell::from(number)
        },
        Cell::from(kind.to_string()),
        Cell::from(posted),
    ];
    if let Some(left) = left {
        cells.push(if colors {
            Cell::from(left).style(dim)
        } else {
            Cell::from(left)
        });
    }
    if !mentions.is_empty() {
        let joined = mentions
            .iter()
            .map(|m| format!("@{m}"))
            .collect::<Vec<_>>()
            .join(" ");
        cells.push(if colors {
            Cell::from(joined).style(Style::new().fg(Color::Cyan))
        } else {
            Cell::from(joined)
        });
    }
    Row::new(cells)
}

/// Writes the story into the scratch directory and hands it to the system
/// viewer.
///
/// The file, not the URL. Handing the CDN address to the browser is what the
/// project this was studied against does, and it puts a signed link to somebody
/// else's story in a browser history — and it opens a browser to look at a
/// picture, which is not what the user asked for.
/// `stem_base` is what comes before the number in the file's name — the
/// username for a story, the username and the highlight's number for a
/// highlight item — so the two browsers write the very names their commands
/// write.
pub(crate) async fn open(
    client: &IgClient,
    stem_base: &str,
    items: &[Story],
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

    let bytes = bytes_of(client, &items[index]).await?;
    // Through the same gate as `keep` and `snob stories --download`, and for
    // the same reason: the name came off the server. `printable` strips what
    // a terminal must not draw and leaves everything a path reads -- a `..`,
    // a drive letter, a UNC share -- and `Path::join` hands an absolute name
    // the whole path. This was the one write of a story that did not ask
    // `default_path`, and with `fs::write` it would also have followed a link.
    let name = default_name(scratch.dir(), stem_base, index + 1, extension_of(&bytes))?;
    let path = scratch.dir().join(name);
    output::create_new(&path)?
        .write_all(&bytes)
        .with_context(|| format!("could not write {}", path.display()))?;
    opener::open(&path).context("the system viewer would not start")?;
    cache[index] = Some(path.clone());
    Ok(path)
}

/// Saves the story where the user is working, rather than in the scratch
/// directory that gets deleted.
pub(crate) async fn keep(
    client: &IgClient,
    stem_base: &str,
    items: &[Story],
    index: usize,
    cache: &mut [Option<PathBuf>],
) -> Result<PathBuf> {
    // Reuse what was already fetched to look at it. Pressing Enter and then D
    // on the same story is one request, not two.
    let bytes = match cache[index].as_ref().filter(|p| p.is_file()) {
        Some(path) => std::fs::read(path)?,
        None => bytes_of(client, &items[index]).await?,
    };

    let path = default_name(Path::new("."), stem_base, index + 1, extension_of(&bytes))?;
    // Created, not written over: the name is one this program invented, and
    // `snob stories --download` refuses to replace a file under such a name.
    // This key did not, which was two answers to one question.
    output::create_new(&path)?
        .write_all(&bytes)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use snob_core::Epoch;

    use super::*;
    use crate::commands::stories::Kind;

    fn story(mentions: &[&str]) -> Story {
        Story {
            kind: Kind::Photo,
            taken_at: Epoch::new(1_700_000_000),
            expiring_at: None,
            url: None,
            mentions: mentions.iter().map(|m| (*m).to_string()).collect(),
        }
    }

    fn listing(count: usize) -> Stories {
        Stories {
            username: "someone".into(),
            items: (0..count).map(|_| story(&[])).collect(),
        }
    }

    /// Draws once into a buffer nobody sees, which is what makes a frame a
    /// value somebody can assert on.
    fn rendered(
        stories: &Stories,
        selected: usize,
        note: &str,
        colors: bool,
        size: (u16, u16),
    ) -> (Terminal<TestBackend>, usize) {
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        let mut list = TableState::default();
        list.select(Some(selected));
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| draw(frame, stories, &mut list, note, colors, &mut page_rows))
            .unwrap();
        (terminal, page_rows)
    }

    fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    /// At 50x10 the frame is: border, padding row, header, five data rows,
    /// padding row is absorbed by the bottom border. The columns have names
    /// because a date and a countdown do not explain themselves.
    #[test]
    fn the_frame_says_whose_stories_and_how_to_drive_them() {
        let (terminal, page_rows) = rendered(&listing(3), 0, "", false, (50, 10));
        assert!(row(&terminal, 0).contains("Stories · @someone · 3 up"));
        let header = row(&terminal, 2);
        assert!(header.contains("kind"), "{header}");
        assert!(header.contains("posted"), "{header}");
        let first = row(&terminal, 3);
        assert!(first.contains("1."), "{first}");
        assert!(first.contains("photo"), "{first}");
        assert!(row(&terminal, 9).contains("enter open"));
        // Borders, the padding row and the header leave six rows of page.
        assert_eq!(page_rows, 6);
    }

    #[test]
    fn a_note_takes_the_hints_place() {
        let (terminal, _) = rendered(&listing(3), 0, "Saved ./someone-1.jpg", false, (50, 10));
        let bottom = row(&terminal, 9);
        assert!(bottom.contains("Saved ./someone-1.jpg"));
        assert!(!bottom.contains("enter open"));
    }

    #[test]
    fn the_position_appears_only_when_the_list_overflows() {
        let (overflowing, _) = rendered(&listing(20), 4, "", false, (50, 8));
        assert!(row(&overflowing, 0).contains("5/20"));
        let (fitting, _) = rendered(&listing(3), 0, "", false, (50, 10));
        assert!(!row(&fitting, 0).contains("1/3"));
    }

    #[test]
    fn the_selected_row_is_reverse_video_when_styling_is_on() {
        let (terminal, _) = rendered(&listing(3), 1, "", true, (50, 10));
        let buffer = terminal.backend().buffer();
        // Data rows start at y 3 (border, padding, header); the second story
        // is y 4, and the selection covers the row band.
        assert!(
            buffer[(4, 4)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert!(
            !buffer[(4, 3)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
    }

    #[test]
    fn a_mention_rides_on_its_story_row() {
        let stories = Stories {
            username: "someone".into(),
            items: vec![story(&["ana", "bob"])],
        };
        // Short terminal: the padding is waived, rows start under the header.
        let (terminal, _) = rendered(&stories, 0, "", false, (60, 6));
        assert!(row(&terminal, 2).contains("@ana @bob"));
    }

    /// The widths come off the items: a countdown column is as wide as its
    /// widest countdown, never a guess.
    #[test]
    fn the_columns_are_measured_from_the_items() {
        let widths = story_widths(&listing(2).items, true);
        assert_eq!(widths.len(), 5);
        assert_eq!(widths[1], Constraint::Length(5), "photo is five columns");
        let without = story_widths(&listing(2).items, false);
        assert_eq!(without.len(), 4, "no left column for highlight items");
    }
}
