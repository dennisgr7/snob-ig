//! `snob watch check`: the configuration answered for while somebody is still
//! here to read the answer.
//!
//! The questions are `super::preflight`, because `setup` asks them too. What is
//! here is the command that puts them and the two ways it says what came
//! back.

use anyhow::Result;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::WatchCheckArgs;
use crate::exit::ExitCode;

use super::preflight::{describe_check, preflight};
use super::wire::check_json;

/// Checks the configuration would work, before it runs unattended.
///
/// Everything `setup` writes down is a claim about a machine, a session and
/// somebody else's server, and every one of them used to be tested for the
/// first time by an unattended run at three in the morning. This puts the same
/// questions while there is still somebody to answer them.
///
/// **It writes nothing and walks no list.** `engine::check` takes `&App`, so it
/// cannot reach the `&mut Store` that recording needs — the guard `watch diff`
/// already rests on — and the cost is one request for the session plus one per
/// configured account. That is what makes it safe to point a monitoring system
/// at.
///
/// It degrades rather than stopping. A missing session does not hide a broken
/// schedule, and a broken schedule does not hide a webhook that has stopped
/// answering: every line is reported, and the exit code is the worst of them.
pub(super) async fn check(
    args: WatchCheckArgs,
    secrets: SecretStore,
    paths: &AppPaths,
) -> Result<ExitCode> {
    let report = preflight(&args, &secrets, paths).await?;

    if args.output.json {
        crate::ui::say!("{}", serde_json::to_string_pretty(&check_json(&report))?);
    } else {
        for line in describe_check(&report) {
            crate::ui::say!("{line}");
        }
    }

    Ok(report.verdict().exit_code())
}
