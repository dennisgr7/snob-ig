//! Terminal input and output.
//!
//! Everything here has to degrade gracefully when there is no interactive
//! terminal: the most common failure is not an old emulator, it is a pipe into
//! another process.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use zeroize::Zeroizing;

/// Room reserved for a typed secret, so the buffer never has to grow. A
/// `sessionid` runs to about seventy characters; this is well past anything
/// anyone will paste.
const SECRET_CAPACITY: usize = 256;

/// How many characters at the end are left visible.
const VISIBLE_TAIL_CHARS: usize = 4;
/// Cap on the asterisks drawn, so the line never wraps and leaves behind
/// remnants that `clear_line` cannot reach.
const MAX_ASTERISKS: usize = 40;

/// Asks for a secret value, masking what is typed.
///
/// Hiding it entirely leaves the user unsure whether the paste worked, which
/// with a seventy-character `sessionid` is a reasonable doubt. One asterisk per
/// character is drawn, the last four are left visible, and the total is shown,
/// which is what lets you confirm at a glance that the whole thing went in.
///
/// With no terminal a plain line is read, so scripting keeps working.
///
/// The buffer is reserved up front and wrapped so it clears itself. Both
/// halves are needed and the first is the one that is easy to miss: a `String`
/// that grows one keystroke at a time leaves every outgrown buffer behind
/// untouched, so a seventy-character `sessionid` typed into an empty `String`
/// scatters half a dozen plaintext prefixes of the cookie through freed memory
/// that nothing will ever clear. `Zeroizing` can only wipe the buffer it still
/// owns.
pub fn prompt_secret(prompt: &str) -> Result<Zeroizing<String>> {
    // Asked of the very terminal the keys are read from, so nothing sits
    // between the question and the answer.
    let term = console::Term::stderr();

    if !can_mask(
        std::io::stdin().is_terminal(),
        term.features().is_attended(),
    ) {
        let mut line = Zeroizing::new(String::with_capacity(SECRET_CAPACITY));
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("could not read input")?;
        return Ok(line);
    }

    let mut buffer = Zeroizing::new(String::with_capacity(SECRET_CAPACITY));

    term.write_str(prompt)
        .context("could not write to the terminal")?;

    loop {
        match term.read_key().context("could not read input")? {
            console::Key::Char(c) => buffer.push(c),
            console::Key::Backspace => {
                buffer.pop();
            }
            console::Key::Enter => break,
            console::Key::Escape | console::Key::CtrlC => {
                term.write_line("")?;
                bail!("input canceled");
            }
            // Arrows, function keys and the like: ignored, `Key::Unknown`
            // included. On Windows `console` reports every virtual key it has
            // no name for that way — Caps Lock, the function keys, Page Up —
            // so treating it as a failure would abort a paste because somebody
            // brushed a key. The spin it used to cause is closed by the gate
            // above rather than here, where it can be closed exactly.
            _ => continue,
        }

        term.clear_line()?;
        term.write_str(&format!("{prompt}{}", mask(&buffer)))?;
    }

    term.write_line("")?;
    Ok(buffer)
}

/// Whether the masked prompt can run, from the two facts it depends on.
///
/// **Both streams, not one.** It reads keys from the terminal and draws to
/// standard error, and it was gated on standard input alone. `console` answers
/// `read_key` with `Key::Unknown` *immediately* when the stream its `Term` was
/// built on is not a tty — not an error, not a block — and the loop's arm for
/// an unrecognized key is `continue`. So `snob login --paste 2> log`, which is
/// what capturing a session looks like, pegged a core forever with nothing on
/// screen and never read what was pasted.
///
/// The same stream asymmetry [`can_show_a_menu`] was written for, reaching the
/// one prompt that never got the gate. Standard input still has to be a
/// terminal too: without it the bytes are arriving from a pipe and there is
/// nobody to draw asterisks for.
fn can_mask(stdin_is_terminal: bool, stderr_is_attended: bool) -> bool {
    stdin_is_terminal && stderr_is_attended
}

fn mask(value: &str) -> String {
    let total = value.chars().count();
    if total == 0 {
        return String::new();
    }

    // With a very short value nothing is shown: revealing "the last four" of
    // something four characters long would reveal all of it.
    if total <= VISIBLE_TAIL_CHARS {
        return format!("{} ({total})", "*".repeat(total));
    }

    let hidden = (total - VISIBLE_TAIL_CHARS).min(MAX_ASTERISKS);
    let tail: String = value.chars().skip(total - VISIBLE_TAIL_CHARS).collect();
    format!("{}{tail} ({total})", "*".repeat(hidden))
}

/// Whether there is somebody at the keyboard to answer a question.
///
/// Standard input alone, because that is the only stream a question needs: it
/// is asked on standard error by [`prompt_line`] and answered on standard
/// input, so redirecting the *results* has nothing to do with it.
/// [`prompt_secret`] draws the line the same way, and the two questions
/// `login --paste` asks must not disagree about whether anyone is there.
///
/// This used to sit beside a predicate that asked about standard input **and**
/// standard output, and that one governed every prompt in the program. It is
/// gone: no question here is asked on standard output, so there was no correct
/// use of it left.
pub fn can_be_asked() -> bool {
    std::io::stdin().is_terminal()
}

/// Whether an arrow-key menu can be shown.
///
/// Stricter than [`can_be_asked`], and about a different pair of streams than
/// the stdin-and-stdout one this replaced. A menu is not a line of text: it
/// needs keys, which arrive on standard input, and it redraws itself with
/// cursor movement on **standard error**, which is where `ui::menu` draws —
/// it writes through a `Term::stderr()`, as the story browser does. Standard
/// output is the one stream a menu never touches.
///
/// Asking about the wrong two got it wrong in both directions: `snob login |
/// tee log` refused to show a menu it could have drawn perfectly well, and
/// `snob login 2>log` drew one into the file and then waited for arrow keys
/// with nothing on screen — the same failure [`prompt_line`] documents, from
/// the other side.
///
/// Standard input is required even though `console` would fall back to the
/// controlling terminal when it is redirected. Whether somebody is there is one
/// question with one answer: [`prompt_secret`] reads piped bytes when standard
/// input is not a terminal, and a menu that still worked in the same run would
/// be asking a person something while the credential arrived from a file.
pub fn can_show_a_menu() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Whether a media browser may open *instead of* the printed listing.
///
/// Stricter than [`can_show_a_menu`] by exactly one stream, and the third
/// stream is the point. A browser needs what a menu needs — keys from
/// standard input, drawing on standard error — but opening one also means
/// **not printing the listing**, and the listing's stream is standard output.
/// With stdout redirected, `snob stories someone > list.txt` is somebody
/// collecting the listing while watching the terminal: both other streams are
/// attended, a menu could be drawn, and drawing one would fill the file with
/// nothing while a browser waited on keys. The listing wins wherever it was
/// asked for; the browser replaces it only where it would have scrolled by.
///
/// This is the predicate behind defaulting to the browser at all — the
/// explicit flags (`-i`, `--no-interactive`, and every action flag) are
/// decided before it is consulted, in `MediaActionArgs::browses`.
pub fn a_human_would_watch_the_listing_scroll_by() -> bool {
    can_show_a_menu() && std::io::stdout().is_terminal()
}

/// Asks a question and reads one line back.
///
/// The prompt goes to standard error because it is not the result. On stdout it
/// was swallowed by a redirect: `snob login --paste > out.txt` on a machine with
/// no Chromium browser installed reaches `prompt_user_agent`, which wrote
/// "User-Agent: " into the file and then blocked on stdin with nothing on
/// screen. That reads as a hang.
pub fn prompt_line(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("could not read input")?;
    if line.is_empty() {
        bail!("nothing arrived on standard input");
    }
    Ok(line.trim().to_string())
}

/// A yes or no question, asked without holding an async worker while the
/// person thinks about it.
///
/// Waiting on a human is the longest block in the program — unbounded, by
/// definition — and the runtime has exactly two workers. Doing it on one of
/// them leaves one for everything else, including the task that owns Ctrl+C
/// now that `tokio::signal` has taken it away from the operating system.
///
/// Only for the prompts that are asked while a run is under way. `purge` asks
/// before there is anything else to schedule, so it calls [`confirm`] directly
/// and this indirection would buy it nothing.
///
/// That is also why it takes the progress bar: being asked while a run is under
/// way means being asked while something is drawing. The bar writes to standard
/// error and so does the prompt, so the question landed on the line the
/// animation owns and the next tick wiped it — leaving somebody watching a
/// spinner and not knowing it was waiting for them.
pub async fn confirm_off_thread(
    progress: &crate::progress::Progress,
    question: String,
    default: bool,
) -> Result<bool> {
    let progress = progress.clone();
    tokio::task::spawn_blocking(move || progress.while_paused(|| confirm(&question, default)))
        .await
        .context("the question could not be asked")?
}

/// A yes or no question. With nobody at the keyboard it keeps the default.
///
/// The gate is [`can_be_asked`], not both standard streams. The question goes
/// to standard error through [`prompt_line`] and the answer comes off standard
/// input, so `snob unfollowers someone | jq` can be asked and answered like any
/// other run. It used to ask about standard output as well — the one stream
/// with nothing to do with it — which turned every redirect and every pipe into
/// a machine with nobody at it.
pub fn confirm(question: &str, default: bool) -> Result<bool> {
    if !can_be_asked() {
        return Ok(default);
    }
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    let answer = prompt_line(&format!("{question} {suffix} "))?;
    Ok(match answer.trim().to_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

/// Login method chosen by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMethod {
    Paste,
    Browser,
}

/// Arrow-key selection menu. Returns `None` if canceled with Esc.
pub fn choose_login_method() -> Result<Option<LoginMethod>> {
    let chosen = choose(
        "How do you want to log in?",
        &[
            "Open a browser and log in there  (recommended)",
            "Paste the sessionid from the developer tools",
        ],
    )?;
    Ok(chosen.map(|i| {
        if i == 0 {
            LoginMethod::Browser
        } else {
            LoginMethod::Paste
        }
    }))
}

/// Picks one of several options with the arrow keys. `None` means Esc.
pub fn choose(prompt: &str, labels: &[&str]) -> Result<Option<usize>> {
    menu::choose(prompt, labels)
}

/// Undoes what a menu or the browser did to the terminal, for an exit that
/// runs no destructors.
///
/// Both hide the cursor while they are up and put the terminal in raw mode to
/// read keys, and both restore the two on the way out through guards. The
/// release profile is `panic = "abort"` and the forced-quit path calls
/// `exit(130)`, so neither guard runs on those ways out — and neither an
/// invisible cursor nor a raw terminal is scoped to this program. They stay
/// that way for the rest of the shell session, long after the user has
/// forgotten what they pressed.
///
/// Idempotent and safe with no terminal: `console` writes the cursor sequence
/// to stderr and does nothing if that is not a terminal, and leaving raw mode
/// a terminal was never in is a no-op.
pub fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = console::Term::stderr().show_cursor();
}

/// What every command says when there is no session, said once.
///
/// Three commands printed this line, byte for byte, hand-written, with a
/// doc-comment in one of them claiming the copy was shared. It was not, and
/// nothing pinned any of the three, so they could have drifted into three ways
/// of describing one situation without a test noticing.
pub fn no_session() {
    eprintln!("No session stored. Run \"snob login\".");
}

pub fn warn(message: &str) {
    eprintln!("warning: {message}");
}

/// A line of the result, on standard output -- `println!` that survives the
/// reader leaving.
///
/// `println!` panics on a closed pipe, and Rust ignores `SIGPIPE`, so
/// `snob watch status | head -1` and `snob whoami --json | jq -r .username`
/// ended with a panic message and an undocumented exit status the moment the
/// far end had read enough. `output::write_rendered` has tolerated a broken
/// pipe for the lists since the start; the fifty-odd lines of prose that go
/// to standard output did not. A closed pipe is the reader saying it has
/// seen what it wanted, which is not an error of this program's.
pub fn say_line(line: std::fmt::Arguments<'_>) {
    let stdout = std::io::stdout();
    let mut locked = stdout.lock();
    for step in [locked.write_fmt(line), locked.write_all(b"\n")] {
        match step {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return,
            // Anything else on standard output is the terminal going away,
            // and there is nobody left to tell.
            Err(_) => return,
            Ok(()) => {}
        }
    }
}

/// `println!`, through [`say_line`]. Same arguments, same shape, one difference.
macro_rules! say {
    () => {
        $crate::ui::say_line(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::ui::say_line(format_args!($($arg)*))
    };
}
pub(crate) use say;

pub fn info(message: &str) {
    eprintln!("{message}");
}

/// Instructions for the paste method.
///
/// The `sessionid` is marked `HttpOnly`, so it cannot be pulled from the
/// console with `document.cookie`: the storage panel is the only place.
pub fn paste_instructions() {
    eprintln!(
        "\n\
         To get your session without snob ever touching your password:\n\
        \n\
         1. Open https://www.instagram.com in your browser and make sure you\n\
            are logged in.\n\
         2. Open the developer tools (F12) and go to the \"Application\" tab\n\
            (\"Storage\" in Firefox). Your browser may show these tab names\n\
            translated.\n\
         3. Under \"Cookies\" -> \"https://www.instagram.com\", find\n\
            \"sessionid\" and copy ONLY its value, by double-clicking it.\n\
        \n\
         There is no need to touch the \"Console\" tab.\n"
    );
}

/// Detects whether what was pasted looks like a dump of the browser console
/// rather than the value being asked for.
///
/// This really happened: copying from the developer tools console drags in
/// dozens of log lines, and pasting them into a terminal runs each one as a
/// command. Spotting it lets us explain what happened instead of leaving the
/// user thinking their machine was broken into.
///
/// **The needles are deliberately language-independent.** Chrome and Instagram
/// print that text in the user's own language, so matching on the words would
/// narrow this check to English browsers instead of widening it. The Spanish
/// ones that used to be here were deleted rather than translated for exactly
/// that reason.
///
/// This is not the barrier, either. The real one is the User-Agent validator,
/// which requires the `Mozilla/5.0` prefix and does not care about language.
/// This only exists to give a better error message.
pub fn looks_like_console_dump(text: &str) -> bool {
    const LANGUAGE_INDEPENDENT_NEEDLES: [&str; 4] =
        ["selfxss", ".js:", "net::ERR_", "chrome-extension://"];
    LANGUAGE_INDEPENDENT_NEEDLES
        .iter()
        .any(|n| text.contains(n))
}

#[cfg(test)]
mod tests {
    use super::{can_be_asked, can_mask, can_show_a_menu, looks_like_console_dump, mask};

    /// The masked prompt needs both streams, because it uses both.
    ///
    /// Gated on standard input alone, `snob login --paste 2> log` -- which is
    /// what capturing a session looks like -- reached the loop with a `Term`
    /// that had no terminal to read from, and the loop's arm for an
    /// unrecognized key is `continue`.
    #[test]
    fn masking_needs_the_stream_it_draws_on_as_well() {
        assert!(can_mask(true, true));
        assert!(
            !can_mask(true, false),
            "stderr redirected: the case that spun"
        );
        assert!(!can_mask(false, true), "the value is arriving from a pipe");
        assert!(!can_mask(false, false));
    }

    /// The library behavior the gate exists for, asserted rather than assumed.
    ///
    /// `Term::read_key` on a stream that is not a tty answers `Key::Unknown` at
    /// once: no error, no block, and forever. That is what turns `continue`
    /// into a spin, and it is a fact about `console` rather than about this
    /// code -- so it is checked here, where a version bump that changes it
    /// fails a test instead of quietly making the gate pointless.
    #[test]
    fn reading_a_key_from_a_stream_that_is_not_a_terminal_answers_at_once() {
        let term = console::Term::stderr();
        if term.features().is_attended() {
            // Run from a terminal, where the call would block. Nothing to say.
            return;
        }

        assert!(
            matches!(term.read_key(), Ok(console::Key::Unknown)),
            "the arm the loop `continue`s on, answered instantly"
        );
        assert!(!can_mask(true, term.features().is_attended()));
    }

    /// A menu is strictly more demanding than a line, and this must never
    /// invert.
    ///
    /// Both need somebody at the keyboard; only the menu also needs a terminal
    /// to draw its cursor movement on. If a run could ever show a menu it could
    /// not ask a plain question, `login` would put arrow keys in front of
    /// somebody it had already decided was not there.
    ///
    /// Deliberately a relation rather than a value: what either answers depends
    /// on where the suite was started from, and a test that asserted `true`
    /// would pass under `cargo test` and fail in a pipeline, or the reverse.
    #[test]
    fn a_menu_asks_about_the_stream_it_draws_on() {
        assert!(!can_show_a_menu() || can_be_asked());
    }

    #[test]
    fn nothing_is_drawn_when_nothing_is_typed() {
        assert_eq!(mask(""), "");
    }

    #[test]
    fn a_short_value_is_not_revealed() {
        // Showing "the last four" of a four-character value would show all of it.
        assert_eq!(mask("abcd"), "**** (4)");
        assert_eq!(mask("ab"), "** (2)");
    }

    #[test]
    fn the_last_four_and_the_total_are_visible() {
        assert_eq!(mask("1234567890"), "******7890 (10)");
    }

    #[test]
    fn a_long_value_does_not_overflow_the_line() {
        let sessionid = "7".repeat(200);
        let drawn = mask(&sessionid);
        assert!(
            drawn.chars().count() < 60,
            "the line is {} characters: {drawn}",
            drawn.chars().count()
        );
        assert!(drawn.ends_with("(200)"));
    }

    #[test]
    fn the_total_shows_whether_the_paste_arrived_whole() {
        let real = "71234567890%3AAbCdEfGhIjKl%3A20%3AAYc123";
        assert!(mask(real).ends_with(&format!("({})", real.len())));
    }

    #[test]
    fn a_console_dump_is_recognized_by_its_stack_frames() {
        assert!(looks_like_console_dump("send @ AHcO4pgUb.js:634"));
        assert!(looks_like_console_dump(
            "Failed to load resource: net::ERR_BLOCKED_BY_CLIENT"
        ));
        assert!(looks_like_console_dump(
            "see https://www.facebook.com/selfxss"
        ));
    }

    /// The needles must not depend on the browser's language, so a real
    /// User-Agent has to pass whatever locale the user runs.
    #[test]
    fn a_real_user_agent_is_not_flagged() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";
        assert!(!looks_like_console_dump(ua));
    }
}

/// Drawing and key-reading for the interactive story list. See its own header
/// for what it costs against the terminal-UI framework it is not.
pub mod browser;
pub mod menu;

/// The interactive story list.
pub mod highlights;
pub mod stories;
