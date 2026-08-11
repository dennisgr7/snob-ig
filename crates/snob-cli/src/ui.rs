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
    if !std::io::stdin().is_terminal() {
        let mut line = Zeroizing::new(String::with_capacity(SECRET_CAPACITY));
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("could not read input")?;
        return Ok(line);
    }

    let term = console::Term::stderr();
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
            // Arrows, function keys and the like: ignored.
            _ => continue,
        }

        term.clear_line()?;
        term.write_str(&format!("{prompt}{}", mask(&buffer)))?;
    }

    term.write_line("")?;
    Ok(buffer)
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
/// cursor movement on **standard error**, which is where `dialoguer` puts every
/// prompt — `Select::interact_opt` builds a `Term::stderr()`. Standard output
/// is the one stream a menu never touches.
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
    use dialoguer::theme::ColorfulTheme;

    dialoguer::Select::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(labels)
        .default(0)
        .interact_opt()
        .context("could not show the menu")
}

/// Undoes what a prompt did to the terminal, for an exit that runs no
/// destructors.
///
/// `dialoguer` hides the cursor while a menu is up and shows it again on the
/// way out. The release profile is `panic = "abort"` and the forced-quit path
/// calls `exit(130)`, so neither of those runs its way out — and an invisible
/// cursor is not scoped to this program. It stays that way for the rest of the
/// shell session, long after the user has forgotten what they pressed.
///
/// Idempotent and safe with no terminal: `console` writes the sequence to
/// stderr and does nothing if that is not a terminal.
pub fn restore_terminal() {
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
    use super::{can_be_asked, can_show_a_menu, looks_like_console_dump, mask};

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
