//! The interactive profile: the account's card, and everything on it opens.
//!
//! The card is the page reduced to a screenful — who they are, how you stand
//! to each other, the counts, the highlights — with one cursor over it.
//! Up/Down move between rows, Left/Right move within the horizontal ones,
//! Enter opens what is under the cursor: the actions submenu, the stories,
//! one of the lists, a highlight. Sub-views come back to the card with
//! Backspace or Esc; `q` leaves from anywhere.
//!
//! **What a click costs is paid at the click.** The card opens on what one
//! `profile::fetch` with `MutualPolicy::Defer` already holds — about three
//! requests — and everything deeper says its price first: the mutual walk
//! runs when the number is opened, a followers or following walk is an
//! inline y/n question naming the size, and on somebody else's account that
//! question is the consent question, carried into the engine as
//! `ListQuery.yes` with the answer recorded on the [`App`].
//!
//! **Never call `ui::stories::browse` or `ui::highlights::browse` from
//! here.** Each makes a [`Tui`] of its own — a second claim on the one
//! terminal this card already holds — and each creates a [`Scratch`] at the
//! same `story_scratch()` path, which deletes the live session's files. The
//! promoted internals (`walk_in`, `keep_folder`, `open`, `keep`) are the
//! supported way in. `ui::people` is entered only through its `browse_in`,
//! which borrows this card's guard instead of building one.
//!
//! Walks are the other thing that cannot run under the browser's terminal
//! state: the progress bars belong on the real screen, where their finished
//! receipts stay in the user's scrollback, and raw mode would stairstep them
//! anyway (it clears OPOST on Unix). [`Tui::suspend`] hands the terminal
//! back before `engine::list` draws its bars; [`Tui::resume`] takes it back
//! and repaints from nothing. Everything terminal-shaped beyond that is
//! `ui::tui`'s — the guard, the chrome, and the receipts said on the real
//! screen after the alternate screen has taken the frame away.

use std::path::PathBuf;

use anyhow::Result;
use console::{Term, measure_text_width};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap,
};
use snob_core::model::{ListKind, User, printable};
use snob_store::paths::AppPaths;

use crate::app::App;
use crate::commands::common;
use crate::commands::highlights::{Entry, Tray};
use crate::commands::pfp::Picture;
use crate::commands::profile::{Highlight, Profile, Visibility, badges};
use crate::commands::stories::Story;
use crate::engine;
use crate::exit::{ExitCode, ExitError};
use crate::report;
use crate::ui::browser::input::{self, Action, Raw, TICK, action_of, page};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::highlights::{Folder, empty_note, keep_folder, stem_of, walk_in};
use crate::ui::people::{Set, Shelf};
use crate::ui::tui::{self, Tui};

/// How long a stored list may serve a click without a question. The same
/// default `--max-age` gives the list commands, for the same reason: past it
/// the stored answer stops describing now.
const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Where the browser is, and what the keys therefore drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Card,
    /// The actions submenu: 0 is the picture, 1 is the scan.
    Actions {
        selected: usize,
    },
    Stories,
    /// Inside one highlight; the selection lives in its [`Folder`].
    Inside {
        index: usize,
    },
}

/// The focusable rows of the card, in cursor order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardRow {
    Actions,
    Stories,
    FollowedBy,
    Counts,
    Highlights,
}

/// Which rows this profile has to move over. Absent parts are simply not
/// focusable: a hidden tray is not a row that says "nothing".
fn card_rows(profile: &Profile) -> Vec<CardRow> {
    let mut rows = vec![CardRow::Actions];
    if matches!(&profile.stories, Visibility::Shown(items) if !items.is_empty()) {
        rows.push(CardRow::Stories);
    }
    if profile.mutual.as_ref().is_some_and(|m| m.count > 0) {
        rows.push(CardRow::FollowedBy);
    }
    rows.push(CardRow::Counts);
    if matches!(&profile.highlights, Visibility::Shown(list) if !list.is_empty()) {
        rows.push(CardRow::Highlights);
    }
    rows
}

/// The inline y/n question in the footer, when one is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    None,
    Scan,
    List(ListKind),
}

struct State {
    level: Level,
    /// Index into [`card_rows`].
    row: usize,
    /// The selected column of the counts row: posts, followers, following.
    counts_col: usize,
    /// The selected highlight chip. Remembered across walks in and out.
    hl_col: usize,
    stories_selected: usize,
    ask: Ask,
}

/// Everything fetched once and kept for the session.
struct Media {
    stories: Vec<Story>,
    stories_opened: Vec<Option<PathBuf>>,
    tray: Tray,
    folders: Vec<Folder>,
    /// The walked mutual list, from the first click on the number.
    mutual: Option<Vec<User>>,
    pfp: Option<Picture>,
    pfp_opened: Option<PathBuf>,
}

/// What a suspended piece of work left the loop to do.
enum After {
    /// Back on the card, with this in the note line.
    Stay(String),
    /// The session is over; the terminal is already restored.
    Leave(ExitCode),
}

/// Drives the card until the user leaves it.
pub async fn browse(
    app: &mut App,
    profile: &Profile,
    typed: Option<&str>,
    paths: &AppPaths,
) -> Result<ExitCode> {
    let term = Term::stderr();
    if !term.is_term() {
        return Err(
            ExitError::new(ExitCode::Error, "--interactive needs a terminal to draw on")
                .with_hint("--no-interactive prints the profile; --format and -o shape it")
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
        .with_hint("--no-interactive prints the profile; --format and -o shape it")
    })?;

    let own = app.viewer().pk == profile.pk;
    let rows = card_rows(profile);
    let mut state = State {
        level: Level::Card,
        row: 0,
        counts_col: 0,
        hl_col: 0,
        stories_selected: 0,
        ask: Ask::None,
    };
    let mut media = Media {
        stories: match &profile.stories {
            Visibility::Shown(items) => items.clone(),
            Visibility::Hidden => Vec::new(),
        },
        stories_opened: Vec::new(),
        tray: Tray {
            username: profile.username.clone(),
            entries: match &profile.highlights {
                Visibility::Shown(list) => list.iter().map(entry_of).collect(),
                Visibility::Hidden => Vec::new(),
            },
        },
        folders: Vec::new(),
        mutual: None,
        pfp: None,
        pfp_opened: None,
    };
    media.stories_opened = vec![None; media.stories.len()];
    media.folders = media
        .tray
        .entries
        .iter()
        .map(|_| Folder {
            items: None,
            opened: Vec::new(),
            selected: 0,
        })
        .collect();

    let colors = tui::colors_enabled();
    let mut note = String::new();
    let mut receipts: Vec<String> = Vec::new();
    let mut list = TableState::default();
    let mut page_rows = 1usize;

    let outcome = loop {
        match state.level {
            Level::Stories => list.select(Some(state.stories_selected)),
            Level::Inside { index } => list.select(Some(media.folders[index].selected)),
            Level::Card | Level::Actions { .. } => {}
        }
        tui.terminal.draw(|frame| match state.level {
            Level::Card => draw_card(frame, profile, &media, &state, &rows, own, &note, colors),
            Level::Actions { selected } => {
                // The card stays underneath: the submenu acts on the account
                // the card shows, and losing sight of it helps nobody.
                draw_card(frame, profile, &media, &state, &rows, own, &note, colors);
                draw_actions_panel(frame, profile, selected, &note, colors);
            }
            Level::Stories => crate::ui::stories::draw_items_view(
                frame,
                format!(
                    "Stories · @{} · {} up",
                    printable(&profile.username),
                    media.stories.len()
                ),
                "↑↓ move · enter open · d download · ← back · q quit",
                &media.stories,
                true,
                &mut list,
                &note,
                colors,
                &mut page_rows,
            ),
            Level::Inside { index } => {
                let folder = &media.folders[index];
                let items = folder.items.as_deref().unwrap_or_default();
                crate::ui::stories::draw_items_view(
                    frame,
                    format!(
                        "@{} · {} · {} items",
                        printable(&profile.username),
                        entry_title(&media.tray.entries[index]),
                        items.len()
                    ),
                    "↑↓ move · enter open · d download · ← back · q quit",
                    items,
                    false,
                    &mut list,
                    &note,
                    colors,
                    &mut page_rows,
                );
            }
        })?;

        let event = match input::read(TICK) {
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
        let key = match event {
            Raw::Tick | Raw::Resized | Raw::Paste(_) => continue,
            Raw::Key(key) => key,
        };
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // A note stays up until the user does something else, not until the
        // next timer tick wipes it -- but never under a pending question,
        // whose branch writes its own.
        if state.ask == Ask::None {
            note.clear();
        }

        // A pending question owns the keyboard: y runs it, n or Esc declines,
        // Ctrl+C still interrupts, and everything else — `q` included — is
        // ignored rather than quietly quitting mid-question.
        if state.ask != Ask::None {
            match key.code {
                KeyCode::Char('c') if ctrl => break ExitCode::Interrupted,
                KeyCode::Char('y') | KeyCode::Char('Y') if plain => {
                    let pending = std::mem::replace(&mut state.ask, Ask::None);
                    let after = match pending {
                        Ask::List(kind) => {
                            consent_was_given(app, typed, own);
                            walk_and_open(app, &mut tui, &mut receipts, typed, profile, kind)
                                .await?
                        }
                        Ask::Scan => {
                            consent_was_given(app, typed, own);
                            scan_and_open(app, &mut tui, &mut receipts, typed, profile).await?
                        }
                        Ask::None => After::Stay(String::new()),
                    };
                    match after {
                        After::Stay(text) => note = text,
                        After::Leave(code) => break code,
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc if plain => {
                    state.ask = Ask::None;
                    note = "Nothing walked".to_string();
                }
                _ => {}
            }
            continue;
        }

        let level_before = state.level;
        match state.level {
            Level::Card => {
                let here = rows[state.row.min(rows.len() - 1)];
                // Left and Right move the column on the horizontal rows, and
                // must not reach `action_of`, whose Left is Back and whose
                // Right is Open.
                if plain && matches!(here, CardRow::Counts | CardRow::Highlights) {
                    let (col, len) = match here {
                        CardRow::Counts => (&mut state.counts_col, 3),
                        CardRow::Highlights => (&mut state.hl_col, media.tray.entries.len()),
                        _ => unreachable!(),
                    };
                    match key.code {
                        KeyCode::Left => {
                            *col = col.saturating_sub(1);
                            continue;
                        }
                        KeyCode::Right => {
                            *col = (*col + 1).min(len.saturating_sub(1));
                            continue;
                        }
                        _ => {}
                    }
                }
                match action_of(key) {
                    Action::Up => state.row = state.row.saturating_sub(1),
                    Action::Down => state.row = (state.row + 1).min(rows.len() - 1),
                    Action::First => state.row = 0,
                    Action::Last => state.row = rows.len() - 1,
                    Action::Open => match here {
                        CardRow::Actions => state.level = Level::Actions { selected: 0 },
                        CardRow::Stories => state.level = Level::Stories,
                        CardRow::FollowedBy => {
                            match followed_by_open(
                                app,
                                &mut tui,
                                &mut receipts,
                                profile,
                                &mut media,
                            )
                            .await?
                            {
                                After::Stay(text) => note = text,
                                After::Leave(code) => break code,
                            }
                        }
                        CardRow::Counts => match state.counts_col {
                            0 => note = "Posts are not browsable yet".to_string(),
                            col => {
                                let kind = if col == 1 {
                                    ListKind::Followers
                                } else {
                                    ListKind::Following
                                };
                                match list_open(
                                    app,
                                    &mut tui,
                                    &mut receipts,
                                    profile,
                                    kind,
                                    &mut state,
                                )
                                .await?
                                {
                                    After::Stay(text) => note = text,
                                    After::Leave(code) => break code,
                                }
                            }
                        },
                        CardRow::Highlights => {
                            let index = state.hl_col;
                            match walk_in(app.client(), &media.tray, index, &mut media.folders)
                                .await
                            {
                                Ok(true) => state.level = Level::Inside { index },
                                Ok(false) => note = empty_note(index),
                                Err(e) => note = format!("Could not open it: {e}"),
                            }
                        }
                    },
                    Action::Download => {
                        if here == CardRow::Highlights {
                            note = keep_folder(
                                app.client(),
                                &media.tray,
                                state.hl_col,
                                &mut media.folders,
                                &mut receipts,
                            )
                            .await;
                        }
                    }
                    Action::Redraw => tui.terminal.clear()?,
                    Action::Quit => break ExitCode::Ok,
                    Action::Interrupt => break ExitCode::Interrupted,
                    Action::Back | Action::PageUp | Action::PageDown | Action::None => {}
                }
            }
            Level::Actions { selected } => {
                // Esc closes the submenu rather than the program; `q` still
                // quits, through `action_of`.
                if plain && key.code == KeyCode::Esc {
                    state.level = Level::Card;
                    continue;
                }
                match action_of(key) {
                    Action::Up => state.level = Level::Actions { selected: 0 },
                    Action::Down => state.level = Level::Actions { selected: 1 },
                    Action::Open => {
                        if selected == 0 {
                            note = pfp_open(app, profile, &mut media, &scratch).await;
                        } else {
                            state.ask = Ask::Scan;
                            state.level = Level::Card;
                        }
                    }
                    Action::Download => {
                        if selected == 0 {
                            note = pfp_keep(app, profile, &mut media, &mut receipts).await;
                        }
                    }
                    Action::Back => state.level = Level::Card,
                    Action::Redraw => tui.terminal.clear()?,
                    Action::Quit => break ExitCode::Ok,
                    Action::Interrupt => break ExitCode::Interrupted,
                    _ => {}
                }
            }
            Level::Stories => {
                if plain && key.code == KeyCode::Esc {
                    state.level = Level::Card;
                    continue;
                }
                let total = media.stories.len();
                match action_of(key) {
                    Action::Up => state.stories_selected = state.stories_selected.saturating_sub(1),
                    Action::Down => {
                        state.stories_selected =
                            (state.stories_selected + 1).min(total.saturating_sub(1));
                    }
                    Action::PageUp => {
                        state.stories_selected =
                            state.stories_selected.saturating_sub(page(page_rows));
                    }
                    Action::PageDown => {
                        state.stories_selected =
                            (state.stories_selected + page(page_rows)).min(total.saturating_sub(1));
                    }
                    Action::First => state.stories_selected = 0,
                    Action::Last => state.stories_selected = total.saturating_sub(1),
                    Action::Open => {
                        note = match crate::ui::stories::open(
                            app.client(),
                            &profile.username,
                            &media.stories,
                            state.stories_selected,
                            &scratch,
                            &mut media.stories_opened,
                        )
                        .await
                        {
                            Ok(path) => format!("Opened {}", path.display()),
                            Err(e) => format!("Could not open it: {e}"),
                        };
                    }
                    Action::Download => {
                        note = match crate::ui::stories::keep(
                            app.client(),
                            &profile.username,
                            &media.stories,
                            state.stories_selected,
                            &mut media.stories_opened,
                        )
                        .await
                        {
                            Ok(path) => {
                                let line = format!("Saved {}", path.display());
                                receipts.push(line.clone());
                                line
                            }
                            Err(e) => format!("Could not save it: {e}"),
                        };
                    }
                    Action::Back => state.level = Level::Card,
                    Action::Redraw => tui.terminal.clear()?,
                    Action::Quit => break ExitCode::Ok,
                    Action::Interrupt => break ExitCode::Interrupted,
                    Action::None => {}
                }
            }
            Level::Inside { index } => {
                if plain && key.code == KeyCode::Esc {
                    state.level = Level::Card;
                    continue;
                }
                let stem = stem_of(&media.tray, index);
                let folder = &mut media.folders[index];
                let items = folder.items.as_deref().unwrap_or_default();
                let total = items.len();
                match action_of(key) {
                    Action::Up => folder.selected = folder.selected.saturating_sub(1),
                    Action::Down => {
                        folder.selected = (folder.selected + 1).min(total.saturating_sub(1));
                    }
                    Action::PageUp => {
                        folder.selected = folder.selected.saturating_sub(page(page_rows));
                    }
                    Action::PageDown => {
                        folder.selected =
                            (folder.selected + page(page_rows)).min(total.saturating_sub(1));
                    }
                    Action::First => folder.selected = 0,
                    Action::Last => folder.selected = total.saturating_sub(1),
                    Action::Open => {
                        note = match crate::ui::stories::open(
                            app.client(),
                            &stem,
                            items,
                            folder.selected,
                            &scratch,
                            &mut folder.opened,
                        )
                        .await
                        {
                            Ok(path) => format!("Opened {}", path.display()),
                            Err(e) => format!("Could not open it: {e}"),
                        };
                    }
                    Action::Download => {
                        note = match crate::ui::stories::keep(
                            app.client(),
                            &stem,
                            items,
                            folder.selected,
                            &mut folder.opened,
                        )
                        .await
                        {
                            Ok(path) => {
                                let line = format!("Saved {}", path.display());
                                receipts.push(line.clone());
                                line
                            }
                            Err(e) => format!("Could not save it: {e}"),
                        };
                    }
                    Action::Back => state.level = Level::Card,
                    Action::Redraw => tui.terminal.clear()?,
                    Action::Quit => break ExitCode::Ok,
                    Action::Interrupt => break ExitCode::Interrupted,
                    Action::None => {}
                }
            }
        }
        // The scroll offset belongs to the level it was scrolled at.
        if state.level != level_before {
            list = TableState::default();
        }
    };

    drop(tui);
    tui::print_receipts(&receipts);
    Ok(outcome)
}

/// One highlight of the profile as the tray type the fetchers take.
fn entry_of(h: &Highlight) -> Entry {
    Entry {
        id: h.id.clone(),
        title: h.title.clone(),
        declared_items: h.items,
        created_at: None,
        updated_at: h.updated_at,
    }
}

fn entry_title(entry: &Entry) -> String {
    if entry.title.is_empty() {
        "(untitled)".to_string()
    } else {
        entry.title.clone()
    }
}

/// The `y` was the consent, for the engine and for the next question alike.
///
/// Recorded only for somebody else's account — your own lists have nothing
/// to agree to — and only when a name was typed, which is the only case the
/// engine would ask about.
fn consent_was_given(app: &mut App, typed: Option<&str>, own: bool) {
    if !own && let Some(name) = typed {
        app.record_consent(engine::target::clean(name));
    }
}

fn list_query(typed: Option<&str>) -> engine::ListQuery {
    engine::ListQuery {
        target: typed.map(str::to_string),
        // The inline question was asked and answered before this is built;
        // without it the engine would try to prompt mid-suspension.
        yes: true,
        refresh: false,
        cache: false,
        max_age: MAX_AGE,
        no_resume: false,
        max_pages: None,
    }
}

fn subject_of(profile: &Profile) -> String {
    format!("@{}", printable(&profile.username))
}

/// Enter on followers or following: the stored list when it still answers,
/// the inline question when it would cost a walk.
async fn list_open(
    app: &mut App,
    tui: &mut Tui,
    receipts: &mut Vec<String>,
    profile: &Profile,
    kind: ListKind,
    state: &mut State,
) -> Result<After> {
    let declared = match kind {
        ListKind::Followers => profile.followers,
        ListKind::Following => profile.following,
    };
    // The counter came off the page moments ago, so comparing against it is
    // the same honesty the poll gives the list commands — without the poll.
    if let Some(people) =
        engine::freshness::fresh_members(app, profile.pk, kind, declared, MAX_AGE)?
    {
        // No walk, so no suspension: the list draws on the guard this card
        // already holds, and coming back is just the next frame.
        let shelf = Shelf::flat(format!("{kind} of {}", subject_of(profile)), &people);
        let code = crate::ui::people::browse_in(tui, &shelf, receipts)?;
        if code == ExitCode::Interrupted {
            return Ok(After::Leave(code));
        }
        return Ok(After::Stay(format!(
            "{} accounts, from the stored list",
            people.len()
        )));
    }
    state.ask = Ask::List(kind);
    Ok(After::Stay(String::new()))
}

/// The walk a `y` pays for, and the list it opens.
async fn walk_and_open(
    app: &mut App,
    tui: &mut Tui,
    receipts: &mut Vec<String>,
    typed: Option<&str>,
    profile: &Profile,
    kind: ListKind,
) -> Result<After> {
    let _ = tui.suspend();
    let subject = subject_of(profile);
    let query = list_query(typed);
    let result = common::walk_named(app, &query, kind, &subject, |_| Ok(())).await;
    app.progress().finish();
    let (people, outcome) = match result {
        Ok(pair) => pair,
        Err(e) => return stay_or_leave(app, tui, e),
    };
    if app.cancel().is_canceled() {
        return Ok(After::Leave(ExitCode::Interrupted));
    }
    tui.resume()?;
    if people.is_empty() {
        return Ok(After::Stay(format!("The {kind} list is empty")));
    }
    let shelf = Shelf::flat(format!("{kind} of {subject}"), &people);
    let code = crate::ui::people::browse_in(tui, &shelf, receipts)?;
    if code == ExitCode::Interrupted {
        return Ok(After::Leave(code));
    }
    Ok(After::Stay(format!(
        "{} accounts - {}",
        people.len(),
        report::requests(outcome.requests)
    )))
}

/// The scan a `y` pays for: both lists, crossed, as the tray `snob scan -i`
/// shows.
async fn scan_and_open(
    app: &mut App,
    tui: &mut Tui,
    receipts: &mut Vec<String>,
    typed: Option<&str>,
    profile: &Profile,
) -> Result<After> {
    let _ = tui.suspend();
    let subject = subject_of(profile);
    let query = list_query(typed);

    let first = common::walk_named(app, &query, ListKind::Followers, &subject, |outcome| {
        if outcome.is_complete() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "the followers list came back incomplete, and a crossing built on it would lie"
            ))
        }
    })
    .await;
    let (followers, followers_outcome) = match first {
        Ok(pair) => pair,
        Err(e) => return stay_or_leave(app, tui, e),
    };
    let second = common::walk_named(app, &query, ListKind::Following, &subject, |_| Ok(())).await;
    app.progress().finish();
    let (following, following_outcome) = match second {
        Ok(pair) => pair,
        Err(e) => return stay_or_leave(app, tui, e),
    };
    if app.cancel().is_canceled() {
        return Ok(After::Leave(ExitCode::Interrupted));
    }
    tui.resume()?;
    if let Err(e) = engine::cooldown::check_same_moment(&followers_outcome, &following_outcome) {
        return Ok(After::Stay(format!("Not crossed: {e}")));
    }

    let sets = [
        (
            "unfollowers",
            snob_core::sets::difference(&following, &followers),
        ),
        ("fans", snob_core::sets::difference(&followers, &following)),
        (
            "friends",
            snob_core::sets::intersection(&followers, &following),
        ),
        ("followers", followers.clone()),
        ("following", following.clone()),
    ];
    let shelf = Shelf {
        title: format!("scan of {subject}"),
        sets: sets
            .iter()
            .map(|(label, people)| Set {
                label: (*label).to_string(),
                people,
            })
            .collect(),
    };
    let code = crate::ui::people::browse_in(tui, &shelf, receipts)?;
    if code == ExitCode::Interrupted {
        return Ok(After::Leave(code));
    }
    Ok(After::Stay(format!(
        "Scanned - {}",
        report::requests(followers_outcome.requests + following_outcome.requests)
    )))
}

/// Enter on the followed-by number: the mutual walk on the first click, the
/// kept list after.
async fn followed_by_open(
    app: &mut App,
    tui: &mut Tui,
    receipts: &mut Vec<String>,
    profile: &Profile,
    media: &mut Media,
) -> Result<After> {
    let Some(mutual) = profile.mutual.as_ref() else {
        return Ok(After::Stay(String::new()));
    };
    if media.mutual.is_none() {
        let _ = tui.suspend();
        // No bar draws for this walk, so one line says why the terminal is
        // quiet while the pacer spaces the pages out.
        eprintln!("walking the accounts you follow that follow them...");
        let walked = crate::commands::profile::mutuals(
            app.client(),
            profile.pk,
            &profile.username,
            mutual.count,
            mutual.preview.clone(),
        )
        .await;
        app.progress().finish();
        match walked {
            Ok(list) => media.mutual = Some(list.people),
            Err(e) => return stay_or_leave(app, tui, e),
        }
        if app.cancel().is_canceled() {
            return Ok(After::Leave(ExitCode::Interrupted));
        }
        tui.resume()?;
    }
    let people = media.mutual.as_deref().unwrap_or_default();
    if people.is_empty() {
        return Ok(After::Stay("The mutual list came back empty".to_string()));
    }
    let shelf = Shelf::flat(format!("followed by, of {}", subject_of(profile)), people);
    let code = crate::ui::people::browse_in(tui, &shelf, receipts)?;
    if code == ExitCode::Interrupted {
        return Ok(After::Leave(code));
    }
    Ok(After::Stay(format!("{} accounts you follow", people.len())))
}

/// A failure while suspended: back on the card with the reason in the note —
/// unless the user interrupted, which ends the session with the terminal
/// already restored.
fn stay_or_leave(app: &App, tui: &mut Tui, error: anyhow::Error) -> Result<After> {
    if app.cancel().is_canceled() {
        return Ok(After::Leave(ExitCode::Interrupted));
    }
    tui.resume()?;
    Ok(After::Stay(format!("Could not walk it: {error}")))
}

/// Enter on the picture row: fetched once, then the story browser's open.
async fn pfp_open(app: &App, profile: &Profile, media: &mut Media, scratch: &Scratch) -> String {
    if let Err(e) = ensure_pfp(app, profile, media).await {
        return format!("Could not fetch the picture: {e}");
    }
    let Some(picture) = media.pfp.as_ref() else {
        return String::new();
    };
    match crate::ui::pfp::open(picture, scratch, &mut media.pfp_opened) {
        Ok(path) => format!("Opened {}", path.display()),
        Err(e) => format!("Could not open it: {e}"),
    }
}

/// D on the picture row: the very file `snob pfp` writes, where the user is.
async fn pfp_keep(
    app: &App,
    profile: &Profile,
    media: &mut Media,
    receipts: &mut Vec<String>,
) -> String {
    if let Err(e) = ensure_pfp(app, profile, media).await {
        return format!("Could not fetch the picture: {e}");
    }
    let Some(picture) = media.pfp.as_ref() else {
        return String::new();
    };
    match crate::ui::pfp::keep(picture) {
        Ok(path) => {
            let line = format!("Saved {}", path.display());
            receipts.push(line.clone());
            line
        }
        Err(e) => format!("Could not save it: {e}"),
    }
}

async fn ensure_pfp(app: &App, profile: &Profile, media: &mut Media) -> Result<()> {
    if media.pfp.is_none() {
        media.pfp = Some(
            crate::commands::pfp::picture(
                app.client(),
                profile.pk,
                &profile.username,
                profile.pfp_url.clone(),
            )
            .await?,
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Frames. Pure rendering: state in, cells out, no terminal asked anything.

/// The widest the card grows. Past this a line of facts stops being a card
/// and starts being a banner; on a wide terminal the card centers instead.
const CARD_MAX_W: u16 = 96;

/// The card's size: at most [`CARD_MAX_W`] wide, as tall as its content —
/// borders, padding, the about block, a blank line, and the focusable rows
/// with a blank line between each pair.
fn card_size(profile: &Profile, rows: &[CardRow], area: Rect) -> (u16, u16) {
    let width = area.width.min(CARD_MAX_W);
    let text_w = width.saturating_sub(2 + 4) as usize;
    let body: u16 = rows.iter().map(row_height).sum::<u16>() + rows.len().saturating_sub(1) as u16;
    let height = (2 + 2 + about_height(profile, text_w) + 1 + body).min(area.height);
    (width, height)
}

fn row_height(row: &CardRow) -> u16 {
    if *row == CardRow::Counts { 2 } else { 1 }
}

/// The about block's lines: the full name in bold, the category dim, the
/// bio's first line plain — it is the one thing here the person wrote, and
/// the least aside-like fact on the card — and the link dim.
fn about_lines(profile: &Profile, width: usize, colors: bool) -> Vec<Line<'static>> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    if let Some(name) = &profile.full_name {
        lines.push(Line::from(if colors {
            Span::styled(name.clone(), bold)
        } else {
            Span::raw(name.clone())
        }));
    }
    if let Some(category) = &profile.category {
        lines.push(Line::from(if colors {
            Span::styled(category.clone(), dim)
        } else {
            Span::raw(category.clone())
        }));
    }
    if let Some(bio) = &profile.biography
        && let Some(first) = bio.lines().next()
        && !first.is_empty()
    {
        for line in wrapped(first, width, 2) {
            lines.push(Line::from(line));
        }
    }
    if let Some(url) = &profile.external_url {
        lines.push(Line::from(if colors {
            Span::styled(url.clone(), dim)
        } else {
            Span::raw(url.clone())
        }));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

fn about_height(profile: &Profile, width: usize) -> u16 {
    about_lines(profile, width, false).len() as u16
}

/// Greedy word wrap to at most `max_lines`, in display columns. Pure, so the
/// card's height can be computed without drawing.
fn wrapped(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if measure_text_width(&candidate) <= width {
            current = candidate;
        } else {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            if lines.len() == max_lines {
                current.clear();
                break;
            }
            current = word.to_string();
        }
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines.truncate(max_lines);
    lines
}

/// Draws the card: identity on the border, an about block, the focusable
/// rows each in its own band with air between them, one voice at the bottom
/// — and, when a question is pending, the question as a modal over all of
/// it, in the middle of the screen where the eyes are.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn draw_card(
    frame: &mut Frame<'_>,
    profile: &Profile,
    media: &Media,
    state: &State,
    rows: &[CardRow],
    own: bool,
    note: &str,
    colors: bool,
) {
    let area = frame.area();
    let here = rows[state.row.min(rows.len() - 1)];
    let dim = Style::new().add_modifier(Modifier::DIM);

    let (w, h) = card_size(profile, rows, area);
    let card = area.centered(Constraint::Length(w), Constraint::Length(h));

    // The identity on the border: the handle bold on the left, what the
    // account is and how you stand to it dim on the right.
    let mut attrs: Vec<&str> = badges(profile);
    attrs.extend(relation_words(profile));
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(tui::card_padding())
        .title_top(Line::from(if colors {
            Span::styled(
                format!(" @{} ", printable(&profile.username)),
                Style::new().add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw(format!(" @{} ", printable(&profile.username)))
        }));
    if colors {
        block = block.border_style(dim);
    }
    if !attrs.is_empty() {
        let text = format!(" {} ", attrs.join(" · "));
        block = block.title_top(
            Line::from(if colors {
                Span::styled(text, dim)
            } else {
                Span::raw(text)
            })
            .right_aligned(),
        );
    }
    block = block.title_bottom(card_footer_line(here, note, colors));
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let width = inner.width as usize;

    // The vertical bands, built from the rows this profile has: an absent
    // part is not a band that says "nothing".
    let about = about_lines(profile, width, colors);
    let mut constraints = vec![
        Constraint::Length(about.len() as u16),
        Constraint::Length(1),
    ];
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Length(row_height(row)));
    }
    constraints.push(Constraint::Fill(1));
    let bands = Layout::vertical(constraints).split(inner);
    frame.render_widget(Paragraph::new(about).wrap(Wrap { trim: false }), bands[0]);

    // Band 0 is the about block, band 1 the blank under it; each row lands
    // on every second band after that.
    for (i, row) in rows.iter().enumerate() {
        let band = bands[2 + i * 2];
        match row {
            CardRow::Actions => draw_row_button(
                frame,
                band,
                "actions on this account",
                None,
                here == CardRow::Actions,
                colors,
            ),
            CardRow::Stories => {
                let n = media.stories.len();
                let label = if n == 1 {
                    "1 story up".to_string()
                } else {
                    format!("{n} stories up")
                };
                draw_row_button(frame, band, &label, None, here == CardRow::Stories, colors);
            }
            CardRow::FollowedBy => {
                let Some(mutual) = profile.mutual.as_ref() else {
                    continue;
                };
                let aside = if mutual.preview.is_empty() {
                    None
                } else {
                    Some(mutual.preview.join(", "))
                };
                draw_row_button(
                    frame,
                    band,
                    &format!("followed by {} you follow", grouped(mutual.count)),
                    aside.as_deref(),
                    here == CardRow::FollowedBy,
                    colors,
                );
            }
            CardRow::Counts => draw_counts(
                frame,
                band,
                profile,
                here == CardRow::Counts,
                state.counts_col,
                colors,
            ),
            CardRow::Highlights => {
                let labels: Vec<String> = media
                    .tray
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, entry)| {
                        let count = media.folders[i]
                            .items
                            .as_ref()
                            .map(|items| items.len() as u64)
                            .or(entry.declared_items);
                        match count {
                            Some(n) => format!("{} · {n}", entry_title(entry)),
                            None => entry_title(entry),
                        }
                    })
                    .collect();
                let selected = (here == CardRow::Highlights).then_some(state.hl_col);
                frame.render_widget(
                    Paragraph::new(chip_row("Highlights  ", &labels, selected, width, colors)),
                    band,
                );
            }
        }
    }

    // The question, over everything and where the eyes are. Drawn last: the
    // veil dims what is already on screen, and the modal's Clear keeps the
    // veil out of the modal.
    if state.ask != Ask::None {
        tui::veil(frame, colors);
        draw_question(frame, state.ask, own, profile, colors);
    }
}

/// One focusable row: a label, an optional dim aside, and a dim `›` at the
/// right edge saying "this opens". The selection covers the whole band, the
/// way a row of a list would, not just the text.
fn draw_row_button(
    frame: &mut Frame<'_>,
    band: Rect,
    label: &str,
    aside: Option<&str>,
    focused: bool,
    colors: bool,
) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let marker = if focused && !colors { ">" } else { " " };
    let mut spans = vec![Span::raw(format!("{marker} {label}"))];
    if let Some(aside) = aside {
        let text = format!("   {aside}");
        spans.push(if colors {
            Span::styled(text, dim)
        } else {
            Span::raw(text)
        });
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), band);
    frame.render_widget(
        Paragraph::new(
            Line::from(if colors {
                Span::styled("› ", dim)
            } else {
                Span::raw("› ")
            })
            .right_aligned(),
        ),
        band,
    );
    if focused && colors {
        frame.buffer_mut().set_style(band, tui::selection());
    }
}

/// The counters as three tiles: the label dim on top, the number bold under
/// it, and the focused tile inverted whole — `column_highlight_style` covers
/// the full height of the rows area, which is exactly the two lines.
fn draw_counts(
    frame: &mut Frame<'_>,
    band: Rect,
    profile: &Profile,
    focused: bool,
    col: usize,
    colors: bool,
) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let tiles = [
        ("posts", profile.posts),
        ("followers", profile.followers),
        ("following", profile.following),
    ];
    // One width for all three: the widest of the six texts, plus a blank
    // cell each side so the inverted tile reads as a button.
    let w = tiles
        .iter()
        .flat_map(|(label, n)| [label.len(), counted(*n).len()])
        .max()
        .unwrap_or(6) as u16
        + 2;

    let row = Row::new(tiles.iter().enumerate().map(|(i, (label, n))| {
        let label_line = if colors {
            Line::from(Span::styled((*label).to_string(), dim)).centered()
        } else {
            let marker = if focused && i == col { ">" } else { " " };
            Line::from(format!("{marker}{label}")).centered()
        };
        let count_line = if colors {
            Line::from(Span::styled(counted(*n), bold)).centered()
        } else {
            Line::from(counted(*n)).centered()
        };
        Cell::from(Text::from(vec![label_line, count_line]))
    }))
    .height(2);

    let mut ts = TableState::default();
    if focused && colors {
        ts.select_column(Some(col));
    }
    frame.render_stateful_widget(
        Table::new([row], [Constraint::Length(w); 3])
            .column_spacing(3)
            .column_highlight_style(tui::selection()),
        band,
        &mut ts,
    );
}

/// "1,234", or "?" when Instagram did not say.
fn counted(n: Option<u64>) -> String {
    n.map(grouped).unwrap_or_else(|| "?".to_string())
}

/// The bottom edge: the note when there is one, the hints otherwise. The
/// question is not here any more — it is a modal, where the eyes are.
fn card_footer_line(here: CardRow, note: &str, colors: bool) -> Line<'static> {
    if !note.is_empty() {
        return tui::outcome_line(note.to_string(), colors);
    }
    let mut hint = String::from("↑↓ move · ←→ pick · enter open");
    if here == CardRow::Highlights {
        hint.push_str(" · d save all of it");
    }
    hint.push_str(" · q quit");
    tui::hint_line(hint, colors)
}

/// What the modal needs to say no twice and yes once, for measuring.
const ANSWER_WORDS: &str = "y  walk it now    n  leave it    esc = no";
const MODAL_MIN_W: u16 = 46;
const MODAL_MAX_W: u16 = 72;

/// The size a question asks for: wide enough for the question or the answer
/// row plus padding and borders, bounded both ways; tall enough for the
/// wrapped question, a blank line, and the answers.
fn question_size(question: &str, area: Rect) -> (u16, u16) {
    let content = measure_text_width(question).max(measure_text_width(ANSWER_WORDS)) as u16;
    let width = (content + 6)
        .clamp(MODAL_MIN_W, MODAL_MAX_W)
        .min(area.width);
    let text_w = width.saturating_sub(6) as usize;
    let lines = wrapped(question, text_w, 3).len() as u16;
    let height = (2 + 2 + lines + 1 + 1).min(area.height);
    (width, height)
}

/// The pending question as a modal: double border — the only double in the
/// program, the stroke that says "on top" without naming a color — over a
/// `Clear`, at the optical center.
///
/// No default answer on purpose: `ui::confirm` prints `[Y/n]`, but a walk of
/// three hundred pages must not have an Enter that quietly starts it.
fn draw_question(frame: &mut Frame<'_>, ask: Ask, own: bool, profile: &Profile, colors: bool) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let question = ask_wording(ask, own, profile);
    let (w, h) = question_size(&question, frame.area());
    let area = tui::modal_area(frame.area(), w, h);
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .padding(tui::card_padding())
        .title_top(
            Line::from(if colors {
                Span::styled(" a question ", dim)
            } else {
                Span::raw(" a question ")
            })
            .centered(),
        );
    let inner = tui::open_modal(frame, area, block);

    let [text_band, _, answer_band] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(if colors {
            Line::from(Span::styled(question, bold))
        } else {
            Line::from(question)
        })
        .wrap(Wrap { trim: false }),
        text_band,
    );
    let [yes, no] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(answer_band);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            if colors {
                Span::styled("y", bold)
            } else {
                Span::raw("y")
            },
            Span::raw("  walk it now"),
        ])),
        yes,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            if colors {
                Span::styled("n", bold)
            } else {
                Span::raw("n")
            },
            Span::raw("  leave it"),
            if colors {
                Span::styled("    esc = no", dim)
            } else {
                Span::raw("    esc = no")
            },
        ])),
        no,
    );
}

/// What a `y` would spend, named before it is spent.
fn ask_wording(ask: Ask, own: bool, profile: &Profile) -> String {
    let whose = if own {
        "your".to_string()
    } else {
        format!("@{}'s", printable(&profile.username))
    };
    match ask {
        Ask::List(kind) => {
            let count = match kind {
                ListKind::Followers => profile.followers,
                ListKind::Following => profile.following,
            };
            let count = count
                .map(|n| format!("{} ", grouped(n)))
                .unwrap_or_default();
            format!("Walk {whose} {count}{kind} now?")
        }
        Ask::Scan => format!("Walk and cross {whose} followers and following now?"),
        Ask::None => String::new(),
    }
}

/// The actions submenu as a panel over the card: the card stays visible
/// under the veil, because the submenu acts on the account it shows.
fn draw_actions_panel(
    frame: &mut Frame<'_>,
    profile: &Profile,
    selected: usize,
    note: &str,
    colors: bool,
) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    tui::veil(frame, colors);

    let rows = [
        ("profile picture", "look at it or save it"),
        ("scan", "followers and following, crossed"),
    ];
    let name_w = 16u16;
    let what_w = rows.iter().map(|(_, what)| what.len()).max().unwrap_or(20) as u16;
    let w = (2 + 4 + 2 + name_w + 2 + what_w)
        .clamp(44, 72)
        .min(frame.area().width);
    let area = tui::modal_area(frame.area(), w, 6);

    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(tui::card_padding())
        .title_top(
            Line::from(if colors {
                Span::styled(
                    format!(" actions on @{} ", printable(&profile.username)),
                    dim,
                )
            } else {
                Span::raw(format!(" actions on @{} ", printable(&profile.username)))
            })
            .centered(),
        )
        .title_bottom(if note.is_empty() {
            tui::hint_line(
                "enter open · d save the picture here · esc back".into(),
                colors,
            )
        } else {
            tui::outcome_line(note.to_string(), colors)
        });
    if colors {
        block = block.border_style(dim);
    }
    let inner = tui::open_modal(frame, area, block);

    let mut ts = TableState::default();
    ts.select(Some(selected));
    frame.render_stateful_widget(
        Table::new(
            rows.iter().map(|(name, what)| {
                Row::new(vec![
                    Cell::from((*name).to_string()),
                    if colors {
                        Cell::from((*what).to_string()).style(dim)
                    } else {
                        Cell::from((*what).to_string())
                    },
                ])
            }),
            [Constraint::Length(name_w), Constraint::Fill(1)],
        )
        .column_spacing(2)
        .row_highlight_style(tui::selection())
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always),
        inner,
        &mut ts,
    );
}

/// One chip. Focused: the text with a blank cell each side, inverted — the
/// button a terminal knows how to draw, no brackets needed. Unfocused: the
/// text plain. The `>` is what is left when there is no styling at all.
fn chip(text: &str, focused: bool, colors: bool) -> Span<'static> {
    if focused {
        if colors {
            Span::styled(format!(" {text} "), tui::selection())
        } else {
            Span::raw(format!(">{text} "))
        }
    } else {
        Span::raw(format!(" {text} "))
    }
}

/// Which chips fit: the smallest window that keeps `selected` whole, grown
/// rightward while there is room. Widths are display columns; `sep` is what
/// sits between two chips.
fn chip_window(
    widths: &[usize],
    sep: usize,
    selected: usize,
    width: usize,
) -> std::ops::Range<usize> {
    if widths.is_empty() {
        return 0..0;
    }
    let selected = selected.min(widths.len() - 1);
    let mut start = selected;
    let mut used = widths[selected];
    while start > 0 && used + sep + widths[start - 1] <= width {
        start -= 1;
        used += sep + widths[start];
    }
    let mut end = selected + 1;
    while end < widths.len() && used + sep + widths[end] <= width {
        used += sep + widths[end];
        end += 1;
    }
    start..end
}

/// One physical row of chips, windowed to `width`, with a dim marker at
/// either end that has chips cut off. The plain labels are measured; the
/// styling lives in spans, so it never counts as columns. Three spaces
/// between chips: with the focused chip an inverted block, the gap alone
/// separates, and a glyph would fight the `·` inside the labels.
fn chip_row(
    prefix: &str,
    labels: &[String],
    selected: Option<usize>,
    width: usize,
    colors: bool,
) -> Line<'static> {
    // Each chip costs its label plus the blank cell each side.
    let widths: Vec<usize> = labels.iter().map(|l| measure_text_width(l) + 2).collect();
    // One column for `‹`, two for `. ›`.
    let room = width
        .saturating_sub(measure_text_width(prefix))
        .saturating_sub(3);
    let window = chip_window(&widths, 3, selected.unwrap_or(0), room);

    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut spans: Vec<Span<'static>> = vec![Span::raw(prefix.to_string())];
    if window.start > 0 {
        spans.push(if colors {
            Span::styled("‹", dim)
        } else {
            Span::raw("‹")
        });
    }
    for index in window.clone() {
        if index > window.start {
            spans.push(Span::raw("   "));
        }
        // A terminal too narrow for even one chip gets that chip cut to fit
        // rather than a row the screen clamps mid-frame.
        let label = if window.len() == 1 && widths[index] > room {
            console::truncate_str(&labels[index], room.saturating_sub(3), "…").into_owned()
        } else {
            labels[index].clone()
        };
        spans.push(chip(&label, selected == Some(index), colors));
    }
    if window.end < labels.len() {
        spans.push(if colors {
            Span::styled(" ›", dim)
        } else {
            Span::raw(" ›")
        });
    }
    Line::from(spans)
}

/// "1,234" — the counts the way the page groups them.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The header's short words for how the two accounts stand to each other.
///
/// Attribute-shaped on purpose — they sit beside "verified, private" — where
/// the printed document says the same thing as a sentence.
fn relation_words(profile: &Profile) -> Vec<&'static str> {
    let mut words = Vec::new();
    if let Some(r) = profile.relation {
        if r.you_follow {
            words.push("you follow them");
        } else if r.you_requested {
            words.push("you requested");
        }
        if r.follows_you {
            words.push("follows you");
        } else if r.they_requested {
            words.push("they requested");
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use snob_core::{Epoch, Pk};

    use super::*;

    fn bare_profile() -> Profile {
        Profile {
            pk: Pk::new(7),
            username: "someone".to_string(),
            full_name: Some("Some One".to_string()),
            biography: None,
            external_url: None,
            is_private: false,
            is_verified: true,
            category: None,
            followers: Some(1234),
            following: Some(567),
            posts: Some(128),
            relation: Some(crate::commands::profile::Relation {
                you_follow: true,
                follows_you: false,
                you_requested: false,
                they_requested: true,
            }),
            mutual: Some(crate::commands::profile::Mutual {
                count: 33,
                preview: vec!["ana".to_string()],
                people: Vec::new(),
                complete: false,
            }),
            highlights: Visibility::Shown(vec![Highlight {
                id: "highlight:1".to_string(),
                title: "trip".to_string(),
                items: Some(5),
                updated_at: None,
            }]),
            pfp_url: None,
            stories: Visibility::Shown(vec![]),
            read_at: Epoch::new(0),
        }
    }

    /// Absent parts are not focusable rows: nothing to open is nothing to
    /// land on.
    #[test]
    fn the_card_only_offers_what_the_profile_has() {
        let full = {
            let mut p = bare_profile();
            p.stories = Visibility::Shown(vec![story()]);
            p
        };
        assert_eq!(
            card_rows(&full),
            vec![
                CardRow::Actions,
                CardRow::Stories,
                CardRow::FollowedBy,
                CardRow::Counts,
                CardRow::Highlights
            ]
        );

        let mut hidden = bare_profile();
        hidden.stories = Visibility::Hidden;
        hidden.highlights = Visibility::Hidden;
        hidden.mutual = None;
        assert_eq!(
            card_rows(&hidden),
            vec![CardRow::Actions, CardRow::Counts],
            "a hidden tray and no mutuals leave nothing to open there"
        );

        let mut none_in_common = bare_profile();
        none_in_common.mutual.as_mut().unwrap().count = 0;
        assert!(!card_rows(&none_in_common).contains(&CardRow::FollowedBy));
    }

    fn story() -> Story {
        Story {
            kind: crate::commands::stories::Kind::Photo,
            taken_at: Epoch::new(0),
            expiring_at: None,
            url: None,
            mentions: Vec::new(),
        }
    }

    /// The window always holds the selected chip whole, and never overruns
    /// the width it was given.
    #[test]
    fn the_chip_window_keeps_the_selection_visible_and_fits() {
        let widths = [10, 8, 12, 6, 9];
        for selected in 0..widths.len() {
            for width in [12, 20, 30, 100] {
                let window = chip_window(&widths, 1, selected, width);
                assert!(
                    window.contains(&selected),
                    "selected {selected} fell out of {window:?} at width {width}"
                );
                let used: usize = window.clone().map(|i| widths[i]).sum::<usize>()
                    + window.len().saturating_sub(1);
                assert!(
                    used <= width || window.len() == 1,
                    "window {window:?} uses {used} of {width}"
                );
            }
        }
        // A width too small for even one chip still shows the selected one.
        assert_eq!(chip_window(&widths, 1, 2, 3), 2..3);
        assert_eq!(chip_window(&[], 1, 0, 10), 0..0);
    }

    /// The rendered strip never claims more columns than the terminal has:
    /// the labels are measured plain, the styling lives in spans.
    #[test]
    fn the_chip_row_measures_within_the_width() {
        let labels: Vec<String> = (0..8).map(|i| format!("highlight-{i} · 12")).collect();
        for width in [30, 50, 80] {
            let row = chip_row("Highlights: ", &labels, Some(4), width, false);
            assert!(
                row.width() <= width,
                "{} columns into {width}: {row}",
                row.width()
            );
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.contains("highlight"), "the selection fell off: {text}");
        }
        // With room, the selected chip is whole; without, it is cut to fit
        // rather than left to the screen's clamp mid-frame.
        let wide: String = chip_row("Highlights: ", &labels, Some(4), 80, false)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(wide.contains("highlight-4 · 12"));
        let narrow: String = chip_row("Highlights: ", &labels, Some(4), 30, false)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(narrow.contains('…'));
    }

    #[test]
    fn the_counts_group_their_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(128), "128");
        assert_eq!(grouped(1234), "1,234");
        assert_eq!(grouped(1234567), "1,234,567");
    }

    /// The header's relation words are attribute-shaped: the follow that
    /// exists is said, the request stands in only while nothing is settled.
    #[test]
    fn the_relation_reads_as_attributes() {
        let p = bare_profile();
        assert_eq!(
            relation_words(&p),
            vec!["you follow them", "they requested"]
        );

        let mut own = bare_profile();
        own.relation = None;
        assert!(relation_words(&own).is_empty());
    }

    /// The question names whose list and how big before anything is spent.
    #[test]
    fn the_question_names_the_size_and_the_owner() {
        let p = bare_profile();
        assert_eq!(
            ask_wording(Ask::List(ListKind::Followers), false, &p),
            "Walk @someone's 1,234 followers now?"
        );
        assert_eq!(
            ask_wording(Ask::List(ListKind::Following), true, &p),
            "Walk your 567 following now?"
        );
        assert_eq!(
            ask_wording(Ask::Scan, true, &p),
            "Walk and cross your followers and following now?"
        );
        let mut unknown = bare_profile();
        unknown.followers = None;
        assert_eq!(
            ask_wording(Ask::List(ListKind::Followers), false, &unknown),
            "Walk @someone's followers now?"
        );
    }

    /// A profile highlight becomes the tray type without inventing a date.
    #[test]
    fn a_profile_highlight_becomes_a_tray_entry() {
        let entry = entry_of(&Highlight {
            id: "highlight:9".to_string(),
            title: String::new(),
            items: Some(3),
            updated_at: Some(Epoch::new(5)),
        });
        assert_eq!(entry.id, "highlight:9");
        assert_eq!(entry.declared_items, Some(3));
        assert_eq!(entry.created_at, None);
        assert_eq!(entry.updated_at, Some(Epoch::new(5)));
        assert_eq!(entry_title(&entry), "(untitled)");
    }

    fn media_for(profile: &Profile) -> Media {
        Media {
            stories: match &profile.stories {
                Visibility::Shown(items) => items.clone(),
                Visibility::Hidden => Vec::new(),
            },
            stories_opened: vec![None],
            tray: Tray {
                username: profile.username.clone(),
                entries: match &profile.highlights {
                    Visibility::Shown(list) => list.iter().map(entry_of).collect(),
                    Visibility::Hidden => Vec::new(),
                },
            },
            folders: vec![Folder {
                items: None,
                opened: Vec::new(),
                selected: 0,
            }],
            mutual: None,
            pfp: None,
            pfp_opened: None,
        }
    }

    fn card_rendered(
        profile: &Profile,
        media: &Media,
        state: &State,
        rows: &[CardRow],
        note: &str,
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw_card(frame, profile, media, state, rows, false, note, false))
            .unwrap();
        terminal
    }

    fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    /// The card is a centered column: identity on its top border, the
    /// person's name and the focusable rows inside with air between them,
    /// and one voice on the bottom edge — the note, or the hints.
    #[test]
    fn the_card_is_a_centered_column_with_one_voice_at_the_bottom() {
        let mut profile = bare_profile();
        profile.stories = Visibility::Shown(vec![story()]);
        let rows = card_rows(&profile);
        let media = media_for(&profile);
        let state = State {
            level: Level::Card,
            row: 0,
            counts_col: 0,
            hl_col: 0,
            stories_selected: 0,
            ask: Ask::None,
        };

        let terminal = card_rendered(&profile, &media, &state, &rows, "");
        // 16 rows of card centered in 24: the border lands on row 4.
        let top = row(&terminal, 4);
        assert!(top.contains("@someone"), "{top}");
        assert!(top.contains("verified"), "{top}");
        assert!(row(&terminal, 6).contains("Some One"));
        let actions = row(&terminal, 8);
        assert!(actions.contains("actions on this account"), "{actions}");
        assert!(actions.contains('›'), "the row says it opens: {actions}");
        // The counters are tiles: labels on one row, numbers under them.
        let labels = row(&terminal, 14);
        assert!(labels.contains("posts"), "{labels}");
        assert!(labels.contains("followers"), "{labels}");
        let numbers = row(&terminal, 15);
        assert!(numbers.contains("1,234"), "{numbers}");
        assert!(numbers.contains("567"), "{numbers}");
        assert!(row(&terminal, 17).contains("trip"));
        let bottom = row(&terminal, 19);
        assert!(bottom.contains("enter open"), "{bottom}");

        let terminal = card_rendered(&profile, &media, &state, &rows, "Saved x");
        assert!(row(&terminal, 19).contains("Saved x"));
    }

    /// The question is a modal at the optical center, never a line on the
    /// bottom border — and the card is still there behind it.
    #[test]
    fn the_question_lands_where_the_eyes_are_and_the_card_stays_behind_it() {
        let mut profile = bare_profile();
        profile.stories = Visibility::Shown(vec![story()]);
        let rows = card_rows(&profile);
        let media = media_for(&profile);
        let state = State {
            level: Level::Card,
            row: 0,
            counts_col: 0,
            hl_col: 0,
            stories_selected: 0,
            ask: Ask::List(ListKind::Followers),
        };

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| draw_card(frame, &profile, &media, &state, &rows, false, "", true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text_row =
            |y: u16| -> String { (0..100u16).map(|x| buffer[(x, y)].symbol()).collect() };

        // 1. The question sits in the upper-middle band, not the last row.
        let y = (0..30u16)
            .find(|&y| text_row(y).contains("followers now?"))
            .expect("the question is on screen");
        assert!((5..=15).contains(&y), "the question landed on row {y}");
        assert!(!text_row(29).contains("now?"), "back on the bottom edge");

        // 2. Double border, centered: the question modal is the one double.
        // The question sits two rows under the modal's top border (border,
        // padding, text).
        let corners = text_row(y - 2);
        assert!(corners.contains('╔'), "{corners}");
        assert!(corners.contains('╗'), "{corners}");

        // 3. The veil dims the card; Clear keeps it out of the modal. This
        //    is what proves there is a real modal, not a line painted over.
        assert!(
            buffer[(10, 10)].modifier.contains(Modifier::DIM),
            "the background was not veiled"
        );
        let inside = (0..100u16)
            .find(|&x| text_row(y).contains("followers now?") && buffer[(x, y)].symbol() == "W")
            .unwrap_or(40);
        assert!(
            !buffer[(inside, y)].modifier.contains(Modifier::DIM),
            "the veil leaked into the modal"
        );

        // 4. The answers say what y spends, and there is no default.
        let answers = text_row(y + 2);
        assert!(answers.contains("walk it now"), "{answers}");
        assert!(answers.contains("leave it"), "{answers}");

        // 5. The card is still behind it: a modal, not a new screen.
        assert!(
            (0..30u16).any(|yy| text_row(yy).contains("@someone")),
            "the card vanished"
        );
    }

    /// The modal's size is bounded and its height follows the wrap.
    #[test]
    fn the_question_size_is_bounded_and_wraps() {
        let area = Rect::new(0, 0, 100, 30);
        let (w, h) = question_size("Walk your 567 following now?", area);
        // The answers row is what governs a short question's width.
        assert_eq!(w, 47);
        assert_eq!(h, 7);
        let long =
            "Walk and cross @averyveryverylongaccountname's followers and following right now?";
        let (w, h) = question_size(long, area);
        assert_eq!(w, MODAL_MAX_W, "long questions get the ceiling");
        assert_eq!(h, 8, "and wrap to two lines");
    }
}
