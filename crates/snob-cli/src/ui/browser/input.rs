//! Keys for the story browser, read through `crossterm` rather than
//! `console::Term::read_key`.
//!
//! This is the one place in the program that does not read keys with `console`,
//! and the split is deliberate: `console` still draws everything, because it
//! measures display columns and knows what a terminal supports, and it is only
//! the *reading* that moved. Two of the reasons are defects rather than
//! improvements, and they are why this is the change that mattered most.
//!
//! **Shift+Left used to download a story.** `console`'s escape parser
//! understands `ESC [ <letter>` and `ESC [ <digit> ~`, and nothing else -- it
//! has no notion of parameters. Every modified arrow is `ESC [ 1 ; <mod>
//! <letter>`, so Shift+Left arrives as `ESC[1;2D` and is read as: an
//! unrecognized sequence (ignored), then `2` (ignored), then **`D`**, which was
//! the download key. Three passes of the loop, all instant. Ctrl+Left is the
//! same. Here a modified arrow is a `KeyEvent` carrying `KeyModifiers`, and
//! [`action_of`] refuses anything that is not plain or plain-with-shift.
//!
//! **Pasting text used to run it as commands.** Nothing turned on DEC private
//! mode 2004, so a terminal sends pasted text as its characters, and a paste
//! containing a `d` downloaded and one containing a `q` quit. There is no fix
//! for this without the mode: with no wrapper there is nothing to tell a paste
//! from somebody typing quickly. `EnableBracketedPaste` plus crossterm's parser
//! turns the whole paste into one `Event::Paste` to drop on the floor, and that
//! is the single strongest argument for reading keys here.
//!
//! Three more things follow, each of which `console` cannot do at all:
//!
//! - **A wait that ends by itself.** `read_key` blocks with no timeout and
//!   there is no `poll` beside it. [`next`] takes a timeout, which is what lets
//!   the browser wake up, re-read the terminal size and redraw.
//! - **Resize.** On Unix it is `SIGWINCH`, folded into the same `mio` poll
//!   tokio already runs. On Windows there is no such signal; it arrives as a
//!   `WINDOW_BUFFER_SIZE_EVENT`, which `console` does not merely fail to report
//!   -- its `read_key_event` loop skips every record that is not a `KEY_EVENT`,
//!   so it consumes and discards it.
//! - **Raw mode that lasts.** See [`Session`].
//!
//! What is **not** taken: mouse tracking, focus events and the kitty keyboard
//! protocol. Mouse capture breaks the terminal's own text selection, which is
//! the loudest complaint against every full-screen tool that turns it on, and
//! four keys do not need it. The kitty protocol exists to disambiguate
//! Shift+Enter and Ctrl+Enter, which nothing here binds.

use std::io::stderr;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;

use super::viewport::Viewport;

/// How long the browser waits before waking up to look at the terminal size.
///
/// An unchanged frame writes nothing, so the cost of a wake-up is one
/// `GetConsoleScreenBufferInfo` or `TIOCGWINSZ` and a comparison of two
/// vectors of strings. Short enough that a resize looks immediate, long enough
/// that an idle browser is not a spin.
pub const TICK: Duration = Duration::from_millis(150);

/// Raw mode and bracketed paste, held for as long as the browser is up.
///
/// **Raw mode for the session, not for one read**, and that is the difference
/// from `console`. `read_single_key` calls `tcsetattr` twice around every
/// keypress, so between two keys the terminal is back in canonical mode with
/// `ECHO` on: anything typed while the browser is off fetching a story from the
/// CDN is echoed into the middle of the frame and then delivered as a line when
/// the next read starts. A story can be several megabytes, so that window is
/// not theoretical.
///
/// It is also what makes Ctrl+C answerable. `Term::read_key` calls
/// `read_single_key(false)`; `cfmakeraw` has already cleared `ISIG`, so on Unix
/// Ctrl+C arrives as `Key::Char('\x03')` and never as `Key::CtrlC`, and on
/// Windows `ENABLE_PROCESSED_INPUT` stays on so it never reaches the read at
/// all. Under crossterm's raw mode it is a `KeyEvent` carrying `CONTROL`, on
/// both platforms, which is something a program can decide about.
///
/// A guard rather than two calls, so that leaving through an error puts the
/// terminal back. **It does not run on a panic** -- the release profile is
/// `panic = "abort"`, so no destructor does -- and what covers that case is not
/// this program: every interactive shell's line editor sets the terminal modes
/// it wants before it prompts.
pub struct Session;

impl Session {
    pub fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        // Not fatal. A terminal that will not turn bracketed paste on is a
        // terminal where paste behaves the way it did before, which is a state
        // the browser is already prepared for -- so this is worth having and
        // not worth refusing to start over.
        execute!(stderr(), EnableBracketedPaste).ok();
        Ok(Self)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        execute!(stderr(), DisableBracketedPaste).ok();
        crossterm::terminal::disable_raw_mode().ok();
    }
}

/// What the browser understands, resolved from a key.
///
/// Separated from the key so that the binding can be tested without a terminal,
/// which is the only way to hold on to the two defects above: there are tests
/// below asserting that Shift+Left and Ctrl+D are not downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
    Open,
    Download,
    Redraw,
    Quit,
    Interrupt,
    None,
}

/// What came back from the terminal.
pub enum Next {
    Do(Action),
    /// The terminal changed size. No numbers travel with it: the browser
    /// re-reads the size when it paints, so all this says is that a paint is
    /// due.
    Resized,
    /// The wait ended with nothing in it.
    Tick,
}

/// Waits up to `timeout` for something to happen.
pub fn next(timeout: Duration) -> std::io::Result<Next> {
    if !event::poll(timeout)? {
        return Ok(Next::Tick);
    }
    Ok(match event::read()? {
        Event::Resize(..) => Next::Resized,
        // Swallowed whole, and that is the entire fix. Without mode 2004 the
        // same paste arrives as its characters and some of them are commands.
        Event::Paste(_) => Next::Do(Action::None),
        // `KeyEventKind` matters on Windows, where the console reports the
        // release of a key as well as the press. Without this filter every
        // keystroke moves the selection twice.
        Event::Key(key) if key.kind == KeyEventKind::Press => Next::Do(action_of(key)),
        _ => Next::Do(Action::None),
    })
}

/// Binds one key.
///
/// Plain, or plain with shift, and nothing else. A modified arrow is somebody
/// reaching for a binding of their terminal, their multiplexer or their window
/// manager, and answering it here is how `ESC[1;2D` used to become a download.
pub fn action_of(key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let plain = !ctrl && !key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('c') if ctrl => Action::Interrupt,
        // The universal "what is on screen is wrong, draw it again". Worth
        // binding in any program that keeps a belief about the screen, and
        // worth it twice on Windows, where ConPTY coalesces positioned writes.
        KeyCode::Char('l') if ctrl => Action::Redraw,
        KeyCode::Up | KeyCode::Char('k') if plain => Action::Up,
        KeyCode::Down | KeyCode::Char('j') if plain => Action::Down,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Home | KeyCode::Char('g') if plain => Action::First,
        KeyCode::End | KeyCode::Char('G') if plain => Action::Last,
        KeyCode::Enter => Action::Open,
        KeyCode::Char('d') | KeyCode::Char('D') if plain => Action::Download,
        KeyCode::Esc | KeyCode::Char('q') if plain => Action::Quit,
        _ => Action::None,
    }
}

/// How far Page Up and Page Down move.
///
/// What is on screen less one row, so that the row the selection was on stays
/// visible after the jump and there is something to read the new position
/// against. Not input, but it belongs beside the two keys that ask for it.
pub fn page(vp: &Viewport) -> usize {
    vp.height.saturating_sub(1).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// The defect this module exists for. Under `console`, `ESC[1;2D` left `2D`
    /// in the queue and the `D` reached the browser as a download.
    #[test]
    fn a_shifted_arrow_is_not_a_download() {
        assert_eq!(
            action_of(key(KeyCode::Left, KeyModifiers::SHIFT)),
            Action::None
        );
        assert_eq!(
            action_of(key(KeyCode::Left, KeyModifiers::CONTROL)),
            Action::None
        );
    }

    #[test]
    fn a_modified_letter_is_not_the_letter() {
        assert_eq!(
            action_of(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Action::None
        );
        assert_eq!(
            action_of(key(KeyCode::Char('q'), KeyModifiers::ALT)),
            Action::None
        );
    }

    #[test]
    fn ctrl_c_interrupts_and_ctrl_l_redraws() {
        assert_eq!(
            action_of(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Interrupt
        );
        assert_eq!(
            action_of(key(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Action::Redraw
        );
    }

    #[test]
    fn the_plain_keys_still_work() {
        assert_eq!(
            action_of(key(KeyCode::Down, KeyModifiers::NONE)),
            Action::Down
        );
        assert_eq!(
            action_of(key(KeyCode::Char('j'), KeyModifiers::NONE)),
            Action::Down
        );
        assert_eq!(
            action_of(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Open
        );
        assert_eq!(
            action_of(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Action::Quit
        );
    }

    /// Shift is what makes a capital letter, so it cannot be the modifier that
    /// disqualifies one -- `D` has to keep working.
    #[test]
    fn a_capital_letter_is_still_its_letter() {
        assert_eq!(
            action_of(key(KeyCode::Char('D'), KeyModifiers::SHIFT)),
            Action::Download
        );
        assert_eq!(
            action_of(key(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Action::Last
        );
    }

    #[test]
    fn a_page_leaves_one_row_of_overlap() {
        assert_eq!(page(&Viewport::new(20)), 19);
        // And never zero, or Page Down would not move.
        assert_eq!(page(&Viewport::new(1)), 1);
    }
}
