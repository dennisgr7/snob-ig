use anyhow::Result;
use snob_ig::client::IgClient;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::WhoamiArgs;
use crate::exit::ExitCode;
use crate::ui;

pub async fn run(args: WhoamiArgs, store: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    let Some(mut session) = store.load()? else {
        ui::no_session();
        // `--json` still gets an object. A session that has *died* already
        // produced a full one with `alive: false` and `error.code:
        // "no_session"`, and no session at all produced zero bytes on standard
        // output with the same exit code — so the two states an automation most
        // wants to tell apart were one code, and one of them handed the parser
        // nothing to read. Every field the object always carries is here;
        // everything that describes a session that does not exist is null.
        if args.json {
            crate::ui::say!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "pk": serde_json::Value::Null,
                    "username": serde_json::Value::Null,
                    "origin": serde_json::Value::Null,
                    "storage": store.backend().as_str(),
                    "storage_path": store.storage_path(),
                    "created_at": serde_json::Value::Null,
                    "validated_at": serde_json::Value::Null,
                    "alive": false,
                    "not_checked": "no_session",
                    "cooldown_until": serde_json::Value::Null,
                    "error": {
                        "code": ExitCode::NoSession.as_str(),
                        "message": "no session is stored on this computer",
                    },
                }))?
            );
        }
        return Ok(ExitCode::NoSession);
    };

    let mut alive = None;
    // The two facts the command holds and used to throw away: when a cooldown
    // ends, and what Instagram actually said. The address in a challenge is held
    // exactly once ever — nothing stores it — so dropping it means the user has
    // to provoke the error again to see it.
    //
    // Why the session was not checked is not a third variable. It is these two
    // read back at the point it is reported, so a return added inside the block
    // below cannot leave it claiming a check that never happened.
    let mut cooldown_until: Option<i64> = None;
    let mut failure: Option<serde_json::Value> = None;
    // The code the command exits with, held rather than returned so the JSON
    // still gets printed on the way out. Reporting `alive: false` under exit 0
    // told a script the opposite of what the body said, and the machine format
    // is precisely the one that cannot read the sentence.
    let mut code = ExitCode::Ok;

    if !args.offline {
        // Assembled by `app::pacer` rather than here, so this one is wired like
        // every other: with the process's cancellation token, and with somebody
        // to tell when the budget imposes a wait. It had neither, so `snob
        // whoami` on a rationed bucket sat silent for as long as the debt
        // lasted and Ctrl+C did not reach it.
        let pacer = crate::app::pacer(
            paths,
            std::sync::Arc::new(|waited: std::time::Duration| {
                ui::info(&format!(
                    "The request budget is rationing; waiting {}.",
                    snob_core::duration::format(waited)
                ));
            }),
        )?;

        // A cooldown means nothing is spent, and checking a session is a
        // request like any other. `--offline` is the way to ask anyway, and it
        // is what this falls back to.
        if let Some(until_ms) = pacer.cooldown()? {
            // Seconds, like `created_at` and `validated_at` in the same object.
            // The conversion comes from `report`, which owns the rule and prints
            // the same cooldown as a date on the next line.
            cooldown_until = Some(crate::report::cooldown_ends_at_secs(until_ms));
            eprintln!(
                "The account is in cooldown until {}, so the session was not checked.",
                crate::report::cooldown_ends_at(until_ms)
            );
        } else {
            let client = IgClient::new(session.clone(), pacer)?;
            match client.whoami().await {
                Ok(identity) => {
                    alive = Some(true);
                    if session.username.is_none() && identity.username.is_some() {
                        session.username = identity.username;
                    }
                    session.mark_validated();
                    // Worth persisting so the name is not looked up again —
                    // and this is the only command that writes it back, so a
                    // silent failure here means every later run pays for the
                    // lookup again and nothing ever says why.
                    if let Err(e) = store.save(&session) {
                        ui::warn(&format!(
                            "the session could not be updated, so the account name will be \
                             looked up again next time: {e}"
                        ));
                    }
                }
                Err(e) => {
                    alive = Some(false);
                    code = ExitCode::from_ig_error(&e);
                    failure = Some(serde_json::json!({
                        "code": code.as_str(),
                        // Validated against instagram.com before it ever reached
                        // an `IgError`, so handing it to a caller adds no trust.
                        "url": e.challenge_url(),
                        "message": e.to_string(),
                    }));
                    // The detail goes to standard error either way, so the JSON
                    // on standard output stays parseable.
                    eprintln!("The session is not responding: {e}");
                }
            }
        }
    }

    if args.json {
        // Every value here is a stable token, never a human-facing string —
        // with exactly one exception, `error.message`, which is documentation
        // for a person and which nothing may branch on. Rewording anything else
        // would break this contract.
        //
        // `cooldown_until` says **when**, never **why**: the reason is written
        // to the cooldowns table but the read side does not hand it back, and
        // widening a snob-core trait for it is not worth doing here.
        let out = serde_json::json!({
            "pk": session.ds_user_id,
            "username": session.username,
            "origin": session.origin.as_str(),
            "storage": store.backend().as_str(),
            "storage_path": store.storage_path(),
            "created_at": session.created_at,
            "validated_at": session.validated_at,
            "alive": alive,
            "not_checked": not_checked(args.offline, cooldown_until),
            "cooldown_until": cooldown_until,
            "error": failure,
        });
        crate::ui::say!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        crate::ui::say!("{}", describe(&session, alive));
    }

    Ok(code)
}

/// Why the session was not checked, when it was not.
///
/// Derived rather than tracked. Both inputs are already held for their own sake
/// — `--offline` is the request not to look, and a cooldown end only exists when
/// one was found — so there is nothing here that can disagree with them.
fn not_checked(offline: bool, cooldown_until: Option<i64>) -> Option<&'static str> {
    if offline {
        Some("offline")
    } else if cooldown_until.is_some() {
        Some("cooldown")
    } else {
        None
    }
}

fn describe(session: &snob_core::session::Session, alive: Option<bool>) -> String {
    let mut lines = Vec::new();

    match &session.username {
        // Filtered here and not in the JSON above: this line is drawn on a
        // terminal, and the name was written by `whoami` out of Instagram's
        // answer. `serde_json` escapes what it emits, and a machine format has
        // to carry the true value.
        Some(u) => lines.push(format!(
            "Account:  @{} ({})",
            snob_core::model::printable(u),
            session.ds_user_id
        )),
        None => lines.push(format!("Account:  {}", session.ds_user_id)),
    }
    lines.push(format!("Origin:   {}", session.origin));
    lines.push(match alive {
        Some(true) => "Status:   the session is responding".to_string(),
        Some(false) => "Status:   the session is NOT responding".to_string(),
        None => "Status:   not checked".to_string(),
    });

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use snob_ig::error::IgError;

    use super::*;

    /// The one thing a caller can act on: the address that clears the check.
    /// It is held exactly once ever — nothing stores it — so dropping it means
    /// provoking the error again to see it.
    #[test]
    fn a_challenge_hands_over_its_address() {
        let url = "https://www.instagram.com/challenge/123/abc/";
        for e in [
            IgError::Challenge {
                url: Some(url.into()),
            },
            IgError::Checkpoint {
                url: Some(url.into()),
            },
        ] {
            assert_eq!(e.challenge_url(), Some(url));
        }

        // Everything else has no address, and must not invent one.
        assert_eq!(IgError::SessionExpired.challenge_url(), None);
        assert_eq!(IgError::Challenge { url: None }.challenge_url(), None);
    }

    /// The tokens in the JSON are the ones the README's exit-code table uses,
    /// so `$?` and the object say the same thing by the same name.
    #[test]
    fn every_code_has_a_stable_token() {
        let codes = [
            (ExitCode::Ok, "ok"),
            (ExitCode::Error, "error"),
            (ExitCode::NoSession, "no_session"),
            (ExitCode::Challenge, "challenge"),
            (ExitCode::RateLimited, "rate_limited"),
            (ExitCode::Interrupted, "interrupted"),
        ];
        let mut seen = std::collections::HashSet::new();
        for (code, token) in codes {
            assert_eq!(code.as_str(), token);
            assert!(seen.insert(token), "{token} is used twice");
        }
    }

    /// `pacer.cooldown()` answers in milliseconds while `created_at` next to it
    /// in the same object is in seconds. Emitting the raw value would put two
    /// units in one object with nothing saying so.
    ///
    /// Asserted through the function that owns the conversion, not against the
    /// arithmetic copied out of it: pinning `div_euclid` here would keep passing
    /// while `report` did something else and the date on screen disagreed with
    /// the field beside it.
    #[test]
    fn the_cooldown_is_emitted_in_the_same_unit_as_the_timestamps() {
        use crate::report::cooldown_ends_at_secs;

        assert_eq!(cooldown_ends_at_secs(1_786_310_990_123), 1_786_310_990);

        // And flooring rather than rounding towards zero, so a value before the
        // epoch does not land in the wrong second.
        assert_eq!(cooldown_ends_at_secs(-1_500), -2);
    }

    /// The three states, and the one that matters: `--offline` is why nothing
    /// was checked even when a cooldown is also in force, because the user asked
    /// not to look before anything went to find out.
    #[test]
    fn why_nothing_was_checked_is_read_back_off_the_two_facts() {
        assert_eq!(not_checked(true, None), Some("offline"));
        assert_eq!(not_checked(true, Some(1)), Some("offline"));
        assert_eq!(not_checked(false, Some(1)), Some("cooldown"));
        assert_eq!(not_checked(false, None), None);
    }
}
