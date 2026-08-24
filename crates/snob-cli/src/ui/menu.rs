//! A short list to pick one line of with the arrow keys.
//!
//! This used to be `dialoguer::Select`; now it is the one interactive view
//! that does **not** take the alternate screen. It lives in the middle of a
//! wizard's questions — `snob login`, `snob watch setup` — so it draws in an
//! inline viewport exactly as tall as itself ([`Tui::inline`]), and on the
//! way out it clears that viewport and leaves the prompt with the answer
//! beside it, where the list was, in the flow of questions around it.
//!
//! What it keeps from the prompt it replaced: the look (a pointer on the
//! selected line, the prompt above), Enter to choose, Esc to decline, and
//! the selection stopping at the ends rather than wrapping -- a list of
//! three that wraps is one where Down on the last line lands on the first,
//! which reads as a mistake.
//!
//! Keys are read through [`input`], which is what closes the two defects that
//! module's header describes, and a paste is swallowed whole here for the
//! same reason it is in the browser: without mode 2004 it arrives as its
//! characters and some of them are commands.

use anyhow::{Context, Result};
use console::Term;
use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::browser::input::{self, Action, Next};
use super::tui::{self, Tui};

/// Draws the list of `labels` under `prompt` and waits for a choice.
///
/// `Some(index)` on Enter, `None` on Esc or `q`. Ctrl+C is an error, so the
/// caller's `?` ends the command the way every other interrupt does. The
/// caller decides whether a menu can be shown at all -- `ui::can_show_a_menu`
/// is the gate, asked before this is reached -- so this assumes a terminal on
/// both standard input and standard error.
pub fn choose(prompt: &str, labels: &[&str]) -> Result<Option<usize>> {
    let rows = u16::try_from(labels.len() + 1).unwrap_or(u16::MAX);
    let mut tui = Tui::inline(rows).context("could not read the terminal")?;
    let colors = tui::colors_enabled();
    let mut selected = 0usize;

    let outcome = loop {
        tui.terminal
            .draw(|frame| draw(frame, prompt, labels, selected, colors))?;
        match input::next(std::time::Duration::from_secs(60))? {
            Next::Tick | Next::Resized => continue,
            Next::Do(action) => match step(selected, labels.len(), action) {
                Step::Stay => continue,
                Step::Move(to) => selected = to,
                Step::Choose => break Some(selected),
                Step::Decline => break None,
                Step::Interrupt => {
                    tui.terminal.clear()?;
                    drop(tui);
                    anyhow::bail!("canceled");
                }
            },
        }
    };

    // What stays on screen: the question and its answer, where the list was.
    // `clear` empties the viewport and leaves the cursor at its first row;
    // the guard drops before anything is printed, so the line below lands in
    // a cooked terminal.
    tui.terminal.clear()?;
    drop(tui);
    let answer = match outcome {
        Some(index) => labels[index],
        None => "(none)",
    };
    Term::stderr().write_line(&format!(
        "{} {} {}",
        console::style("?").green().for_stderr(),
        prompt,
        console::style(answer).cyan().for_stderr()
    ))?;
    Ok(outcome)
}

/// Draws the prompt and one row per label into the inline viewport.
///
/// A long label is clipped at the viewport's edge by the renderer itself --
/// the cell buffer has nowhere to wrap to -- which is what the old string
/// renderer had to arrange by hand.
fn draw(frame: &mut Frame<'_>, prompt: &str, labels: &[&str], selected: usize, colors: bool) {
    let mut lines = Vec::with_capacity(labels.len() + 1);
    lines.push(Line::from(vec![
        if colors {
            Span::styled("?", Style::new().fg(Color::Green))
        } else {
            Span::raw("?")
        },
        Span::raw(format!(" {prompt}")),
    ]));
    for (index, label) in labels.iter().enumerate() {
        if index == selected {
            // The pointer keeps the accent; the label itself is bold, not
            // colored — `tui::selection` documents why a selection never
            // names a color, and this menu is no exception.
            lines.push(Line::from(vec![
                if colors {
                    Span::styled("› ", Style::new().fg(Color::Cyan))
                } else {
                    Span::raw("› ")
                },
                if colors {
                    Span::styled(
                        (*label).to_string(),
                        Style::new().add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw((*label).to_string())
                },
            ]));
        } else {
            lines.push(Line::from(format!("  {label}")));
        }
    }
    frame.render_widget(Paragraph::new(lines), frame.area());
}

/// What one key does to a selection of `len` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Stay,
    Move(usize),
    Choose,
    Decline,
    Interrupt,
}

/// The menu's half of the key binding: the browser's actions, read as a menu
/// reads them. Pure, so the ends-do-not-wrap rule has a test.
pub fn step(selected: usize, len: usize, action: Action) -> Step {
    let last = len.saturating_sub(1);
    match action {
        Action::Up if selected > 0 => Step::Move(selected - 1),
        Action::Down if selected < last => Step::Move(selected + 1),
        Action::First | Action::PageUp if selected > 0 => Step::Move(0),
        Action::Last | Action::PageDown if selected < last => Step::Move(last),
        Action::Open => Step::Choose,
        Action::Quit => Step::Decline,
        Action::Interrupt => Step::Interrupt,
        // `Download` is a browser verb; a menu has nothing to download, and
        // `d` on a menu is a typo, not a choice.
        _ => Step::Stay,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    /// The selection stops at the ends. `dialoguer` did the same, and a list
    /// of three that wraps is one where Down on the last line lands on the
    /// first, which reads as a mistake rather than a feature.
    #[test]
    fn the_selection_stops_at_the_ends_and_does_not_wrap() {
        assert_eq!(step(0, 3, Action::Up), Step::Stay);
        assert_eq!(step(0, 3, Action::Down), Step::Move(1));
        assert_eq!(step(2, 3, Action::Down), Step::Stay);
        assert_eq!(step(2, 3, Action::Up), Step::Move(1));
        assert_eq!(step(1, 3, Action::First), Step::Move(0));
        assert_eq!(step(1, 3, Action::Last), Step::Move(2));
        assert_eq!(step(0, 3, Action::First), Step::Stay);
        // A list of one has nowhere to go in either direction.
        assert_eq!(step(0, 1, Action::Down), Step::Stay);
        assert_eq!(step(0, 1, Action::Up), Step::Stay);
    }

    /// Enter chooses, Esc declines, Ctrl+C interrupts, and the browser's
    /// verbs that mean nothing on a menu do nothing.
    #[test]
    fn the_three_ways_out_and_the_keys_that_are_not_one() {
        assert_eq!(step(1, 3, Action::Open), Step::Choose);
        assert_eq!(step(1, 3, Action::Quit), Step::Decline);
        assert_eq!(step(1, 3, Action::Interrupt), Step::Interrupt);
        assert_eq!(step(1, 3, Action::Download), Step::Stay);
        assert_eq!(step(1, 3, Action::Redraw), Step::Stay);
        assert_eq!(step(1, 3, Action::None), Step::Stay);
    }

    /// The drawn rows, in the plain form a terminal without color gets: one
    /// line for the prompt, one per label, the selected one marked.
    #[test]
    fn the_rows_are_the_prompt_and_one_line_per_label() {
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal
            .draw(|frame| draw(frame, "How?", &["browser", "paste"], 1, false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(row(0), "? How?");
        assert_eq!(row(1), "  browser");
        assert_eq!(row(2), "› paste");
    }

    /// The selected label is bold, never a color: the pointer keeps the
    /// accent, and a selection that named a color would be the one place in
    /// the program contradicting `tui::selection`.
    #[test]
    fn the_selected_label_is_bold_not_colored() {
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal
            .draw(|frame| draw(frame, "How?", &["browser", "paste"], 1, true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        // Row 2 is the selected label; column 2 is its first letter.
        assert!(
            buffer[(2, 2)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(buffer[(2, 2)].fg, ratatui::style::Color::Reset);
    }

    /// A label wider than the viewport is clipped at its edge by the cell
    /// buffer itself: there is no row for it to wrap onto.
    #[test]
    fn a_long_label_is_cut_to_the_width() {
        let long = "x".repeat(100);
        let mut terminal = Terminal::new(TestBackend::new(30, 2)).unwrap();
        terminal
            .draw(|frame| draw(frame, "?", &[long.as_str()], 0, false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 30, "the buffer is the clamp");
    }
}
