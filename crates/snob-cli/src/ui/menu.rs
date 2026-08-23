//! A short list to pick one line of with the arrow keys.
//!
//! This used to be `dialoguer::Select`, which was the crate's one use in the
//! program -- every other question here is a typed line or a yes/no -- and it
//! was kept for a two-entry menu while the story browser already drew a list
//! with a highlighted row out of `crossterm` keys and `console` drawing. The
//! pieces were here; this is the menu made of them. Two crates left with it,
//! and with them the reason `ui::restore_terminal` existed in the shape it
//! had: a prompt that hides the cursor and cannot be trusted to show it again
//! on a forced exit.
//!
//! What it keeps from the prompt it replaced: the look (a pointer on the
//! selected line, the prompt above, and on the way out the prompt with the
//! answer beside it where the list was), Enter to choose, Esc to decline,
//! and the selection stopping at the ends rather than wrapping -- a list of
//! three that wraps is one where Down on the last line lands on the first,
//! which reads as a mistake.
//!
//! Keys are read through [`input`], which is what closes the two defects that
//! module's header describes, and a paste is swallowed whole here for the
//! same reason it is in the browser: without mode 2004 it arrives as its
//! characters and some of them are commands.

use anyhow::{Context, Result};
use console::Term;

use super::browser::input::{self, Action, Next};

/// Draws the list of `labels` under `prompt` and waits for a choice.
///
/// `Some(index)` on Enter, `None` on Esc or `q`. Ctrl+C is an error, so the
/// caller's `?` ends the command the way every other interrupt does. The
/// caller decides whether a menu can be shown at all -- `ui::can_show_a_menu`
/// is the gate, asked before this is reached -- so this assumes a terminal on
/// both standard input and standard error.
pub fn choose(prompt: &str, labels: &[&str]) -> Result<Option<usize>> {
    let term = Term::stderr();
    let rows = labels.len();
    let mut selected = 0usize;

    // Raw mode for the keys, restored by the guard on every way out of this
    // function; the cursor hidden while the list is up, restored by its own
    // guard for the same reason. A panic runs neither -- the release profile
    // aborts -- and `ui::restore_terminal` is the backstop for that.
    let _keys = input::Session::enter().context("could not read the terminal")?;
    let _cursor = HiddenCursor::hide(&term);

    let width = usize::from(term.size().1).max(20);
    for line in render(prompt, labels, selected, width) {
        term.write_line(&line)?;
    }

    let outcome = loop {
        match input::next(std::time::Duration::from_secs(60))? {
            Next::Tick | Next::Resized => continue,
            Next::Do(action) => match step(selected, rows, action) {
                Step::Stay => continue,
                Step::Move(to) => {
                    selected = to;
                    // Redrawn in place: up over the list, then the same rows
                    // again. `clear_last_lines` moves the cursor up and
                    // clears, which is the whole repaint for a list this size.
                    term.clear_last_lines(rows + 1)?;
                    for line in render(prompt, labels, selected, width) {
                        term.write_line(&line)?;
                    }
                }
                Step::Choose => break Some(selected),
                Step::Decline => break None,
                Step::Interrupt => {
                    term.clear_last_lines(rows + 1)?;
                    anyhow::bail!("canceled");
                }
            },
        }
    };

    // What stays on screen: the question and its answer, where the list was.
    term.clear_last_lines(rows + 1)?;
    let answer = match outcome {
        Some(index) => labels[index],
        None => "(none)",
    };
    term.write_line(&format!(
        "{} {} {}",
        console::style("?").green().for_stderr(),
        prompt,
        console::style(answer).cyan().for_stderr()
    ))?;
    Ok(outcome)
}

/// The lines the menu draws, for a test to read.
///
/// Every row is cut to the terminal width by display columns, the way the
/// browser's rows are, so a long label cannot wrap and leave a line behind
/// that `clear_last_lines` does not know about. Styling is applied only where
/// `console` says a terminal wants it, which is also what makes the plain
/// form testable: with `NO_COLOR` set the rows are exactly the text.
pub fn render(prompt: &str, labels: &[&str], selected: usize, width: usize) -> Vec<String> {
    let mut lines = Vec::with_capacity(labels.len() + 1);
    lines.push(format!(
        "{} {}",
        console::style("?").green().for_stderr(),
        console::truncate_str(prompt, width.saturating_sub(2), "…")
    ));
    for (index, label) in labels.iter().enumerate() {
        let label = console::truncate_str(label, width.saturating_sub(2), "…");
        if index == selected {
            lines.push(format!(
                "{} {}",
                console::style("›").cyan().for_stderr(),
                console::style(label).cyan().for_stderr()
            ));
        } else {
            lines.push(format!("  {label}"));
        }
    }
    lines
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

/// Hides the cursor for as long as it lives.
struct HiddenCursor<'a>(&'a Term);

impl<'a> HiddenCursor<'a> {
    fn hide(term: &'a Term) -> Self {
        let _ = term.hide_cursor();
        Self(term)
    }
}

impl Drop for HiddenCursor<'_> {
    fn drop(&mut self) {
        let _ = self.0.show_cursor();
    }
}

#[cfg(test)]
mod tests {
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
        console::set_colors_enabled_stderr(false);
        let rows = render("How?", &["browser", "paste"], 1, 80);
        assert_eq!(rows, vec!["? How?", "  browser", "› paste"]);
    }

    /// A label wider than the terminal is cut, not wrapped: a wrapped row is
    /// a line the repaint does not know about and cannot clear.
    #[test]
    fn a_long_label_is_cut_to_the_width() {
        console::set_colors_enabled_stderr(false);
        let long = "x".repeat(100);
        let rows = render("?", &[&long], 0, 30);
        for row in &rows {
            assert!(
                console::measure_text_width(row) <= 30,
                "{} columns: {row}",
                console::measure_text_width(row)
            );
        }
    }
}
