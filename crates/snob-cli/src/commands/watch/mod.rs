//! `snob watch`: the monitor.
//!
//! Two ways to ask the same question. `diff` answers out of storage and leaves
//! everything as it found it, so it can be run as often as anybody likes and
//! costs nothing. `once` goes and looks, reports, and remembers having
//! reported — which is the difference that matters, because what a run reports
//! it does not report again.
//!
//! The scheduled mode is what remains: it is `once` on a timer, and the report
//! it produces is the same one.
//!
//! Wording, dates and exit codes live here. What actually changed is
//! [`crate::engine::watch`]'s answer, and this never recomputes any of it.
//!
//! Getting a report to a receiver is [`delivery`], and the split is by question
//! rather than by layer: this module decides what a report says, that one
//! decides where it goes and what travels with it. It is a module of its own
//! because the rules in it are about one another and were six hundred lines
//! apart -- whether a stored credential may be attached, whether the address it
//! would go to can be posted to at all, and what a queued report may be handed
//! to on a later run are one rule asked three times.

use anyhow::Result;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::{WatchArgs, WatchCommand};
use crate::exit::ExitCode;

pub(super) mod delivery;

/// The two commands that are about the configuration rather than about a
/// report. They were one file beside this one, `commands::watch_setup`, which
/// made `snob watch setup` a sibling of `snob watch` in the source and a child
/// of it everywhere else — and left them reaching back in through
/// `super::watch::` for the seven helpers they share with the rest of the
/// monitor. Each of those is a file of its own now, and they name it.
pub(super) mod setup;
pub(super) mod status;

/// A report said two ways. [`wire`] is what a receiver is sent and what a
/// signature covers; [`say`] is what a person reads. They were four hundred and
/// fifty lines apart in this file with the orchestration interleaved between
/// them, so "does this change break somebody's integration" and "is this
/// sentence right" were the same question about the same page.
pub(super) mod say;
pub(super) mod wire;

/// The commands somebody types, one file each, and the loop that runs the
/// first of them on a timer. What the four have in common is `run`: one run of
/// the monitor over the accounts it watches, which is the whole of `once` and
/// one turn of `scheduled`.
mod check;
mod diff;
mod once;
mod run;
mod scheduled;

/// What a run is told before it starts — which accounts it is for and on what
/// schedule — and, for `check` and `setup`, whether any of it would work at
/// all. Read by the commands above and by `setup` and `status`, which is why
/// they are not part of any one of them.
mod preflight;
mod schedule;
mod watched;

/// Shapes for the tests of every file here.
#[cfg(test)]
mod fixtures;

use check::check;
use diff::diff;
use once::once;
use scheduled::scheduled;

pub async fn run(args: WatchArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    match args.command {
        Some(WatchCommand::Diff(args)) => diff(args, secrets, paths),
        Some(WatchCommand::Once(args)) => once(args, secrets, paths).await,
        Some(WatchCommand::Check(args)) => check(args, secrets, paths).await,
        Some(WatchCommand::Setup(args)) => setup::setup(args, secrets, paths).await,
        Some(WatchCommand::Status(args)) => status::status(args, paths),
        None => scheduled(args.run, secrets, paths).await,
    }
}
