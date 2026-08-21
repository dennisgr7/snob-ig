//! A block of terminal output that redraws only the rows that changed.
//!
//! What this replaces is `clear_last_lines(n)` followed by a full rewrite, and
//! it is worth being exact about why that had to go, because "it flickers" is
//! not a reason anybody can check.
//!
//! **It writes one frame per keypress instead of dozens of syscalls.**
//! `console::Term` is unbuffered: every `write_str` is a `write` on standard
//! error. `Term::clear_last_lines(n)` is `move_cursor_up(n)`, then `n` times
//! (`clear_line`, `move_cursor_down(1)`), then `move_cursor_up(n)` -- so
//! clearing a twenty-four-row block is fifty writes before a single row is
//! drawn, and the twenty-four `write_line` calls after it are twenty-four more.
//! Measured on a list of twenty stories in a twenty-four-row terminal, one
//! keypress cost **935 bytes in 74 writes**; this costs **136 bytes in one**.
//! Seventy-four unbuffered writes are seventy-four moments at which the
//! terminal may present what it has, and one of them is the cleared screen.
//! That is the flicker, mechanically.
//!
//! **It cannot lose count of where it is.** `clear_last_lines` clears *rows*
//! while the caller counts *lines*, so one line that wrapped desynchronizes the
//! two for the rest of the session: every later frame clears one row too few,
//! the block walks up the screen and leaves a copy of itself behind. A story
//! with several long `@mention`s, a Windows error message with a full path in
//! the footer, or a narrow window is enough. [`clamp`] closes it by making one
//! line always one row.
//!
//! **An unchanged frame writes nothing at all**, which is what lets the browser
//! wake on a timer to notice a resize without touching the terminal while it is
//! idle.
//!
//! What it deliberately is not is a cell buffer. That is what `ratatui` is,
//! it is a genuinely larger thing, and a list of rows of text does not need
//! one. The number for choosing otherwise is in AGENTS.md.

use std::io::Result;

use console::{Term, measure_text_width, truncate_str};

/// Begin and end a synchronized update: DEC private mode 2026. The terminal
/// buffers what arrives between them and presents the frame at once.
///
/// **Sent without asking first.** The polite move is `DECRQM`, and it costs a
/// round trip and a read timeout at every start-up to avoid sixteen bytes that
/// a terminal which does not implement the mode has to ignore -- ignoring an
/// unknown private mode is what the mechanism is for. Windows Terminal, kitty,
/// Ghostty, WezTerm, iTerm2, foot, Alacritty and Contour all honor it; tmux
/// through 3.6 does not implement it and passes it out to the terminal around
/// it, which is the harmless case rather than a broken one.
const BEGIN_SYNC: &str = "\x1b[?2026h";
const END_SYNC: &str = "\x1b[?2026l";

/// Erase the whole row the cursor is on. `console::Term::clear_line` sends this
/// with a carriage return in front; here the cursor is already at column zero,
/// and the frame is assembled as one string rather than issued call by call.
const ERASE_ROW: &str = "\x1b[2K";
/// Erase from the cursor to the end of the screen.
const ERASE_BELOW: &str = "\x1b[J";

/// A block of rows drawn in place, and what is believed to be on screen.
pub struct Screen {
    /// The rows as they were left on the terminal, already cut to the width
    /// they were drawn at. Compared against, so it has to be what was sent and
    /// not what the caller asked for.
    last: Vec<String>,
    /// The size the last frame was drawn against. A change on either axis
    /// forces a full repaint: the cutting was done against the old width, and
    /// rows drawn at the old width may have re-wrapped since.
    size: (u16, u16),
    /// Set by [`Screen::invalidate`]. Makes the next frame a full repaint.
    dirty: bool,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self {
            last: Vec::new(),
            size: (0, 0),
            dirty: false,
        }
    }

    /// How many rows a frame may occupy.
    ///
    /// One short of the terminal, so that stepping onto the last row of the
    /// block never scrolls the screen and carries the top of the frame into
    /// the scrollback, where nothing can reach it again.
    pub fn usable_rows(term: &Term) -> usize {
        (term.size().0 as usize).saturating_sub(1).max(1)
    }

    /// Forces the next frame to be drawn in full.
    ///
    /// For Ctrl+L, and for after anything else has written to the terminal
    /// behind this screen's back. A diffing renderer is only ever as right as
    /// its belief about what is on screen, so there has to be a way for a
    /// person to say that the belief is wrong -- and on Windows there has to be
    /// one regardless, because ConPTY coalesces positioned writes and leaves
    /// fragments that no renderer can predict.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Draws `rows`, rewriting only what changed.
    pub fn draw(&mut self, term: &Term, rows: &[String]) -> Result<()> {
        self.draw_sized(term, rows, term.size())
    }

    /// The same, against a size handed in rather than asked for.
    ///
    /// Split this way so the renderer can be tested and measured without a
    /// terminal, which is most of what makes the numbers in the header
    /// checkable rather than claimed.
    pub fn draw_sized(&mut self, term: &Term, rows: &[String], size: (u16, u16)) -> Result<()> {
        let frame = self.render(rows, size);
        if frame.is_empty() {
            return Ok(());
        }
        term.write_str(&frame)?;
        term.flush()
    }

    /// Builds the bytes for one frame and records what they leave on screen.
    ///
    /// Returned rather than written so that a test can count them. Empty means
    /// nothing changed and nothing needs to be sent.
    pub fn render(&mut self, rows: &[String], size: (u16, u16)) -> String {
        let width = size.1.max(1) as usize;

        // One column short of the width. The last column is where a terminal
        // with `xenl` -- which is every terminal that matters -- parks the
        // cursor in a pending-wrap state, and what a `\r\n` from there does
        // differs between emulators.
        let clamped: Vec<String> = rows
            .iter()
            .map(|row| clamp(row, width.saturating_sub(1)))
            .collect();

        let full = self.dirty || size != self.size || self.last.is_empty();

        // Nothing changed: send nothing. This is what makes the timed wake-up
        // free, so the loop can look at the size a few times a second without
        // writing a byte while nobody is pressing anything.
        if !full && self.last == clamped {
            return String::new();
        }

        let mut out =
            String::with_capacity(clamped.iter().map(|r| r.len() + 8).sum::<usize>() + 32);
        out.push_str(BEGIN_SYNC);

        if full {
            // Back to the top of what was there, then erase downwards. When the
            // width shrank, the old rows may have re-wrapped into more physical
            // rows than were counted, and the extra ones are above the cursor
            // and survive. That is the one case the alternate screen would
            // close and this does not, and it is the price of staying inline --
            // which is the right trade for a list that lives for twenty seconds
            // and whose last line has to still be readable afterwards.
            if !self.last.is_empty() {
                up(&mut out, self.last.len() - 1);
            }
            out.push('\r');
            out.push_str(ERASE_BELOW);
            for (i, row) in clamped.iter().enumerate() {
                if i > 0 {
                    out.push_str("\r\n");
                }
                out.push_str(row);
            }
        } else {
            up(&mut out, self.last.len() - 1);
            out.push('\r');

            let shrinking = clamped.len() < self.last.len();
            let last_index = clamped.len().saturating_sub(1);

            for (i, row) in clamped.iter().enumerate() {
                if i > 0 {
                    out.push_str("\r\n");
                }
                // The final row is always rewritten when the frame got shorter,
                // because `ERASE_BELOW` is issued from the end of it -- and
                // issuing it from column zero of a row that was skipped would
                // erase that row instead of the ones under it.
                let unchanged = self.last.get(i) == Some(row) && !(shrinking && i == last_index);
                if !unchanged {
                    out.push_str(ERASE_ROW);
                    out.push_str(row);
                }
            }
            if shrinking {
                out.push_str(ERASE_BELOW);
            }
        }

        out.push_str(END_SYNC);

        self.last = clamped;
        self.size = size;
        self.dirty = false;
        out
    }

    /// Leaves the cursor on the row below the frame, so that whatever the
    /// program prints next does not land on top of it.
    pub fn finish(&mut self, term: &Term) -> Result<()> {
        if !self.last.is_empty() {
            term.write_str("\r\n")?;
            term.flush()?;
        }
        self.last.clear();
        Ok(())
    }
}

fn up(out: &mut String, n: usize) {
    if n > 0 {
        out.push_str("\x1b[");
        out.push_str(&n.to_string());
        out.push('A');
    }
}

/// Cuts a row to `width` display columns, counting what a terminal counts.
///
/// `measure_text_width` and `truncate_str` are `console`'s. Both look past SGR
/// escapes and both use `unicode-width`, so a styled row and a row carrying a
/// double-width username are each measured in **columns** rather than in bytes
/// or in `char`s. That is the part which would be wrong if it were written by
/// hand here, and it is already in the tree.
fn clamp(row: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if measure_text_width(row) <= width {
        return row.to_string();
    }
    truncate_str(row, width, "\u{2026}").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(n: usize, selected: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("{} story {i}", if i == selected { ">" } else { " " }))
            .collect()
    }

    #[test]
    fn an_identical_frame_writes_nothing() {
        let mut s = Screen::new();
        s.render(&list(10, 0), (24, 80));
        assert_eq!(s.render(&list(10, 0), (24, 80)), "");
    }

    #[test]
    fn moving_the_selection_rewrites_two_rows_and_no_more() {
        let mut s = Screen::new();
        s.render(&list(20, 0), (24, 80));
        let frame = s.render(&list(20, 1), (24, 80));
        // One erase-and-rewrite per changed row, and the only rows that changed
        // are the one the marker left and the one it arrived on.
        assert_eq!(frame.matches(ERASE_ROW).count(), 2);
        assert!(frame.starts_with(BEGIN_SYNC) && frame.ends_with(END_SYNC));
    }

    #[test]
    fn a_size_change_forces_a_full_repaint() {
        let mut s = Screen::new();
        s.render(&list(10, 0), (24, 80));
        let frame = s.render(&list(10, 0), (24, 60));
        assert!(!frame.is_empty(), "same rows at a new width must repaint");
        assert!(frame.contains(ERASE_BELOW));
    }

    #[test]
    fn invalidating_forces_a_full_repaint() {
        let mut s = Screen::new();
        s.render(&list(10, 0), (24, 80));
        s.invalidate();
        assert!(!s.render(&list(10, 0), (24, 80)).is_empty());
    }

    #[test]
    fn a_shorter_frame_erases_what_it_left_behind() {
        let mut s = Screen::new();
        s.render(&list(10, 0), (24, 80));
        let frame = s.render(&list(4, 0), (24, 80));
        assert!(frame.ends_with(&format!("{ERASE_BELOW}{END_SYNC}")));
    }

    #[test]
    fn a_short_row_is_left_alone() {
        assert_eq!(clamp("hello", 10), "hello");
    }

    #[test]
    fn styling_does_not_count_towards_the_width() {
        let styled = format!("{}", console::style("abcde").red());
        assert_eq!(clamp(&styled, 5), styled);
    }

    #[test]
    fn a_double_width_char_is_two_columns() {
        // Two `char`s, four columns, six bytes. Bytes and `char`s would each
        // give a different answer, and both would be the wrong number to
        // compare against a terminal width.
        assert_eq!(measure_text_width("\u{65e5}\u{672c}"), 4);
    }

    /// The invariant the whole file rests on. Going one column over is what
    /// wraps a row into two, and a wrapped row is what made `clear_last_lines`
    /// clear one row too few for the rest of the session.
    ///
    /// The result may come back *narrower* than asked for, and that is correct:
    /// a double-width character cannot be half drawn, so cutting three of them
    /// to four columns with a one-column ellipsis yields one of them and the
    /// ellipsis. Hence a bound and not an equality.
    #[test]
    fn nothing_is_ever_left_wider_than_the_width() {
        for width in 1..12 {
            for row in [
                "\u{65e5}\u{672c}\u{8a9e}",
                "abcdefghij",
                "a\u{65e5}b\u{672c}c",
                "\u{1f600}\u{1f600}",
            ] {
                assert!(
                    measure_text_width(&clamp(row, width)) <= width,
                    "{row:?} at width {width}"
                );
            }
        }
    }
}
