//! Keys for the interactive views, read through `crossterm` rather than
//! `console::Term::read_key`.
//!
//! Reading is its own module because it is its own problem: `ratatui` only
//! writes, and `console` -- which still asks the capability questions and
//! prints everything outside the views -- reads keys badly. Two of the
//! reasons are defects rather than improvements, and they are why this is
//! the change that mattered most.
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
//! - **Raw mode that lasts.** `read_single_key` calls `tcsetattr` twice
//!   around every keypress, so between two keys the terminal is back in
//!   canonical mode with `ECHO` on: anything typed while a browser is off
//!   fetching megabytes from the CDN would echo into the frame. The raw mode
//!   these views hold for the whole session lives on [`crate::ui::tui::Tui`],
//!   with the rest of the terminal claim.
//!
//! What is **not** taken: mouse tracking, focus events and the kitty keyboard
//! protocol. Mouse capture breaks the terminal's own text selection, which is
//! the loudest complaint against every full-screen tool that turns it on, and
//! four keys do not need it. The kitty protocol exists to disambiguate
//! Shift+Enter and Ctrl+Enter, which nothing here binds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use snob_ig::pace::CancelToken;

/// How long the browser waits before waking up to look at the terminal size.
///
/// An unchanged frame writes nothing, so the cost of a wake-up is one
/// `GetConsoleScreenBufferInfo` or `TIOCGWINSZ` and a cell-buffer diff that
/// finds nothing to send. Short enough that a resize looks immediate, long
/// enough that an idle browser is not a spin.
///
/// The raw mode these reads depend on is the guard's — `ui::tui::Tui` turns
/// it on for the whole session, which is also what makes Ctrl+C answerable:
/// under crossterm's raw mode it is a `KeyEvent` carrying `CONTROL`, on both
/// platforms, which is something a program can decide about.
pub const TICK: Duration = Duration::from_millis(150);

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
    /// One level up, in a browser that has levels. The story list has none
    /// and ignores it; the highlights browser goes from the items back to
    /// the tray. Not `Quit`: leaving a folder and leaving the program are
    /// different intentions, and Esc keeps meaning the second everywhere.
    Back,
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

/// What came back from the terminal, before any binding is applied.
///
/// For the one browser that has a text mode: while the account list is being
/// filtered, a `q` is a letter of somebody's name and not a way out, so the
/// binding cannot be decided here. Everything else keeps reading through
/// [`next`], which applies [`action_of`] and swallows pastes — a browser with
/// no text mode has nowhere safe to put one.
pub enum Raw {
    /// A key going down. Releases are already filtered out — on Windows the
    /// console reports both, and without the filter every keystroke would
    /// count twice.
    Key(KeyEvent),
    /// A paste, whole, thanks to bracketed paste. The caller decides whether
    /// it is text to keep or noise to drop.
    Paste(String),
    Resized,
    Tick,
}

/// Waits up to `timeout` for something to happen, and hands it over unbound.
pub fn read(timeout: Duration) -> std::io::Result<Raw> {
    if !event::poll(timeout)? {
        return Ok(Raw::Tick);
    }
    Ok(match event::read()? {
        Event::Resize(..) => Raw::Resized,
        Event::Paste(text) => Raw::Paste(text),
        // `KeyEventKind` matters on Windows, where the console reports the
        // release of a key as well as the press. Without this filter every
        // keystroke moves the selection twice.
        Event::Key(key) if key.kind == KeyEventKind::Press => Raw::Key(key),
        _ => Raw::Tick,
    })
}

/// Waits up to `timeout` for something to happen.
pub fn next(timeout: Duration) -> std::io::Result<Next> {
    Ok(match read(timeout)? {
        Raw::Tick => Next::Tick,
        Raw::Resized => Next::Resized,
        // Swallowed whole, and that is the entire fix. Without mode 2004 the
        // same paste arrives as its characters and some of them are commands.
        Raw::Paste(_) => Next::Do(Action::None),
        Raw::Key(key) => Next::Do(action_of(key)),
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
        // Right alongside Enter and Left alongside Backspace: in a browser
        // with levels the arrows walk into and out of a folder, the way every
        // two-pane file view binds them. In the flat story list Right opens,
        // which is what Enter does, and Left does nothing.
        KeyCode::Right if plain => Action::Open,
        KeyCode::Left | KeyCode::Backspace if plain => Action::Back,
        KeyCode::Char('d') | KeyCode::Char('D') if plain => Action::Download,
        KeyCode::Esc | KeyCode::Char('q') if plain => Action::Quit,
        _ => Action::None,
    }
}

/// Whether the keyboard ended a fetch early, and what the person meant by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The future ran to its own end.
    No,
    /// `q`: stop what is in flight and leave the browser cleanly.
    ///
    /// Esc is deliberately not a stop. In a browser with levels it means "up
    /// one level", and a canceled pacer token stays canceled — a browser
    /// that stayed open after one would refuse every fetch it has left.
    /// Leaving is the only thing a cancellation can be followed by, and `q`
    /// and Ctrl+C are the two keys that already mean leaving everywhere.
    Quit,
    /// Ctrl+C: stop and leave saying so.
    Interrupt,
}

impl Stopped {
    /// The way out of the browser loop, when this asks for one.
    pub fn leave(self) -> Option<crate::exit::ExitCode> {
        match self {
            Stopped::No => None,
            Stopped::Quit => Some(crate::exit::ExitCode::Ok),
            Stopped::Interrupt => Some(crate::exit::ExitCode::Interrupted),
        }
    }
}

/// How often the fetch watcher wakes to look for a stop key. Shorter than
/// [`TICK`]: it is also the longest a finished fetch waits for the watcher
/// to notice it can stand down.
const WATCH: Duration = Duration::from_millis(50);

/// Runs a fetch with the keyboard still alive.
///
/// Raw mode delivers Ctrl+C as a key event, never as a signal — so during an
/// `.await` inside a browser loop *neither* cancellation route used to exist:
/// the `interrupt::install` task never fires, and the key loop is not
/// running. A story that stalls against its 60-second timeout held the whole
/// terminal hostage. This watches the keyboard on a blocking thread while
/// the future runs; Ctrl+C and `q` cancel `cancel` — the token every
/// request and CDN download already races — so the future resolves promptly
/// with a canceled error and the caller leaves through [`Stopped::leave`].
///
/// Keys that are not a stop are deliberately consumed and dropped: type-ahead
/// against a busy browser replays against whatever frame comes next, and a
/// buffered `d` is a download nobody watched themselves ask for.
pub async fn watching_cancel_keys<T>(
    cancel: &CancelToken,
    fut: impl std::future::Future<Output = T>,
) -> (T, Stopped) {
    let done = Arc::new(AtomicBool::new(false));
    let watcher = {
        let done = done.clone();
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            while !done.load(Ordering::Relaxed) {
                match read(WATCH) {
                    // Bound by key rather than through `action_of`: Esc also
                    // maps to `Action::Quit` there, and Esc is not a stop —
                    // see `Stopped::Quit`.
                    Ok(Raw::Key(key)) => {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        if key.code == KeyCode::Char('c') && ctrl {
                            cancel.cancel();
                            return Stopped::Interrupt;
                        }
                        if key.code == KeyCode::Char('q') && key.modifiers.is_empty() {
                            cancel.cancel();
                            return Stopped::Quit;
                        }
                    }
                    Ok(_) => {}
                    // A keyboard that cannot be read is the loop's own error
                    // to hit; the watcher just stands down.
                    Err(_) => return Stopped::No,
                }
            }
            Stopped::No
        })
    };

    let out = fut.await;
    done.store(true, Ordering::Relaxed);
    let stopped = watcher.await.unwrap_or(Stopped::No);
    (out, stopped)
}

/// How far Page Up and Page Down move, given how many rows the list has.
///
/// What is on screen less one row, so that the row the selection was on stays
/// visible after the jump and there is something to read the new position
/// against. Not input, but it belongs beside the two keys that ask for it.
pub fn page(height: usize) -> usize {
    height.saturating_sub(1).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// The defect this module exists for. Under `console`, `ESC[1;2D` left `2D`
    /// in the queue and the `D` reached the browser as a download. Shift+Left
    /// is plain-with-shift so it lands on `Back`, which is harmless; Ctrl+Left
    /// is somebody's terminal binding and stays untouched.
    #[test]
    fn a_shifted_arrow_is_not_a_download() {
        assert_eq!(
            action_of(key(KeyCode::Left, KeyModifiers::SHIFT)),
            Action::Back
        );
        assert_eq!(
            action_of(key(KeyCode::Left, KeyModifiers::CONTROL)),
            Action::None
        );
    }

    /// The keys a browser with levels adds: out of a folder, and into one.
    #[test]
    fn left_goes_back_and_right_opens() {
        assert_eq!(
            action_of(key(KeyCode::Left, KeyModifiers::NONE)),
            Action::Back
        );
        assert_eq!(
            action_of(key(KeyCode::Backspace, KeyModifiers::NONE)),
            Action::Back
        );
        assert_eq!(
            action_of(key(KeyCode::Right, KeyModifiers::NONE)),
            Action::Open
        );
        // Esc stays "leave the program", not "leave the folder".
        assert_eq!(
            action_of(key(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Quit
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
        assert_eq!(page(20), 19);
        // And never zero, or Page Down would not move.
        assert_eq!(page(1), 1);
    }
}
