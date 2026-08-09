use anyhow::Result;
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;
use snob_core::store::Store;
use snob_core::store::rate_budget::SqliteRateBudget;
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;

use crate::cli::WhoamiArgs;
use crate::exit::ExitCode;
use crate::ui;

pub async fn run(args: WhoamiArgs, store: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    let Some(mut session) = store.load()? else {
        ui::no_session();
        return Ok(ExitCode::NoSession);
    };

    let mut alive = None;
    // Why the session was not checked, when it was not, and the two facts the
    // command holds and used to throw away: when a cooldown ends, and what
    // Instagram actually said. The address in a challenge is held exactly once
    // ever — nothing stores it — so dropping it means the user has to provoke
    // the error again to see it.
    let mut not_checked: Option<&'static str> = Some("offline");
    let mut cooldown_until: Option<i64> = None;
    let mut failure: Option<serde_json::Value> = None;
    // The code the command exits with, held rather than returned so the JSON
    // still gets printed on the way out. Reporting `alive: false` under exit 0
    // told a script the opposite of what the body said, and the machine format
    // is precisely the one that cannot read the sentence.
    let mut code = ExitCode::Ok;

    if !args.offline {
        // The store goes first: it is what creates the schema the budget then
        // opens its own connection to.
        Store::open(paths)?;
        let pacer = Pacer::new(std::sync::Arc::new(SqliteRateBudget::open(paths)?));

        // A cooldown means nothing is spent, and checking a session is a
        // request like any other. `--offline` is the way to ask anyway, and it
        // is what this falls back to.
        not_checked = None;
        if let Some(until_ms) = pacer.cooldown()? {
            not_checked = Some("cooldown");
            // Seconds, like `created_at` and `validated_at` in the same object.
            // `pacer.cooldown()` answers in milliseconds, and `div_euclid`
            // rather than `/` for the reason pinned in `report`.
            cooldown_until = Some(until_ms.div_euclid(1000));
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
                        "url": challenge_url(&e),
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
            "not_checked": not_checked,
            "cooldown_until": cooldown_until,
            "error": failure,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("{}", describe(&session, alive));
    }

    Ok(code)
}

/// The address that clears a check, when Instagram gave one.
///
/// `IgError` carries it precisely so it can be shown, and `whoami` is the one
/// command that can hand it to a caller rather than only to a person: it is
/// validated against instagram.com before it ever reaches an `IgError`, so
/// passing it on adds no trust.
fn challenge_url(e: &snob_ig::error::IgError) -> Option<&str> {
    use snob_ig::error::IgError;
    match e {
        IgError::Challenge { url } | IgError::Checkpoint { url } => url.as_deref(),
        _ => None,
    }
}

fn describe(session: &snob_core::session::Session, alive: Option<bool>) -> String {
    let mut lines = Vec::new();

    match &session.username {
        Some(u) => lines.push(format!("Account:  @{u} ({})", session.ds_user_id)),
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
            assert_eq!(challenge_url(&e), Some(url));
        }

        // Everything else has no address, and must not invent one.
        assert_eq!(challenge_url(&IgError::SessionExpired), None);
        assert_eq!(challenge_url(&IgError::Challenge { url: None }), None);
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
    #[test]
    fn the_cooldown_is_emitted_in_the_same_unit_as_the_timestamps() {
        let until_ms = 1_786_310_990_123_i64;
        assert_eq!(until_ms.div_euclid(1000), 1_786_310_990);

        // And `div_euclid` rather than `/`, so a value before the epoch does
        // not round towards zero into the wrong second.
        assert_eq!((-1_500_i64).div_euclid(1000), -2);
    }
}
