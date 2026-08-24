//! The interactive account list: arrow keys, Enter to open a profile, `/` to
//! filter.
//!
//! Like the media browsers, this is the default at a human terminal: the
//! rule and its matrix live at `BrowseArgs` in `cli.rs`, and a pipe, a
//! redirect, `--format`, `-o` and `--no-interactive` all print the listing
//! exactly as before. Open, it earns its keep with the two things scrollback
//! cannot do: it narrows a long list as you type, and it opens the account
//! under the cursor without anybody retyping a username into a browser.
//!
//! Everything terminal-shaped is `ui::tui`'s and `ui::browser::input`'s, and
//! this file adds none of its own — with one deliberate exception: it can be
//! entered with a guard somebody else already holds. The profile card lends
//! its own `Tui` through [`browse_in`], because two guards would be two
//! claims on one terminal; `browse` is the standalone door that makes a guard
//! and prints the receipts. Like the highlights browser it is one loop over
//! two levels — `snob scan -i` starts at a tray of the five crossings and
//! Enter walks into one — and a single list is the same loop with the tray
//! skipped.
//!
//! **Opening hands the profile address to the system browser.** The address is
//! `User::profile_url`, the same public page the printed table already puts
//! behind every username as a hyperlink — encoded, never filtered, so the name
//! with an odd character in it opens the account it names. This is unlike the
//! story browser's refusal to hand over CDN addresses, and the difference is
//! what the address is: a signed link to somebody's story is a credential in a
//! browser history, a profile address is public and carries nothing.
//!
//! **The filter is the one text mode in the tool**, and it is why
//! `input::read` exists beside `input::next`: while a query is being typed a
//! `q` is a letter of somebody's name, so the browser binds keys itself here
//! rather than through `action_of`. A paste lands in the query — text typed
//! fast and text pasted mean the same thing in a search box — with anything
//! below space dropped, so a multi-line paste cannot smuggle an Enter. While
//! the query owns the keyboard the frame shows a real cursor at the end of
//! it, which is the terminal's own way of saying where typing goes.

use anyhow::Result;
use console::{Term, measure_text_width};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Position};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, HighlightSpacing, Paragraph, Row, Table, TableState};
use snob_core::model::User;

use crate::exit::{ExitCode, ExitError};
use crate::ui::browser::input::Action;
use crate::ui::browser::input::{self, Raw, TICK, action_of, page};
use crate::ui::tui::{self, Tui};

/// What the browser shows: one list, or a tray of them.
///
/// Borrowed, not owned — the commands already hold the lists, and the browser
/// only reads them.
pub struct Shelf<'a> {
    /// "unfollowers of @someone" — what the heading says the rows are.
    pub title: String,
    pub sets: Vec<Set<'a>>,
}

pub struct Set<'a> {
    /// What the tray calls this list. Unused when the shelf holds one set.
    pub label: String,
    pub people: &'a [User],
}

impl<'a> Shelf<'a> {
    /// One list, no tray: the shape every list command hands over.
    pub fn flat(title: String, people: &'a [User]) -> Self {
        Self {
            title,
            sets: vec![Set {
                label: String::new(),
                people,
            }],
        }
    }
}

/// Where the browser is, and what the arrow keys therefore move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Tray,
    /// Inside one set, by its index into the shelf.
    Inside(usize),
}

/// Whether keys move the selection or edit the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Moving,
    Filtering,
}

/// Everything the loop reads to draw one frame, kept apart from the terminal
/// so [`draw`] stays a function a test can point at a buffer.
struct State {
    level: Level,
    mode: Mode,
    query: String,
    /// Indices into the current set's people that the query lets through.
    /// The whole list when the query is empty.
    shown: Vec<usize>,
    /// The selected row of the current level's list.
    selected: usize,
    /// The tray row to come back to after walking out of a set.
    tray_selected: usize,
}

/// The refusal `-i` earns where no browser can be drawn.
///
/// Asked at the top of the command, before the session is opened and long
/// before a request is spent — the same order `-o` is checked in, and for the
/// same reason: a walk that succeeds and then cannot be shown is the
/// expensive order to find out in. The predicate is [`crate::ui::can_show_a_menu`],
/// because a browser needs what a menu needs: keys from standard input,
/// drawing on standard error. Standard output is deliberately not consulted —
/// `-i` was said, so nothing is being detected, and the listing it would
/// protect is exactly what `-i` asks to withhold.
pub fn check_drawable() -> Result<()> {
    if crate::ui::can_show_a_menu() {
        return Ok(());
    }
    Err(
        ExitError::new(ExitCode::Error, "--interactive needs a terminal to draw on")
            .with_hint("--no-interactive prints the result instead")
            .into(),
    )
}

/// Drives the list until the user leaves it, with a terminal of its own.
pub fn browse(shelf: &Shelf<'_>) -> Result<ExitCode> {
    let term = Term::stderr();
    if !term.is_term() {
        return Err(
            ExitError::new(ExitCode::Error, "--interactive needs a terminal to draw on")
                .with_hint("--no-interactive prints the listing; --format and -o shape it")
                .into(),
        );
    }

    let mut tui = Tui::fullscreen().map_err(|e| {
        ExitError::new(
            ExitCode::Error,
            format!("the terminal would not go into raw mode: {e}"),
        )
        .with_hint("--no-interactive prints the listing; --format and -o shape it")
    })?;

    let mut receipts: Vec<String> = Vec::new();
    let outcome = browse_in(&mut tui, shelf, &mut receipts);
    drop(tui);
    tui::print_receipts(&receipts);
    outcome
}

/// The same list on a guard somebody else holds.
///
/// The profile card enters here: it already owns the terminal, and two `Tui`s
/// would be two claims on it. Receipts belong to the guard's owner too — they
/// are printed after *that* guard drops, not here.
pub(crate) fn browse_in(
    tui: &mut Tui,
    shelf: &Shelf<'_>,
    receipts: &mut Vec<String>,
) -> Result<ExitCode> {
    let colors = tui::colors_enabled();
    let flat = shelf.sets.len() == 1;
    let mut state = State {
        level: if flat { Level::Inside(0) } else { Level::Tray },
        mode: Mode::Moving,
        query: String::new(),
        shown: (0..shelf.sets.first().map_or(0, |s| s.people.len())).collect(),
        selected: 0,
        tray_selected: 0,
    };
    if !flat {
        state.shown = Vec::new();
    }

    let mut note = String::new();
    let mut list = TableState::default();
    let mut page_rows = 1usize;

    loop {
        let rows_here = rows_in(shelf, &state);
        state.selected = state.selected.min(rows_here.saturating_sub(1));
        list.select(Some(state.selected));
        tui.terminal.draw(|frame| {
            draw(
                frame,
                shelf,
                &state,
                &note,
                colors,
                &mut list,
                &mut page_rows,
            );
        })?;

        let event = match input::read(TICK) {
            Ok(event) => event,
            Err(e) => {
                return Err(ExitError::new(
                    ExitCode::Error,
                    format!("the keyboard could not be read: {e}"),
                )
                .into());
            }
        };
        // A note stays up until the user does something else, not until the
        // next timer tick wipes it.
        if !matches!(event, Raw::Tick | Raw::Resized) {
            note.clear();
        }

        let level_before = state.level;
        let step = match state.mode {
            Mode::Moving => moving(shelf, &mut state, flat, page_rows, receipts, event),
            Mode::Filtering => filtering(shelf, &mut state, event),
        };
        // The scroll offset belongs to the level it was scrolled at; the
        // first draw of the next level pulls its remembered selection back
        // into view.
        if state.level != level_before {
            list = TableState::default();
        }
        match step {
            Step::Go => {}
            Step::Note(text) => note = text,
            Step::Redraw => tui.terminal.clear()?,
            Step::Leave(code) => return Ok(code),
        }
    }
}

/// What one key did to the loop.
enum Step {
    Go,
    Note(String),
    Redraw,
    Leave(ExitCode),
}

/// How many rows the current level has to move over.
fn rows_in(shelf: &Shelf<'_>, state: &State) -> usize {
    match state.level {
        Level::Tray => shelf.sets.len(),
        Level::Inside(_) => state.shown.len(),
    }
}

/// Recomputes which rows the query lets through, from the top.
///
/// The selection goes back to the first match rather than trying to follow a
/// row that may no longer be shown: while somebody is typing, the first match
/// is the row they are steering toward.
fn refilter(shelf: &Shelf<'_>, state: &mut State) {
    let Level::Inside(index) = state.level else {
        return;
    };
    let needle = state.query.to_lowercase();
    state.shown = shelf.sets[index]
        .people
        .iter()
        .enumerate()
        .filter(|(_, person)| {
            needle.is_empty()
                || person.username.to_lowercase().contains(&needle)
                || person
                    .full_name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase().contains(&needle))
        })
        .map(|(i, _)| i)
        .collect();
    state.selected = 0;
}

/// One key while the arrows own the list.
fn moving(
    shelf: &Shelf<'_>,
    state: &mut State,
    flat: bool,
    page_rows: usize,
    receipts: &mut Vec<String>,
    event: Raw,
) -> Step {
    let key = match event {
        Raw::Tick | Raw::Resized | Raw::Paste(_) => return Step::Go,
        Raw::Key(key) => key,
    };

    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);

    // The two keys `action_of` cannot answer for this browser: `/` starts a
    // query, and Esc clears one before it means leave.
    if let Level::Inside(_) = state.level {
        if key.code == KeyCode::Char('/') && plain {
            state.mode = Mode::Filtering;
            state.query.clear();
            refilter(shelf, state);
            return Step::Go;
        }
        if key.code == KeyCode::Esc && !state.query.is_empty() {
            state.query.clear();
            refilter(shelf, state);
            return Step::Go;
        }
    }

    let rows = rows_in(shelf, state);
    match action_of(key) {
        Action::Up => state.selected = state.selected.saturating_sub(1),
        Action::Down => state.selected = (state.selected + 1).min(rows.saturating_sub(1)),
        Action::PageUp => state.selected = state.selected.saturating_sub(page(page_rows)),
        Action::PageDown => {
            state.selected = (state.selected + page(page_rows)).min(rows.saturating_sub(1));
        }
        Action::First => state.selected = 0,
        Action::Last => state.selected = rows.saturating_sub(1),
        Action::Open => return open(shelf, state, receipts),
        Action::Back => {
            if matches!(state.level, Level::Inside(_)) && !flat {
                state.level = Level::Tray;
                state.selected = state.tray_selected;
                state.query.clear();
                state.shown = Vec::new();
            }
        }
        Action::Redraw => return Step::Redraw,
        Action::Quit => return Step::Leave(ExitCode::Ok),
        Action::Interrupt => return Step::Leave(ExitCode::Interrupted),
        Action::Download | Action::None => {}
    }
    Step::Go
}

/// Enter, wherever the selection is.
fn open(shelf: &Shelf<'_>, state: &mut State, receipts: &mut Vec<String>) -> Step {
    match state.level {
        Level::Tray => {
            state.tray_selected = state.selected;
            state.level = Level::Inside(state.selected);
            state.query.clear();
            refilter(shelf, state);
            state.selected = 0;
            Step::Go
        }
        Level::Inside(index) => {
            let Some(&person) = state.shown.get(state.selected) else {
                return Step::Go;
            };
            let person = &shelf.sets[index].people[person];
            let url = person.profile_url();
            match opener::open(&url) {
                // Twice on purpose: the note is for now, the receipt is for
                // after the alternate screen has taken the note away.
                Ok(()) => {
                    let line = format!("Opened {url}");
                    receipts.push(line.clone());
                    Step::Note(line)
                }
                Err(e) => Step::Note(format!("Could not open it: {e}")),
            }
        }
    }
}

/// One key while the query owns the keyboard.
fn filtering(shelf: &Shelf<'_>, state: &mut State, event: Raw) -> Step {
    match event {
        Raw::Tick | Raw::Resized => Step::Go,
        // Pasting into a search box is typing fast. Anything below space is
        // dropped so a multi-line paste cannot carry an Enter in it.
        Raw::Paste(text) => {
            state.query.extend(text.chars().filter(|c| !c.is_control()));
            refilter(shelf, state);
            Step::Go
        }
        Raw::Key(key) => filtering_key(shelf, state, key),
    }
}

fn filtering_key(shelf: &Shelf<'_>, state: &mut State, key: KeyEvent) -> Step {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('c') {
        return Step::Leave(ExitCode::Interrupted);
    }
    if ctrl && key.code == KeyCode::Char('l') {
        return Step::Redraw;
    }
    let rows = rows_in(shelf, state);
    match key.code {
        // Esc gives the whole list back; Enter keeps what the query narrowed
        // it to. Both put the arrows back in charge.
        KeyCode::Esc => {
            state.mode = Mode::Moving;
            state.query.clear();
            refilter(shelf, state);
        }
        KeyCode::Enter => state.mode = Mode::Moving,
        KeyCode::Backspace => {
            if state.query.pop().is_some() {
                refilter(shelf, state);
            } else {
                state.mode = Mode::Moving;
            }
        }
        // The arrows keep working mid-query, so narrowing and picking are one
        // motion rather than a mode change apart.
        KeyCode::Up => state.selected = state.selected.saturating_sub(1),
        KeyCode::Down => state.selected = (state.selected + 1).min(rows.saturating_sub(1)),
        KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            state.query.push(c);
            refilter(shelf, state);
        }
        _ => {}
    }
    Step::Go
}

/// Draws one frame: the shared chrome, whichever level's table, and — while
/// the query owns the keyboard — a real cursor at the end of it.
fn draw(
    frame: &mut Frame<'_>,
    shelf: &Shelf<'_>,
    state: &State,
    note: &str,
    colors: bool,
    list: &mut TableState,
    page_rows: &mut usize,
) {
    let area = frame.area();
    let total = rows_in(shelf, state);

    let mut block = tui::view_block(title_of(shelf, state), colors, tui::list_padding(area));
    let inner = block.inner(area);
    let viewport = (inner.height as usize).max(1);
    *page_rows = viewport;
    let fits = total <= viewport;
    if !fits {
        block = block.title_top(tui::position_line(state.selected, total, colors));
    }
    block = block.title_bottom(footer_line(shelf, state, note, colors));
    frame.render_widget(block, area);

    *list.offset_mut() = tui::scrolled_offset(list.offset(), state.selected, total, viewport, 2);
    match state.level {
        Level::Tray => {
            let label_w = shelf
                .sets
                .iter()
                .map(|set| measure_text_width(&set.label))
                .max()
                .unwrap_or(1) as u16;
            frame.render_stateful_widget(
                Table::new(
                    shelf.sets.iter().map(|set| {
                        // A tray of five is a menu, not an enumeration: a
                        // blank row between entries gives each one a hand.
                        Row::new(vec![
                            Cell::from(set.label.clone()),
                            Cell::from(Line::from(set.people.len().to_string()).right_aligned()),
                        ])
                        .bottom_margin(1)
                    }),
                    [Constraint::Length(label_w), Constraint::Length(9)],
                )
                .column_spacing(2)
                .row_highlight_style(tui::selection())
                .highlight_symbol("> ")
                .highlight_spacing(HighlightSpacing::Always),
                inner,
                list,
            );
        }
        Level::Inside(_) if state.shown.is_empty() => {
            let text = if state.query.is_empty() {
                "(nobody)"
            } else {
                "(nobody matches)"
            };
            frame.render_widget(
                Paragraph::new(if colors {
                    Line::from(Span::styled(text, Style::new().add_modifier(Modifier::DIM)))
                } else {
                    Line::from(text)
                }),
                inner,
            );
        }
        Level::Inside(index) => {
            let set = &shelf.sets[index];
            frame.render_stateful_widget(
                Table::new(
                    state
                        .shown
                        .iter()
                        .enumerate()
                        .map(|(row, &i)| person_row(&set.people[i], row == state.selected, colors)),
                    columns(set.people, &state.shown, inner.width),
                )
                .column_spacing(2)
                .row_highlight_style(tui::selection())
                .highlight_symbol("> ")
                .highlight_spacing(HighlightSpacing::Always),
                inner,
                list,
            );
        }
    }
    if !fits {
        tui::scrollbar(frame, area, total, state.selected, viewport as u16);
    }

    // The cursor is the terminal's own way of saying where typing goes, and
    // it is shown only while there is somewhere for typing to go. Drawn last,
    // over the bottom border, at the end of the query.
    if state.mode == Mode::Filtering {
        let x = area.x
            + 1
            + measure_text_width(FILTER_PREFIX) as u16
            + measure_text_width(&state.query) as u16;
        let y = area.bottom().saturating_sub(1);
        if x < area.right().saturating_sub(1) {
            frame.set_cursor_position(Position { x, y });
        }
    }
}

/// What the top border calls the current level.
fn title_of(shelf: &Shelf<'_>, state: &State) -> String {
    match state.level {
        Level::Tray => shelf.title.clone(),
        Level::Inside(index) => {
            let set = &shelf.sets[index];
            let title = if shelf.sets.len() == 1 {
                shelf.title.clone()
            } else {
                format!("{} — {}", shelf.title, set.label)
            };
            if state.query.is_empty() {
                format!("{title} — {} accounts", set.people.len())
            } else {
                format!(
                    "{title} — {} of {} match \"{}\"",
                    state.shown.len(),
                    set.people.len(),
                    state.query
                )
            }
        }
    }
}

/// What the filter footer starts with; the cursor position is computed
/// against it — in display columns, like everything else — so the two
/// cannot drift apart.
const FILTER_PREFIX: &str = " / ";

/// The bottom border: the note when there is one, the query while it is being
/// typed, the hints otherwise.
fn footer_line(shelf: &Shelf<'_>, state: &State, note: &str, colors: bool) -> Line<'static> {
    if !note.is_empty() {
        return tui::outcome_line(note.to_string(), colors);
    }
    if state.mode == Mode::Filtering {
        let hint = "  enter keep · esc clear ";
        return Line::from(vec![
            Span::raw(FILTER_PREFIX),
            Span::raw(state.query.clone()),
            if colors {
                Span::styled(hint, Style::new().add_modifier(Modifier::DIM))
            } else {
                Span::raw(hint)
            },
        ]);
    }

    let mut hint = String::from(match state.level {
        Level::Tray => "↑↓ move · enter open · q quit",
        Level::Inside(_) if shelf.sets.len() > 1 => {
            "↑↓ move · enter open profile · / filter · ← back · q quit"
        }
        Level::Inside(_) => "↑↓ move · enter open profile · / filter · q quit",
    });
    if !state.query.is_empty() {
        hint.push_str(" · esc clear filter");
    }
    tui::hint_line(hint, colors)
}

/// The account list's columns, measured in display columns from what is
/// shown. Only `Length`: under `Flex::Start` the spare width stays unused on
/// the right, which is exactly what is wanted — a rail on the left, not
/// three facts scattered to the ends of a two-hundred-column terminal.
///
/// Narrow terminals drop whole columns rather than clip them: the badge
/// falls away first, then the full name, and the username column is the one
/// that never goes.
fn columns(people: &[User], shown: &[usize], width: u16) -> Vec<Constraint> {
    let user_w = shown
        .iter()
        .map(|&i| measure_text_width(&people[i].safe_username()) + 1)
        .max()
        .unwrap_or(1)
        .clamp(12, 26) as u16;
    let name_w = shown
        .iter()
        .filter_map(|&i| people[i].safe_full_name())
        .map(|n| measure_text_width(&n))
        .max()
        .unwrap_or(0)
        .min(32) as u16;
    // The widest badge is "verified, private". Left-aligned: badges are
    // words, not numbers.
    let badge_w = if shown.iter().any(|&i| attributes_of(&people[i]).is_some()) {
        17u16
    } else {
        0
    };

    let gutter = 2 + 2; // "> " and the spacing after the username column
    let mut out = vec![Constraint::Length(user_w)];
    if badge_w > 0 && width >= gutter + user_w + 2 + name_w + 2 + badge_w {
        out.push(Constraint::Length(name_w.max(1)));
        out.push(Constraint::Length(badge_w));
    } else if width >= gutter + user_w + 2 + 12 {
        out.push(Constraint::Length(width - gutter - user_w - 2));
    }
    out
}

/// One account as a table row: the username, the full name, and the badges
/// dim at the end — dim except on the selection, where the reverse video is
/// the emphasis and a dim run inside it would mute it.
fn person_row(person: &User, selected: bool, colors: bool) -> Row<'static> {
    let mut cells = vec![
        Cell::from(format!("@{}", person.safe_username())),
        Cell::from(person.safe_full_name().unwrap_or_default()),
    ];
    if let Some(attributes) = attributes_of(person) {
        cells.push(if colors && !selected {
            Cell::from(attributes).style(Style::new().add_modifier(Modifier::DIM))
        } else {
            Cell::from(attributes)
        });
    }
    Row::new(cells)
}

/// The same badge words the printed table uses, so the browser and the table
/// never describe one account two ways.
fn attributes_of(person: &User) -> Option<&'static str> {
    match (
        person.is_verified.unwrap_or(false),
        person.is_private.unwrap_or(false),
    ) {
        (true, true) => Some("verified, private"),
        (true, false) => Some("verified"),
        (false, true) => Some("private"),
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use snob_core::Pk;

    use super::*;

    fn person(pk: u64, username: &str, full_name: Option<&str>) -> User {
        User {
            pk: Pk::new(pk),
            username: username.to_string(),
            full_name: full_name.map(str::to_string),
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    fn shelf_of(people: &[User]) -> Shelf<'_> {
        Shelf::flat("unfollowers of @someone".to_string(), people)
    }

    fn inside(shelf: &Shelf<'_>) -> State {
        let mut state = State {
            level: Level::Inside(0),
            mode: Mode::Moving,
            query: String::new(),
            shown: Vec::new(),
            selected: 0,
            tray_selected: 0,
        };
        refilter(shelf, &mut state);
        state
    }

    fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    fn rendered(shelf: &Shelf<'_>, state: &State) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(70, 10)).unwrap();
        let mut list = TableState::default();
        list.select(Some(state.selected));
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| draw(frame, shelf, state, "", false, &mut list, &mut page_rows))
            .unwrap();
        terminal
    }

    /// The filter reads the username and the full name, ignores case, and an
    /// emptied query gives the whole list back.
    #[test]
    fn the_query_narrows_by_name_and_by_full_name() {
        let people = [
            person(1, "anna", Some("Anna Banana")),
            person(2, "bob", None),
            person(3, "carol", Some("Anna's Friend")),
        ];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        assert_eq!(state.shown, vec![0, 1, 2]);

        state.query = "ANNA".to_string();
        refilter(&shelf, &mut state);
        assert_eq!(state.shown, vec![0, 2], "case must not matter");

        state.query.clear();
        refilter(&shelf, &mut state);
        assert_eq!(state.shown, vec![0, 1, 2]);
    }

    /// Narrowing the list moves the selection to the first match: while
    /// somebody is typing, that is the row they are steering toward.
    #[test]
    fn narrowing_resets_the_selection() {
        let people = [person(1, "anna", None), person(2, "bob", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.selected = 1;

        state.query = "b".to_string();
        refilter(&shelf, &mut state);
        assert_eq!(state.selected, 0);
        assert_eq!(state.shown, vec![1]);
    }

    /// While a query is being typed, `q` is a letter of somebody's name.
    #[test]
    fn typing_a_q_into_the_filter_does_not_quit() {
        let people = [person(1, "quentin", None), person(2, "anna", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;

        let step = filtering_key(
            &shelf,
            &mut state,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert!(matches!(step, Step::Go));
        assert_eq!(state.query, "q");
        assert_eq!(state.shown, vec![0]);
        assert_eq!(state.mode, Mode::Filtering);
    }

    /// Esc empties the query and hands the keys back; Enter keeps the
    /// narrowed list. Both put the arrows back in charge.
    #[test]
    fn esc_clears_and_enter_keeps() {
        let people = [person(1, "anna", None), person(2, "bob", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;
        state.query = "b".to_string();
        refilter(&shelf, &mut state);

        filtering_key(
            &shelf,
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(state.mode, Mode::Moving);
        assert_eq!(state.shown, vec![1], "Enter keeps what was narrowed");

        state.mode = Mode::Filtering;
        filtering_key(
            &shelf,
            &mut state,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert_eq!(state.mode, Mode::Moving);
        assert_eq!(state.shown, vec![0, 1], "Esc gives the whole list back");
    }

    /// A paste is typing fast — it lands in the query, minus anything below
    /// space, so a multi-line paste cannot carry an Enter into the list.
    #[test]
    fn a_paste_lands_in_the_query_without_its_control_characters() {
        let people = [person(1, "anna", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;

        filtering(&shelf, &mut state, Raw::Paste("an\r\nna".to_string()));
        assert_eq!(state.query, "anna");
    }

    /// Ctrl+C stays an interrupt even while the query owns the keyboard.
    #[test]
    fn ctrl_c_still_interrupts_mid_query() {
        let people = [person(1, "anna", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;

        let step = filtering_key(
            &shelf,
            &mut state,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(matches!(step, Step::Leave(ExitCode::Interrupted)));
    }

    /// In a shelf with a tray, Enter walks in and Left walks back out, with
    /// the tray row remembered; on a flat shelf Back does nothing.
    #[test]
    fn the_tray_opens_and_closes_like_a_folder() {
        let a = [person(1, "anna", None)];
        let b = [person(2, "bob", None), person(3, "carol", None)];
        let shelf = Shelf {
            title: "scan of @someone".to_string(),
            sets: vec![
                Set {
                    label: "unfollowers".to_string(),
                    people: &a,
                },
                Set {
                    label: "fans".to_string(),
                    people: &b,
                },
            ],
        };
        let mut state = State {
            level: Level::Tray,
            mode: Mode::Moving,
            query: String::new(),
            shown: Vec::new(),
            selected: 1,
            tray_selected: 0,
        };

        open(&shelf, &mut state, &mut Vec::new());
        assert_eq!(state.level, Level::Inside(1));
        assert_eq!(state.shown, vec![0, 1], "the set is shown unfiltered");
        assert_eq!(state.selected, 0);

        moving(
            &shelf,
            &mut state,
            false,
            10,
            &mut Vec::new(),
            Raw::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        );
        assert_eq!(state.level, Level::Tray);
        assert_eq!(state.selected, 1, "the tray remembers where it was");
    }

    /// The heading carries the count, and the query rewrites it into a
    /// fraction so the narrowing is legible without reading the rows.
    #[test]
    fn the_heading_counts_and_the_query_rewrites_it() {
        let people = [person(1, "anna", None), person(2, "bob", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);

        let terminal = rendered(&shelf, &state);
        assert!(
            row(&terminal, 0).contains("unfollowers of @someone — 2 accounts"),
            "{}",
            row(&terminal, 0)
        );

        state.query = "b".to_string();
        refilter(&shelf, &mut state);
        let terminal = rendered(&shelf, &state);
        assert!(
            row(&terminal, 0).contains("1 of 2 match \"b\""),
            "{}",
            row(&terminal, 0)
        );
    }

    /// While the query owns the keyboard the bottom border carries it, and
    /// the cursor sits at its end — a real cursor, not a drawn one.
    #[test]
    fn the_filter_footer_carries_the_query_and_a_real_cursor() {
        let people = [person(1, "bob", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;
        state.query = "bo".to_string();
        refilter(&shelf, &mut state);

        let mut terminal = Terminal::new(TestBackend::new(70, 10)).unwrap();
        let mut list = TableState::default();
        list.select(Some(0));
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| draw(frame, &shelf, &state, "", false, &mut list, &mut page_rows))
            .unwrap();
        assert!(row(&terminal, 9).contains("/ bo"));
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            Position { x: 6, y: 9 },
            "one past the query: border, prefix, two letters"
        );
    }

    /// The badge words are the printed table's, so the browser and the table
    /// never describe one account two ways.
    #[test]
    fn the_badges_speak_the_tables_words() {
        let mut flagged = person(1, "anna", None);
        flagged.is_verified = Some(true);
        flagged.is_private = Some(true);
        assert_eq!(attributes_of(&flagged), Some("verified, private"));
        assert_eq!(attributes_of(&person(2, "bob", None)), None);
    }

    /// Column widths count display columns, not characters: a double-width
    /// name does not push the next column out of line, and the badge starts
    /// at the same x on every row.
    #[test]
    fn the_columns_line_up_even_with_double_width_names() {
        let mut wide = person(1, "大大大", None);
        wide.is_private = Some(true);
        let mut narrow = person(2, "ab", None);
        narrow.is_private = Some(true);
        let people = [wide, narrow];
        let shelf = shelf_of(&people);
        let state = inside(&shelf);
        let terminal = rendered(&shelf, &state);
        let buffer = terminal.backend().buffer();
        let column_of = |y: u16| {
            (0..buffer.area.width).find(|&x| {
                let mut text = String::new();
                for dx in 0..7u16 {
                    if x + dx < buffer.area.width {
                        text.push_str(buffer[(x + dx, y)].symbol());
                    }
                }
                text.starts_with("private")
            })
        };
        // Rows sit under border, padding and no header; both carry a badge.
        let ys: Vec<u16> = (1..9).filter(|&y| column_of(y).is_some()).collect();
        assert_eq!(ys.len(), 2, "two badge rows");
        assert_eq!(
            column_of(ys[0]),
            column_of(ys[1]),
            "the badge column moved between rows"
        );
    }

    /// Narrow terminals drop whole columns rather than clip them: the badge
    /// goes first, the username never goes.
    #[test]
    fn narrow_terminals_drop_whole_columns() {
        let mut flagged = person(1, "somebody", Some("Some Body"));
        flagged.is_private = Some(true);
        let people = [flagged];
        let shown = vec![0usize];
        let wide = columns(&people, &shown, 80);
        assert_eq!(wide.len(), 3, "username, name, badge");
        let narrow = columns(&people, &shown, 30);
        assert!(narrow.len() < 3, "something was dropped whole");
        assert!(!narrow.is_empty(), "the username survives");
    }
}
