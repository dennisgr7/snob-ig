//! Terminal input and output.
//!
//! Everything here has to degrade gracefully when there is no interactive
//! terminal: the most common failure is not an old emulator, it is a pipe into
//! another process.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};

pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

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
pub fn prompt_secret(prompt: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("could not read input")?;
        return Ok(line);
    }

    let term = console::Term::stderr();
    let mut buffer = String::new();

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

pub fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
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

/// A yes or no question. With no terminal it keeps the default.
pub fn confirm(question: &str, default: bool) -> Result<bool> {
    if !is_interactive() {
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
    use super::{looks_like_console_dump, mask};

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
