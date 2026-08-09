//! `snob login`.
//!
//! Two ways in, and the order of what each one asks is deliberate. Everything
//! that can fail on its own — the store being writable, a browser being
//! installed, which browser — is settled **before** the user is asked for
//! anything, so nobody completes a two-factor login only to be told afterwards
//! that there was nowhere to put the result.

use std::io::IsTerminal;

use anyhow::{Result, bail};
use snob_core::paths::AppPaths;
use snob_core::secrets::{Backend, SecretStore};
use snob_core::session::Session;
use snob_core::store::rate_budget::SqliteRateBudget;
use snob_ig::login::{self, ValidationOutcome};
use snob_ig::pace::{CancelToken, Pacer};

use crate::cli::LoginArgs;
use crate::exit::ExitCode;
use crate::progress::Progress;
use crate::report;
use crate::ui::{self, LoginMethod};
use crate::{browser, cdp, interrupt};

pub async fn run(args: LoginArgs, store: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    // Before anything is asked for: if the session cannot be stored anywhere,
    // better to find out now than after two-factor authentication.
    let wanted = store.backend();
    let usable = match store.probe_writable() {
        Ok(backend) => backend,
        Err(e) => bail!(
            "{e}\n\
             There is nowhere to put the session: neither the system keyring nor \
             a file in the data directory could be written."
        ),
    };

    // Falling back is fine. Falling back quietly is not: a session stored
    // somewhere less protected than the user expected should say so.
    if wanted == Backend::Keyring && usable == Backend::File {
        ui::warn(
            "no system keyring is available here, so the session goes to a \
             protected file instead.\n\
             On a headless machine that is normal — Secret Service needs a \
             desktop session — and the file is readable only by you.",
        );
    }
    let store = store.using(usable);

    // Before anything is asked for, and this is the half the probe above does
    // not cover: `probe_writable` returns as soon as the keyring answers, so it
    // never touches the data directory at all on a machine that has one. The
    // database was then opened for the first time inside `finish`, *after* the
    // browser login or the paste — so a full or read-only data directory threw
    // away a login the user had already completed. With `--paste` that means
    // fetching and pasting the sessionid again; with `--browser`, relaunching.
    let pacer = pacer(paths)?;

    announce_replacement(&store);

    let Some(method) = choose_method(&args)? else {
        ui::info("Login canceled.");
        return Ok(ExitCode::Ok);
    };

    match method {
        LoginMethod::Paste => by_paste(args, store, pacer).await,
        LoginMethod::Browser => by_browser(args, store, paths, pacer).await,
    }
}

/// Says whose session is about to be replaced.
///
/// Logging in over an existing session is usually deliberate, so this does not
/// ask — but running it by accident and silently losing the account you were
/// on is a surprise worth one line.
fn announce_replacement(store: &SecretStore) {
    let Ok(Some(session)) = store.load() else {
        return;
    };
    ui::info(&format!(
        "There is already a session for {}. Logging in again replaces it.",
        who(&session)
    ));
}

/// How to name the account a session belongs to.
fn who(session: &Session) -> String {
    crate::app::Viewer {
        pk: session.ds_user_id,
        username: session.username.clone(),
    }
    .label()
}

/// The budget the one validation request is charged to.
///
/// Checking a session is a request like any other, so it is counted like any
/// other. Before this it was free, which is how repeated logins could spend
/// without the budget ever knowing.
fn pacer(paths: &AppPaths) -> Result<Pacer> {
    // The store goes first: it is what creates the schema the budget then
    // opens its own connection to.
    snob_core::store::Store::open(paths)?;
    Ok(Pacer::new(std::sync::Arc::new(SqliteRateBudget::open(
        paths,
    )?)))
}

/// Works out the method: whatever the flags say, or whatever the user picks
/// from the menu. With no interactive terminal there is no menu, so the method
/// has to be given explicitly.
fn choose_method(args: &LoginArgs) -> Result<Option<LoginMethod>> {
    if args.paste {
        return Ok(Some(LoginMethod::Paste));
    }
    if args.browser {
        return Ok(Some(LoginMethod::Browser));
    }
    if !ui::is_interactive() {
        bail!(
            "there is no interactive terminal to show the menu in.\n\
             Give the method explicitly, for example \"snob login --paste\"."
        );
    }
    ui::choose_login_method()
}

/// Which browser this login is about.
///
/// With one installed there is nothing to ask. With several there is, and it
/// matters both ways: for `--browser` it decides which one opens, and for
/// `--paste` it decides the User-Agent the session will be tied to. Guessing
/// gets it wrong about half the time on a machine with Chrome and Edge.
///
/// The list is passed in rather than detected here. Off Windows, detecting
/// means launching every installed browser to ask its version, so
/// `resolve_user_agent` calling this after its own `detect_all` spawned six
/// processes to answer a question worth three. Detecting twice also meant the
/// count that decides whether to ask "is that the right browser?" came from a
/// different snapshot than the browser actually chosen.
fn choose_browser(purpose: &str, installed: &[browser::Browser]) -> Option<browser::Browser> {
    match installed {
        [] => None,
        [only] => Some(only.clone()),
        _ if !ui::is_interactive() => installed.first().cloned(),
        _ => {
            let labels: Vec<String> = installed
                .iter()
                .map(|b| format!("{} {}", b.name, b.major_version))
                .collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            match ui::choose(purpose, &refs) {
                // `get` rather than `nth`: total where the old one relied on the
                // index being in range.
                Ok(Some(i)) => installed.get(i).cloned(),
                // Esc, or no menu to show: the preferred one still beats
                // refusing to continue.
                _ => installed.first().cloned(),
            }
        }
    }
}

/// Opens a browser, waits for the login, and takes the session from it.
///
/// Nothing is read out of the user's own browser profile. This one is ours,
/// under our data directory, and the browser hands the cookies over itself
/// through its debugging protocol. `snob logout --purge-profile` deletes it.
async fn by_browser(
    args: LoginArgs,
    store: SecretStore,
    paths: &AppPaths,
    pacer: Pacer,
) -> Result<ExitCode> {
    let installed = browser::detect_all();
    let Some(found) = choose_browser("Which browser should snob open?", &installed) else {
        bail!(
            "no Chromium-based browser was found installed, and this needs one to \
             open.\n\
             Use \"snob login --paste\" instead."
        );
    };

    let profile = paths.browser_profile();
    let cancel = interrupt::install();

    ui::info(&format!(
        "Opening {} on Instagram's login page.\n\
         It uses a profile of its own, at {}, so it is not your everyday browser \
         and logging in there changes nothing about it.\n\
         Log in as usual; snob will notice when you are done, and gives up after \
         {} minutes. Ctrl+C to cancel.",
        found.name,
        profile.display(),
        cdp::LOGIN_TIMEOUT.as_secs() / 60
    ));

    // Anything from here on can be interrupted, and a Ctrl+C has to read as
    // one rather than as a failure, so the result is held rather than unwrapped
    // until the browser has been shut down.
    let captured = capture(&found, args.user_agent.clone(), paths, &cancel).await;
    if cancel.is_canceled() {
        ui::info("Login canceled.");
        return Ok(ExitCode::Interrupted);
    }
    let (cookies, user_agent) = captured?;

    let mut session = login::session_from_cookies(&cookies, &user_agent)?;
    session.user_agent_pinned = args.user_agent.is_some();
    session.browser = Some(found.name.to_string());
    finish(session, store, pacer, LoginMethod::Browser).await
}

/// Drives the browser from launch to captured session, and always closes it.
///
/// A browser that is never connected to is killed by its own `Drop`, so only
/// the connected case needs closing by hand — which is why the result is held
/// rather than propagated until after `close`.
async fn capture(
    found: &browser::Browser,
    requested_user_agent: Option<String>,
    paths: &AppPaths,
    cancel: &CancelToken,
) -> Result<(login::BrowserCookies, String)> {
    let mut cdp = cdp::Cdp::connect(cdp::launch(found, paths, cancel).await?).await?;
    let outcome = collect(&mut cdp, found, requested_user_agent, cancel).await;
    cdp.close().await;
    outcome
}

async fn collect(
    cdp: &mut cdp::Cdp,
    found: &browser::Browser,
    requested_user_agent: Option<String>,
    cancel: &CancelToken,
) -> Result<(login::BrowserCookies, String)> {
    // Asked for rather than reconstructed: this is the browser the cookie will
    // belong to, and Instagram checks that the two agree. An explicit
    // --user-agent still wins, since it is there to override exactly this.
    let user_agent = match requested_user_agent {
        Some(ua) => ua,
        None => cdp.user_agent().await.unwrap_or_else(|e| {
            tracing::debug!(error = %e, "falling back to the rebuilt User-Agent");
            found.user_agent()
        }),
    };

    // The profile keeps whatever was logged in last time, and Instagram sends a
    // browser that still has a session straight past the login page. Saying so
    // beats announcing an account nobody chose here.
    let cookies = match cdp.instagram_cookies().await? {
        Some(existing) => {
            ui::info(
                "This browser profile was still logged in, so that session is the one \
                 being stored.\n\
                 To sign in as somebody else, run \"snob logout --purge-profile\" first.",
            );
            existing
        }
        None => {
            // Ten minutes with nothing on screen reads as a hang, and this is
            // the one stretch where the user is in another window typing a
            // password and a code and comes back to check.
            //
            // Started inside this branch rather than before `capture()`: the
            // sibling branch above prints with a plain `eprintln!`, which would
            // be overdrawn by a bar already running. The countdown is
            // deadline-driven, so one call covers the whole wait.
            //
            // `stderr().is_terminal()` rather than `true`, because that is
            // where the bar draws — and rather than `ui::is_interactive()`,
            // which asks about stdin and stdout.
            let progress = Progress::new(std::io::stderr().is_terminal());
            progress.waiting("waiting for you to log in", cdp::LOGIN_TIMEOUT);
            let captured = cdp::wait_for_login(cdp, cancel).await;
            // Before the `?`, so both the Ctrl+C bail and a broken socket leave
            // a clean terminal behind.
            progress.finish();
            captured?
        }
    };

    Ok((cookies, user_agent))
}

async fn by_paste(args: LoginArgs, store: SecretStore, pacer: Pacer) -> Result<ExitCode> {
    // The User-Agent is settled first because it is the half that can still ask
    // questions. Sorting it out after a seventy-character paste means answering
    // a menu with the credential already sitting on screen.
    let chosen = match args.user_agent {
        // The flag is the user saying it outright.
        Some(ua) => ChosenAgent::pinned(ua),
        None => resolve_user_agent()?,
    };

    ui::paste_instructions();

    let sessionid = ui::prompt_secret("sessionid: ")?;
    if sessionid.trim().is_empty() {
        bail!("no sessionid was entered");
    }

    let mut session = login::session_from_paste(&sessionid, &chosen.user_agent)?;
    session.user_agent_pinned = chosen.pinned;
    session.browser = chosen.browser;
    finish(session, store, pacer, LoginMethod::Paste).await
}

/// Validates, stores and reports. Shared by both methods so a session obtained
/// either way is checked and announced identically.
async fn finish(
    mut session: Session,
    store: SecretStore,
    pacer: Pacer,
    method: LoginMethod,
) -> Result<ExitCode> {
    ui::info("Checking the session against Instagram...");
    let outcome = login::validate(&mut session, pacer).await.map_err(|e| {
        // A rejection during login means different things depending on where
        // the session came from. Telling someone to check what they pasted is
        // useless advice when the browser handed it over itself.
        match &e {
            login::LoginError::Instagram(ig) if ig.invalidates_session() => anyhow::anyhow!(e)
                .context(match method {
                    LoginMethod::Paste => {
                        "Instagram rejected the session. Check that the sessionid was \
                             copied whole and that the User-Agent belongs to the same browser"
                    }
                    LoginMethod::Browser => {
                        "Instagram rejected the session the browser handed over. It may \
                             have been signed out in the meantime; try again"
                    }
                }),
            _ => anyhow::anyhow!(e),
        }
    })?;

    store.save(&session)?;

    match outcome {
        ValidationOutcome::Confirmed => {
            println!(
                "Session stored for {} in the {}.",
                who(&session),
                store.describe()
            );
        }
        ValidationOutcome::Unconfirmed => {
            println!("Session stored in the {}.", store.describe());
            ui::warn(
                "it could not be confirmed with Instagram because it is throttling \
                 requests. The session is probably valid; check in a few minutes \
                 with \"snob whoami\".",
            );
        }
        ValidationOutcome::Skipped { until_ms } => {
            println!("Session stored in the {}.", store.describe());
            ui::warn(&format!(
                "it was not checked: the account is in cooldown until {}, and during one \
                 nothing is spent — not even the single request this would cost. \
                 Check it with \"snob whoami\" once it lifts.",
                report::cooldown_ends_at(until_ms)
            ));
        }
    }

    // Somebody who has just logged in has no idea what to type next, and the
    // whole-account summary is the answer to the question they came with.
    ui::info("Try \"snob scan\" for the whole picture, or \"snob unfollowers\".");

    Ok(ExitCode::Ok)
}

/// A User-Agent and what is known about where it came from.
struct ChosenAgent {
    user_agent: String,
    /// The user gave it outright, so nothing may rewrite it later.
    pinned: bool,
    /// Which browser it describes, when one was picked. This is what lets the
    /// daily refresh follow the right browser on a machine with several.
    browser: Option<String>,
}

impl ChosenAgent {
    fn pinned(user_agent: String) -> Self {
        Self {
            user_agent,
            pinned: true,
            browser: None,
        }
    }
}

/// Gets the User-Agent without sending the user to the browser console.
///
/// Instagram shows a warning in that console saying that anyone asking you to
/// paste something there is scamming you, and copying from it easily drags in
/// dozens of log lines that end up running as commands in the terminal. It is
/// rebuilt from the installed browser's version, which is the only part of
/// Chrome's User-Agent that varies since Google reduced it.
fn resolve_user_agent() -> Result<ChosenAgent> {
    let installed = browser::detect_all();

    if let Some(b) = choose_browser("Which browser is your Instagram session in?", &installed) {
        let user_agent = b.user_agent();
        ui::info(&format!(
            "Using the User-Agent of {} {}.",
            b.name, b.major_version
        ));

        // With one browser installed nothing was asked, so the confirmation is
        // the only chance to say it guessed wrong.
        if installed.len() == 1
            && ui::is_interactive()
            && !ui::confirm("Is that the browser your session is in?", true)?
        {
            return prompt_user_agent();
        }
        return Ok(ChosenAgent {
            user_agent,
            pinned: false,
            browser: Some(b.name.to_string()),
        });
    }

    ui::warn("no Chromium-based browser was found installed");
    prompt_user_agent()
}

/// Asks for it outright, which is the last resort and also the most explicit
/// thing the user can do — so what comes back is pinned. Following a browser's
/// updates makes no sense for a string we could not have produced ourselves.
fn prompt_user_agent() -> Result<ChosenAgent> {
    eprintln!(
        "\n\
         In the address bar of the browser your session is in, go to:\n\
        \n\
             about:version\n\
        \n\
         and copy the whole \"User Agent\" line. Your browser may show that\n\
         label translated.\n"
    );

    let text = ui::prompt_line("User-Agent: ")?;

    if ui::looks_like_console_dump(&text) {
        bail!(
            "that looks like a dump of the browser console, not a User-Agent.\n\
             Copying from the \"Console\" tab easily picks up dozens of lines, and \
             pasting them makes the terminal try to run each one.\n\
             Use \"about:version\", which shows the value on its own."
        );
    }

    Ok(ChosenAgent::pinned(text))
}
