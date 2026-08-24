//! The checks behind `snob watch check`, and the baseline `setup` lays down.
//!
//! A file of its own because `setup` asks the same questions: they are asked of
//! a configuration rather than of a run, and both callers want every one of
//! them answered rather than the first failure.

use anyhow::Result;
use snob_core::Pk;
use snob_core::model::printable;
use snob_store::config::{self, WatchConfig};
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::{WatchCheckArgs, WatchRunArgs, WebhookArgs};
use crate::commands::common::{self, Session};
use crate::report;

use super::delivery::{delivery_from, run_id};
use super::run::open_and_run;
use super::schedule::schedule_from;
use super::watched::watched_from;
use super::wire::preflight_body;

/// The checks themselves, without the printing, so `setup` can run them too.
pub(super) async fn preflight(
    args: &WatchCheckArgs,
    secrets: &SecretStore,
    paths: &AppPaths,
) -> Result<crate::engine::check::CheckReport> {
    use crate::engine::check::{self, Verdict};

    let configured = config::load(paths)?;
    let now = snob_core::clock::now();

    // Built the same way a run builds it, or this would be checking a schedule
    // nobody is on. A configuration with none at all is not an error here — it
    // is one of the things worth reporting.
    //
    // **The error is kept.** This used to be `.ok()`, and `without_a_session`
    // pushed a schedule line only on `Some`, so a file whose schedule the
    // scheduler refuses produced no schedule line at all — and
    // `CheckReport::verdict()` is `max().unwrap_or(Ok)`, so `check` exited 0
    // about a monitor that dies at `schedule_from` on every single invocation.
    // The `NotConfigured` warning does not cover it either, because a file
    // exists. `schedule_of`'s own doc says it "is what catches a file the
    // scheduler would refuse at every run"; it was never reached for that case.
    // Every shape gets here through a hand-edit, which the first line of
    // `watch.toml` says is fine: `every = "5m"`, `cron = "0 9 * *"`,
    // `at = ["25:00"]`, `every` with `on` and no `at`. All pass `config::parse`,
    // which reads TOML, the schema number and one key clash, and nothing else.
    let schedule = configured
        .as_ref()
        .map(|c| schedule_from(&WatchRunArgs::default(), Some(c)).map_err(|e| e.to_string()));
    let mut report = check::without_a_session(configured.as_ref(), schedule.as_ref(), now);

    // Before the session, like `once` does, so an address that could never work
    // is reported even on a machine that cannot log in.
    let delivery = match delivery_from(&WebhookArgs::default(), configured.as_ref(), secrets) {
        Ok(delivery) => delivery,
        Err(e) => {
            report.checked.push(check::Checked {
                what: check::What::Webhook {
                    destination: String::new(),
                    status: None,
                    signed: false,
                },
                verdict: Verdict::Failed,
                problem: Some(check::Problem::Foreign(e.to_string())),
            });
            None
        }
    };

    match common::open_with_progress(false, secrets, paths)? {
        Session::Open(app) => {
            let watched = watched_from(None, configured.as_ref());
            check::with_a_session(&app, secrets, &watched, &mut report).await;
        }
        Session::Missing => report.checked.push(check::Checked {
            what: check::What::Session {
                viewer: None,
                backend: secrets.backend().as_str(),
            },
            verdict: Verdict::Failed,
            problem: Some(check::Problem::NoSession),
        }),
    }

    // The address as `check` may print it, with any path secret cut off.
    // `Delivery::destination` is the exact URL on purpose -- it is the outbox
    // key -- so the redaction happens here, where the report is built: every
    // line and every `--json` object downstream renders what this hands over,
    // and a Slack or Discord address printed whole is the secret printed whole.
    let shown = delivery
        .as_ref()
        .map(|d| (shown_destination(&d.destination), d.signed));

    if let Some(line) = not_posted(
        shown
            .as_ref()
            .map(|(destination, signed)| (destination.as_str(), *signed)),
        args.no_webhook,
    ) {
        // Not posted is not the same as nowhere to post, and with no line at
        // all the two were the same report.
        report.checked.push(line);
    } else if let Some(delivery) = delivery.as_ref() {
        let id = run_id(now, Pk::new(0));
        let body = serde_json::to_string(&preflight_body(&id, now))?;
        report.checked.push(
            check::webhook_of(
                &delivery.client,
                shown_destination(&delivery.destination),
                delivery.signed,
                &id,
                &body,
            )
            .await,
        );
    }

    Ok(report)
}

/// [`crate::watch::webhook::shown`], asked of the outbox's exact address.
///
/// The parse cannot really fail -- the destination began as `Url::as_str` --
/// but an address that somehow does not parse is shown as it came rather than
/// dropped, because which address was meant is the content of the line.
fn shown_destination(destination: &str) -> String {
    url::Url::parse(destination).map_or_else(
        |_| destination.to_string(),
        |url| crate::watch::webhook::shown(&url),
    )
}

/// The webhook line for a run that is deliberately not posting one.
///
/// `None` when there is nothing to say — no webhook configured at all, or a run
/// that is about to post and will report what came back.
///
/// With `--no-webhook` nothing was pushed at all, so `check --json` had no
/// webhook object and the terminal no webhook line: byte for byte the report a
/// machine with no `[webhook]` produces. That is the one flag somebody reaches
/// for to check everything else without disturbing a receiver, and it made "is a
/// receiver configured?" unanswerable from the probe's own output. The
/// validation that did happen went with it — by the time this is reached
/// `delivery_from` has parsed the address and put every configured header
/// through `webhook::check`, and a failure there is already its own `Failed`
/// line, so what is left is a check that passed and was thrown away.
///
/// The flag is an argument rather than a condition at the call site, because the
/// defect is the decision not to push a line: a helper that only built one would
/// leave the revert green.
fn not_posted(
    webhook: Option<(&str, bool)>,
    no_webhook: bool,
) -> Option<crate::engine::check::Checked> {
    use crate::engine::check::{Checked, Problem, Verdict, What};

    let (destination, signed) = webhook.filter(|_| no_webhook)?;
    Some(Checked {
        what: What::Webhook {
            destination: destination.to_string(),
            status: None,
            signed,
        },
        verdict: Verdict::Ok,
        problem: Some(Problem::NotPosted),
    })
}

/// Walks the configured accounts once, so there is something to compare
/// against.
///
/// One ordinary run with no webhook: a baseline has nothing to report, so there
/// is nothing to send, and going through the same path as every other run is
/// what stops this being a second way of laying one down.
pub(super) async fn baseline_now(
    configured: Option<&WatchConfig>,
    secrets: &SecretStore,
    paths: &AppPaths,
) -> Result<()> {
    let watched = watched_from(None, configured);
    open_and_run(&WatchRunArgs::default(), &watched, None, secrets, paths).await
}

/// One line per check, in the order they were made.
pub(super) fn describe_check(report: &crate::engine::check::CheckReport) -> Vec<String> {
    use crate::engine::check::{Verdict, What};

    let mut lines = Vec::new();
    for checked in &report.checked {
        let (label, detail) = match &checked.what {
            What::NotConfigured => ("config".to_string(), String::new()),
            What::Schedule { next } => (
                "schedule".to_string(),
                next.iter()
                    .map(|at| report::stored_on(*at))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            What::Session { viewer, backend } => (
                "session".to_string(),
                match viewer {
                    Some(name) => format!("@{} ({backend})", printable(name)),
                    None => format!("({backend})"),
                },
            ),
            What::Account {
                target,
                followers,
                following,
                ..
            } => (
                crate::app::target_label(target.as_deref()),
                match (followers, following) {
                    (Some(a), Some(b)) => format!("{a} followers, {b} following"),
                    _ => String::new(),
                },
            ),
            What::Webhook {
                destination,
                status,
                signed,
            } => (
                "webhook".to_string(),
                match status {
                    Some(code) => format!(
                        "{destination} answered {code}{}",
                        if *signed { ", signed" } else { "" }
                    ),
                    None => destination.clone(),
                },
            ),
            What::Baseline { taken_at } => (
                "baseline".to_string(),
                match taken_at.first() {
                    Some((_, at)) => format!("stored on {}", report::stored_on(*at)),
                    None => "nothing stored yet".to_string(),
                },
            ),
        };

        let mark = match checked.verdict {
            Verdict::Ok => "ok  ",
            Verdict::Warned => "note",
            Verdict::Failed => "FAIL",
        };
        // Trimmed, because a check with nothing to say in the detail column
        // would otherwise pad to it and leave the line ending in spaces.
        let mut line = format!("{mark}  {label:<16}{detail}")
            .trim_end()
            .to_string();
        if let Some(problem) = &checked.problem {
            // The sentence is `say`'s, and it is the same one `check --json`
            // carries: two renderings of one answer rather than one rendering
            // and a copy of it.
            let problem = super::say::problem_line(problem);
            // Under the detail column rather than at column zero, so a reason
            // reads as belonging to the line above it. Named rather than
            // written inline: a run of spaces inside a string literal is what
            // the layout guard in `tests/language.rs` looks for, and it is
            // right to — this is the one shape it cannot tell from a typo.
            const UNDER_THE_LABEL: &str = "            ";
            line.push('\n');
            line.push_str(UNDER_THE_LABEL);
            line.push_str(&printable(&problem));
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::wire::check_json;

    /// Not posting is not the same as having nowhere to post.
    ///
    /// With `--no-webhook` no entry was pushed at all, so `check --json` had no
    /// webhook object and the terminal no webhook line -- indistinguishable from
    /// a machine with no `[webhook]` in its file. That is the flag somebody uses
    /// to check everything else without disturbing a receiver, so it is exactly
    /// when the question "is a receiver configured?" is being asked. The address
    /// and every configured header have been through `webhook::check` by then,
    /// so there is a validation that passed to report rather than nothing.
    #[test]
    fn not_posting_is_not_the_same_as_having_nowhere_to_post() {
        use crate::engine::check::{CheckReport, Verdict, What};

        const WHERE_TO: &str = "https://n8n.local/webhook/snob";

        let checked =
            not_posted(Some((WHERE_TO, true)), true).expect("the address was checked and not used");
        assert!(
            matches!(&checked.what, What::Webhook { destination, status: None, signed: true }
                if destination == WHERE_TO),
            "the address it did not post to is the answer: {:?}",
            checked.what
        );
        assert_eq!(checked.verdict, Verdict::Ok);
        assert_eq!(
            checked.problem,
            Some(crate::engine::check::Problem::NotPosted),
            "and the line has to say why nothing was posted"
        );
        assert!(
            super::super::say::problem_line(&crate::engine::check::Problem::NotPosted)
                .contains("--no-webhook"),
            "which is the flag it has to name"
        );

        // A run that is going to post reports what came back instead, and an
        // address nobody configured has nothing to say either way.
        assert!(not_posted(Some((WHERE_TO, true)), false).is_none());
        assert!(not_posted(None, true).is_none());

        // The whole point is that it reaches both readers.
        let report = CheckReport {
            checked: vec![checked],
        };
        assert!(
            describe_check(&report)
                .iter()
                .any(|l| l.contains("webhook")),
            "a probe cannot tell a receiver that was not posted to from no receiver"
        );
        assert_eq!(check_json(&report)["checks"][0]["what"], "webhook");
    }
}
