use anyhow::Result;
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;
use snob_core::store::Store;
use snob_core::store::rate_budget::SqliteRateBudget;
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;

use crate::cli::WhoamiArgs;
use crate::exit::ExitCode;

pub async fn run(args: WhoamiArgs, store: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    let Some(mut session) = store.load()? else {
        eprintln!("No session stored. Run \"snob login\".");
        return Ok(ExitCode::NoSession);
    };

    let mut alive = None;
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
        if let Some(until_ms) = pacer.cooldown()? {
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
                    // Worth persisting so the name is not looked up again.
                    let _ = store.save(&session);
                }
                Err(e) => {
                    alive = Some(false);
                    code = ExitCode::from_ig_error(&e);
                    // The detail goes to standard error either way, so the JSON
                    // on standard output stays parseable.
                    eprintln!("The session is not responding: {e}");
                }
            }
        }
    }

    if args.json {
        // Every value here is a stable token, never a human-facing string:
        // rewording a message must not be able to break this contract.
        let out = serde_json::json!({
            "pk": session.ds_user_id,
            "username": session.username,
            "origin": session.origin.as_str(),
            "storage": store.backend().as_str(),
            "storage_path": store.storage_path(),
            "created_at": session.created_at,
            "validated_at": session.validated_at,
            "alive": alive,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("{}", describe(&session, alive));
    }

    Ok(code)
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
