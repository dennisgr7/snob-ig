//! `snob watch diff`: what changed, out of storage, changing nothing.

use anyhow::Result;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::WatchDiffArgs;
use crate::commands::common;
use crate::exit::ExitCode;

use super::say::describe;
use super::wire::{self, as_json};

/// Shows what changed, and changes nothing.
///
/// The marks are deliberately left where they are. This command is a question,
/// and a question whose answer is different the second time it is asked is one
/// nobody can check: somebody who runs it, reads three departures and runs it
/// again to copy the names must get the same three. Advancing the marks is a
/// thing `snob watch` does when it has actually reported them somewhere.
pub(super) fn diff(
    args: WatchDiffArgs,
    secrets: SecretStore,
    paths: &AppPaths,
) -> Result<ExitCode> {
    // Nothing is fetched here, so there is no bar to draw.
    let app = common::app(&secrets, paths, false)?;

    let report = crate::engine::watch::from_store(&app, args.target.as_deref())?;

    if args.output.json {
        // With `schema`, like every other message: this was the one JSON this
        // command family emits with no version on it, while the README says
        // each one carries it.
        let mut out = as_json(&report);
        out["schema"] = serde_json::json!(wire::SCHEMA);
        crate::ui::say!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(ExitCode::Ok);
    }

    for line in describe(&report, false) {
        crate::ui::say!("{line}");
    }
    Ok(ExitCode::Ok)
}
