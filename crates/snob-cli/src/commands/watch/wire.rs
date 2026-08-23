//! What a report looks like on the wire.
//!
//! The JSON `--json` prints and the body a webhook receives. Its own module
//! because it is **protocol, not presentation**: `payload` builds the exact
//! string that gets signed and stored, so an edit here is an edit to what a
//! receiver deduplicates on and to what a signature covers, which is not the
//! same kind of change as rewording a sentence. `super::say` is where the
//! sentences live.
//!
//! [`SCHEMA`] is the number that says so out loud. Everything a receiver may
//! rely on is in here, in one place, so that "did this change break somebody's
//! integration" is a question about one file.

use snob_core::Epoch;
use snob_core::model::User;
use snob_core::watch::{Basis, ListDiff, Rename};

use crate::engine::watch::{ListReport, Skipped, TickReport, WatchReport, Watched};
use crate::exit::ExitCode;

pub(super) fn check_json(report: &crate::engine::check::CheckReport) -> serde_json::Value {
    use crate::engine::check::What;

    serde_json::json!({
        "verdict": report.verdict().as_str(),
        "checks": report.checked.iter().map(|checked| {
            let (what, detail) = match &checked.what {
                What::NotConfigured => ("config", serde_json::json!(null)),
                What::Schedule { next } => ("schedule", serde_json::json!({ "next": next })),
                What::Session { viewer, backend } => (
                    "session",
                    serde_json::json!({ "viewer": viewer, "storage": backend }),
                ),
                What::Account { target, pk, followers, following, may_run_unattended } => (
                    "account",
                    serde_json::json!({
                        "target": target,
                        "pk": pk,
                        "followers": followers,
                        "following": following,
                        "may_run_unattended": may_run_unattended,
                    }),
                ),
                What::Webhook { destination, status, signed } => (
                    "webhook",
                    serde_json::json!({
                        "destination": destination,
                        "status": status,
                        "signed": signed,
                    }),
                ),
                What::Baseline { taken_at } => (
                    "baseline",
                    serde_json::json!({
                        "lists": taken_at.iter()
                            .map(|(kind, at)| serde_json::json!({ "kind": kind.as_str(), "taken_at": at }))
                            .collect::<Vec<_>>(),
                    }),
                ),
            };
            serde_json::json!({
                "what": what,
                "verdict": checked.verdict.as_str(),
                "detail": detail,
                // The sentence a person reads, so it comes from where the
                // sentences are. It used to be built in `engine::check` and
                // read out here as a string, which made this and the terminal
                // report agree by copying rather than by construction.
                "problem": checked.problem.as_ref().map(super::say::problem_line),
            })
        }).collect::<Vec<_>>(),
    })
}

/// The machine-readable answer.
///
/// Hand-built rather than derived from the report, because this is a contract
/// with whatever is reading it and the struct behind it is not: renaming a
/// field in `ListReport` must not silently rename a key here.
/// The `account` object, shared by `--json` and the webhook body.
///
/// The eight lines were verbatim in both documents, which is the one way the
/// two could come to describe the same account differently. The true value,
/// unfiltered: `printable` is for terminals; a machine format has to carry
/// the name that identifies the account, and `serde_json` escapes what it
/// emits.
fn report_account_json(report: &WatchReport) -> serde_json::Value {
    serde_json::json!({
        "pk": report.account_pk,
        "username": report.username,
        "is_self": report.is_self,
    })
}

/// The `lists` object, shared for the same reason as [`report_account_json`].
fn report_lists_json(report: &WatchReport) -> serde_json::Value {
    serde_json::json!({
        "followers": list_json(report.followers.as_ref()),
        "following": list_json(report.following.as_ref()),
    })
}

pub(super) fn as_json(report: &WatchReport) -> serde_json::Value {
    serde_json::json!({
        "account": report_account_json(report),
        "lists": report_lists_json(report),
        "changes": {
            "followers": diff_json(report.followers.as_ref()),
            "following": diff_json(report.following.as_ref()),
            "renamed": report.renamed.iter().map(rename_json).collect::<Vec<_>>(),
        },
        "counts": {
            "followers_gained": count(report.followers.as_ref(), |d| d.gained.len()),
            "followers_lost":   count(report.followers.as_ref(), |d| d.lost.len()),
            "following_gained": count(report.following.as_ref(), |d| d.gained.len()),
            "following_lost":   count(report.following.as_ref(), |d| d.lost.len()),
            "renamed": report.renamed.len(),
        },
    })
}

/// What `snob watch status --json` prints.
///
/// Here and not in `status`, for the reason the header of this file gives:
/// everything a receiver may rely on is in one place, so "did this change
/// break somebody's integration" is a question about one file. `check` and
/// `diff` went through here and `status` built its document inline, which
/// made the header's claim untrue for the one output a monitoring system is
/// most likely to parse.
pub(super) fn status_json(
    configured: bool,
    config_path: &std::path::Path,
    owed: &snob_store::store::deliveries::Owed,
    health: &super::status::Health,
    last_runs: &[snob_store::store::watch::Run],
    marks: &[snob_store::store::watch::AccountMark],
) -> serde_json::Value {
    serde_json::json!({
        "configured": configured,
        "config_path": config_path.display().to_string(),
        // The whole queue, unchanged, so a probe reading this field still
        // gets what it always did — and beside it the split, because "the
        // next run tries these" and "nothing here can send these" are two
        // facts and this was one number.
        "pending_deliveries": owed.waiting + owed.elsewhere,
        "deliveries": {
            "waiting": owed.waiting,
            "elsewhere": owed.elsewhere,
            // Not owed -- owing has ended. Deliberately outside
            // `pending_deliveries`, which counts work still to do: this is
            // work that will never be done, and a probe that added it to the
            // queue length would report a backlog that no run can shorten.
            "given_up": owed.given_up,
        },
        // The verdict, so a caller reading this does not have to reimplement
        // which combinations of the fields below mean the monitor has stopped
        // doing its job.
        "health": {
            "verdict": health.verdict.as_str(),
            "notes": health.notes,
        },
        // Told apart from the marks below on purpose. A run that could not
        // look moves no mark, so without this a monitor sitting in a cooldown
        // is indistinguishable from one that was killed.
        "last_runs": last_runs.iter().map(|run| serde_json::json!({
            "pk": run.account_pk,
            "at": run.started_at,
            // The token, which is the string this field has always carried —
            // including for a row spelled by a build that is not this one,
            // which is printed back as it was rather than as "unknown".
            "outcome": run.outcome.as_ref().map(|outcome| outcome.as_str()),
            "requests": run.requests,
            "changes": run.changes,
        })).collect::<Vec<_>>(),
        "accounts": marks.iter().map(|m| serde_json::json!({
            "pk": m.account_pk,
            "kind": m.kind.as_str(),
            "last_reported_at": m.compared_at,
            "has_baseline": m.snapshot_id.is_some(),
        })).collect::<Vec<_>>(),
    })
}

/// A tick's answer, which is the report plus what the run itself did.
///
/// `looked` is the field an automation branches on and the one that cannot be
/// derived from the arrays: empty changes mean "nothing happened" when the run
/// looked and "I could not see" when it did not, and something watching for
/// silence reads those as the same thing.
///
/// **`schema` is here for the same reason it is on the wire.** The README's own
/// recipe is `snob watch --json >> events.ndjson`, which makes this file a data
/// feed with readers of its own, and it was the one output of the three with no
/// version on it: a receiver could version-check a webhook body and a preflight
/// and not the file it was told to append to. The number is the same one, and
/// it moves with the same rule, because it describes the same report -- the two
/// differ in what the run says about itself, not in what a change looks like.
pub(super) fn tick_json(tick: &TickReport) -> serde_json::Value {
    let mut out = as_json(&tick.report);
    out["schema"] = serde_json::json!(SCHEMA);
    out["run"] = serde_json::json!({
        "at": tick.at(),
        "looked": tick.looked(),
        "requests": tick.requests,
        "lists": run_lists_json(tick),
    });
    out
}

/// Which lists this run read, and which it refused.
///
/// One builder for both streams. The webhook body used to say nothing about
/// this at all: a list served during a cooldown, after a failed poll or cut
/// short is dropped before the comparison, so it reaches [`payload`] as `None`
/// and serializes to `null` — the same `null` an account with no capture of
/// that list produces, with `counts.following_lost` at `0` either way. A run
/// where the following walk met the truncation wall and a run where nothing
/// happened were byte-identical, so `{{ $json.counts.following_lost > 0 }}`
/// routed to "quiet" for as long as the wall lasted. The refusal was said out
/// loud on standard error, where no receiver hears it.
pub(super) fn run_lists_json(tick: &TickReport) -> serde_json::Value {
    tick.lists
        .iter()
        .map(|l| {
            serde_json::json!({
                "kind": l.kind.as_str(),
                "skipped": l.skipped.map(skipped_token),
            })
        })
        .collect::<Vec<_>>()
        .into()
}

/// The line a tick that failed leaves in the stream.
///
/// Written after the `?` in `tick_one`, the JSON line was not written at all: a
/// tick that could not resolve an account, or met a mid-walk cooldown, or could
/// not read `history_head`, put its whole account on standard error as an
/// English paragraph and left the file with no line for that interval. On a
/// recipe the README offers as a complete way to use the tool.
///
/// It carries the same `run` object a successful line carries, so one reader
/// can take `run.at` off every line without asking which kind it is, and
/// `error` is what tells the two apart. `error.code` is the vocabulary of the
/// README's exit table and of `watch_runs.outcome`, which is the field worth
/// branching on; `error.message` is the chain, for a person reading the file.
///
/// The name and the message are the true values, unfiltered, for the reason
/// `as_json` gives about a username: `printable` is for terminals, a machine
/// format has to carry what identifies the account, and `serde_json` escapes
/// what it emits. Everything drawn at a person still goes through
/// `report::print_error`.
pub(super) fn failed_tick_json(
    watched: &Watched,
    error: &anyhow::Error,
    at: Epoch,
    requests: u32,
) -> serde_json::Value {
    serde_json::json!({
        // The failure line carries it too. README says every message this tool
        // emits carries `schema`, "including every line of the `--json`
        // stream", and this was the one line without it — so a reader that
        // branches on `msg["schema"] == 1`, which is what the version is for,
        // threw on exactly the ticks it most needed to handle.
        "schema": SCHEMA,
        "account": {
            "username": watched.name(),
            "is_self": watched.name().is_none(),
        },
        "run": {
            "at": at,
            "looked": false,
            "requests": requests,
            "lists": [],
        },
        "error": {
            "code": ExitCode::from_chain(error).unwrap_or(ExitCode::Error).as_str(),
            "message": format!("{error:#}"),
        },
    })
}

/// One JSON line. One object, one line, in both modes.
///
/// `once` used to lay its object out to be read, on the grounds that somebody
/// is looking at it — and that holds right up to the moment `watch.toml`
/// carries a second `[[account]]`, which is what `snob watch setup` writes as
/// soon as anybody answers yes to watching somebody else. `run_accounts` prints
/// one object per account with nothing wrapping them, so two pretty-printed
/// objects came out back to back: `json.load` stops at `Extra data`, and
/// PowerShell's `ConvertFrom-Json` refuses it outright. The bytes were in no
/// documented format at all — neither one document nor NDJSON — for the mode
/// whose own help says it is meant for cron.
///
/// So the layout is not a mode's decision any more. `--json` is a stream of
/// lines, which is what the CHANGELOG already promised and what
/// `snob watch once --json >> events.ndjson` has to mean. What the two modes
/// still differ on is whether a tick with no news is printed at all, and that
/// is [`Printing::watching`], where it belongs.
///
/// The fallback is `Value::to_string` rather than a `?` because one call site
/// is reached from the arm already handling a failure: a serializer error on a
/// `Value` built here is not a second failure worth returning instead of the
/// first.
pub(super) fn json_line(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

/// The version of the shape every message this tool emits has.
///
/// One number, in one place, because there are three emitters and they were
/// three literals: the webhook body, the preflight, and the `--json` stream.
/// Two spellings of one condition is how a receiver comes to be told a report
/// is schema 1 and a preflight schema 2 for the same release, and this file
/// already carries that lesson about `event`.
///
/// It moves when a field is removed or its meaning changes, and not when one is
/// added: additive is what lets a receiver keep working, and `run.lists`
/// arriving beside `counts` did not move it.
pub(super) const SCHEMA: u32 = 1;

/// What goes on the wire.
///
/// A contract with whatever is on the other end, so it is built here by hand
/// and asserted in a test: this is the one output of the tool that a stranger's
/// automation branches on, and a field renamed by accident breaks a workflow
/// somebody built months ago.
///
/// Four decisions worth knowing about, all of them about what an n8n node
/// actually needs:
///
/// - **`counts` is separate from `changes`**, and redundant with the array
///   lengths on purpose. `{{ $json.counts.followers_lost > 0 }}` is the
///   condition people write, and it is far less fragile than an expression over
///   `.length` on a field that may be absent.
/// - **`schema` and `event` are at the top**, so fields can be added later
///   without breaking anybody and a Switch node can tell a heartbeat from a
///   report without looking inside.
/// - **`looked` is not derivable from the arrays.** Empty changes mean "nothing
///   happened" when the run could see and "I could not look" when it could not,
///   and something watching for silence reads those as the same thing.
/// - **`run.lists` is `looked` at the granularity a receiver needs.** `looked`
///   is true when *either* list was read, so a run that walked followers and
///   was refused following says `true` while `lists.following` is `null` and
///   `counts.following_lost` is `0` — indistinguishable from a quiet run, and
///   from an account with no following capture at all. The token is per list
///   and additive, so `schema` stays 1. `lists.<kind>` deliberately stays
///   `null` rather than becoming an object: a receiver testing it against
///   `null` is the shape this shipped with, and there is no version to warn
///   them by.
pub(super) fn payload(tick: &TickReport, run_id: &str, event: &str) -> serde_json::Value {
    let report = &tick.report;
    let changes = report.changes();

    serde_json::json!({
        "schema": SCHEMA,
        "event": event,
        "run": {
            "id": run_id,
            // The tick's own moment, not a third reading of the clock. It is
            // what `commit_report` files the mark at and what the `--json` line
            // carries, so one event has one time in all three places.
            "at": tick.at(),
            "looked": tick.looked(),
            "requests": tick.requests,
            "lists": run_lists_json(tick),
            "tool": { "name": "snob", "version": env!("CARGO_PKG_VERSION") },
        },
        "account": report_account_json(report),
        "lists": report_lists_json(report),
        "counts": {
            "followers_gained": changes.followers.gained.len(),
            "followers_lost":   changes.followers.lost.len(),
            "following_gained": changes.following.gained.len(),
            "following_lost":   changes.following.lost.len(),
            "renamed": changes.renamed.len(),
            "total": changes.len(),
        },
        "events": {
            // The accounts are the same `User` that `snob followers --format
            // json` already emits, plus the address: whoever receives this is
            // usually about to put it in a message, and rebuilding the URL at
            // the other end is exactly where somebody pastes a name without
            // encoding it.
            "followers_gained": changes.followers.gained.iter().map(account_json).collect::<Vec<_>>(),
            "followers_lost":   changes.followers.lost.iter().map(account_json).collect::<Vec<_>>(),
            "following_gained": changes.following.gained.iter().map(account_json).collect::<Vec<_>>(),
            "following_lost":   changes.following.lost.iter().map(account_json).collect::<Vec<_>>(),
            "renamed": changes.renamed.iter().map(rename_json).collect::<Vec<_>>(),
        },
    })
}

/// What `snob watch check` posts.
///
/// The same skeleton [`payload`] uses, and it did not used to be. Three events
/// exist; two go through `payload` and this one did not, so the preflight was
/// the only message with no `schema` — the one a receiver cannot version-check
/// — and it carried the id and the moment at the top level while every other
/// message carries them under `run`. `webhook_of`'s own doc claimed "a receiver
/// can branch on it exactly as it branches on the rest".
///
/// **Breaking**, for anybody reading `$json.run_id` or `$json.at` on a
/// preflight, and bounded: a preflight is never queued, so no stored body has
/// the old shape and there is nothing to migrate. `schema` stays 1 rather than
/// becoming 2 — the preflight is joining the family, and bumping it would make
/// every report receiver re-check a version over a message that did not change.
///
/// `looked`, `requests` and `lists` are deliberately absent: this run looked at
/// nothing and spent nothing on the account, and a `false` there would read as
/// a report that could not see rather than as a message that is not a report.
/// The `note` says so in words, for whoever opens one by hand.
pub(super) fn preflight_body(run_id: &str, at: Epoch) -> serde_json::Value {
    serde_json::json!({
        "schema": SCHEMA,
        "event": crate::engine::check::PREFLIGHT_EVENT,
        "run": {
            "id": run_id,
            "at": at,
            "tool": { "name": "snob", "version": env!("CARGO_PKG_VERSION") },
        },
        "note": "snob watch check: this is not a report, and nothing is queued",
    })
}

/// One account, as an automation wants it.
pub(super) fn account_json(user: &User) -> serde_json::Value {
    let mut value = serde_json::to_value(user).unwrap_or(serde_json::Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "profile_url".to_string(),
            serde_json::Value::String(user.profile_url()),
        );
    }
    value
}

/// The stable name of why a list was left out.
pub(super) fn skipped_token(skipped: Skipped) -> &'static str {
    match skipped {
        Skipped::NobodyLooked(_) => "not_verified",
        Skipped::Incomplete(..) => "incomplete",
    }
}

pub(super) fn count(report: Option<&ListReport>, of: impl Fn(&ListDiff) -> usize) -> usize {
    report.map(|r| of(&r.diff)).unwrap_or_default()
}

pub(super) fn list_json(report: Option<&ListReport>) -> serde_json::Value {
    let Some(report) = report else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        // A stable token, so a caller can tell "nothing changed" from "this is
        // the first look" without reading a sentence.
        "basis": basis_token(report.basis),
        "since": report.since,
        "until": report.until,
        "count": report.total,
    })
}

pub(super) fn diff_json(report: Option<&ListReport>) -> serde_json::Value {
    let Some(report) = report else {
        return serde_json::json!({ "gained": [], "lost": [] });
    };
    serde_json::json!({
        "gained": report.diff.gained,
        "lost": report.diff.lost,
    })
}

pub(super) fn rename_json(rename: &Rename) -> serde_json::Value {
    serde_json::json!({
        "pk": rename.pk,
        "from": rename.from,
        "to": rename.to,
        "changed_at": rename.at,
    })
}

/// The stable name of what a list's report is. Written out here rather than on
/// [`Basis`] because it is vocabulary aimed at a caller, and the domain does
/// not decide how it is spelled.
pub(super) fn basis_token(basis: Basis) -> &'static str {
    match basis {
        Basis::Baseline { .. } => "baseline",
        Basis::Unchanged { .. } => "unchanged",
        Basis::Compare { .. } => "compared",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::fixtures::{list, report_with, user};
    use crate::engine::Provenance;
    use crate::engine::watch::TickList;
    use snob_core::Pk;
    use snob_core::model::ListKind;
    use snob_core::watch::{Basis, ListDiff, Rename};

    /// The tokens are what a caller branches on, so they are asserted rather
    /// than left to whatever the enum happens to be called.
    #[test]
    fn the_json_carries_stable_tokens_for_each_basis() {
        for (basis, token) in [
            (Basis::Baseline { snapshot_id: 1 }, "baseline"),
            (Basis::Unchanged { snapshot_id: 1 }, "unchanged"),
            (
                Basis::Compare {
                    before: 1,
                    after: 2,
                },
                "compared",
            ),
        ] {
            assert_eq!(basis_token(basis), token);
        }
    }

    /// The README publishes the body, so it has to be the body.
    ///
    /// The example was the **stdout** shape minus its lists: `run` carried
    /// `looked` and `requests` and neither `id` nor `at`. Those are the two
    /// fields the surrounding prose depends on — the id is what "queued and
    /// retried", at-least-once and `X-Snob-Delivery` are all about, and the
    /// moment is the only timestamp in the message — so a receiver written from
    /// the document had neither. `counts` showed three of six keys, and nothing
    /// marked the object as abbreviated.
    ///
    /// It compares keys and not values, because the block is an example and
    /// abbreviates the arrays. What it may not do is name a key the payload does
    /// not emit, or leave one out of `run` — which is the object the drift
    /// happened in.
    ///
    /// The version inside `tool` is deliberately not asserted: doing that makes
    /// every release a README edit.
    #[test]
    fn the_readme_publishes_the_body_that_goes_out() {
        // Walks up from this crate until a `Cargo.lock` shows up, the way the
        // source-reading guards in `snob-core` do, and answers nothing from a
        // packaged build where there is no repository to read.
        let Some(root) = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|d| d.join("Cargo.lock").is_file())
        else {
            return;
        };

        let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
        assert_eq!(
            readme.matches("```json").count(),
            1,
            "the payload is the only JSON block, and this test takes the first"
        );
        let block = readme
            .split("```json")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("the README shows the payload");
        let published: serde_json::Value =
            serde_json::from_str(block).expect("the README's example has to be JSON");

        let real = payload(
            &TickReport::for_test(report_with(None, vec![]), 14, Epoch::new(1_700_000_000)),
            "run-1",
            "watch.changes",
        );

        let keys = |value: &serde_json::Value| {
            let mut names: Vec<String> = value
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();
            names
        };
        assert_eq!(
            keys(&published["run"]),
            keys(&real["run"]),
            "the `run` object is published in full or not at all"
        );

        // One direction only: the example abbreviates, and it is allowed to.
        // What it may not do is publish a field nothing sends.
        fn only_real_keys(published: &serde_json::Value, real: &serde_json::Value, path: &str) {
            let (Some(published), Some(real)) = (published.as_object(), real.as_object()) else {
                return;
            };
            for (key, value) in published {
                let here = format!("{path}.{key}");
                let counterpart = real.get(key).unwrap_or_else(|| {
                    panic!("the README publishes \"{here}\", which is not sent")
                });
                only_real_keys(value, counterpart, &here);
            }
        }
        only_real_keys(&published, &real, "");
    }

    /// Every message this tool emits can be version-checked, and the two that
    /// go to a receiver put the id and the moment in the same place.
    ///
    /// Three emitters exist and they were three literal `1`s and, for a while,
    /// two shapes. The preflight was the only body with no `schema` at all —
    /// the one message a receiver cannot version-check — with `run_id` and `at`
    /// at the top level while the other two carried them under `run`;
    /// `webhook_of`'s doc claimed the opposite in as many words.
    ///
    /// The `--json` stream was the third, and it had no version either. The
    /// README's own recipe is `snob watch --json >> events.ndjson`, which makes
    /// that file a data feed with readers of its own, so a receiver could
    /// version-check a webhook body and a preflight and not the file it was told
    /// to append to. It carries no `run.id`, and that is right rather than an
    /// omission: an id exists to deduplicate an at-least-once delivery, and a
    /// line written once to a local file is not one.
    #[test]
    fn every_message_carries_the_schema() {
        let report = payload(
            &TickReport::for_test(report_with(None, vec![]), 0, Epoch::new(1_700_000_000)),
            "run-1",
            "watch.changes",
        );
        let preflight = preflight_body("run-2", Epoch::new(1_700_000_000));
        let streamed = tick_json(&TickReport::for_test(
            report_with(None, vec![]),
            0,
            Epoch::new(1_700_000_000),
        ));

        for (which, message) in [
            ("report", &report),
            ("preflight", &preflight),
            ("stream line", &streamed),
        ] {
            assert_eq!(message["schema"], 1, "{which} cannot be version-checked");
            assert_eq!(
                message["run"]["at"], 1_700_000_000,
                "{which} does not say when"
            );
            assert!(
                message.get("at").is_none(),
                "{which} still has the moment at the top level"
            );
        }

        for (which, message) in [("report", &report), ("preflight", &preflight)] {
            assert!(
                message["event"].as_str().is_some(),
                "{which} has nothing for a Switch node to read"
            );
            assert!(
                message["run"]["id"].as_str().is_some(),
                "{which} does not say which run it is"
            );
            assert!(
                message.get("run_id").is_none(),
                "{which} still has the id at the top level"
            );
        }

        // And the name is one constant, so the header and the body cannot come
        // to disagree the way they did over heartbeats.
        assert_eq!(preflight["event"], crate::engine::check::PREFLIGHT_EVENT);
    }

    /// A refused list is not a quiet one, and the body has to say which it was.
    ///
    /// `tick` drops a list it could not verify before the comparison, so it
    /// reaches `payload` as `None` and `list_json` turns it into `null` — the
    /// same `null` an account with no capture of that list produces, with
    /// `counts.following_lost` at `0` in both. The two bodies were byte
    /// identical, so `{{ $json.counts.following_lost > 0 }}` routed a run that
    /// could not see to "nothing happened" for as long as the wall lasted, and
    /// the only place the refusal was said out loud was standard error.
    ///
    /// `run.looked` cannot resolve it, and it is asserted equal here to say so:
    /// it is `any`, not `all`, so a run that read followers and was refused
    /// following reports `true`. Which list is the question.
    #[test]
    fn a_refused_list_is_not_reported_as_a_quiet_one() {
        let body = |skipped| {
            let mut tick = TickReport::for_test(
                report_with(
                    Some(list(
                        Basis::Compare {
                            before: 1,
                            after: 2,
                        },
                        ListDiff {
                            gained: vec![user(Pk::new(1), "arrived")],
                            lost: vec![],
                        },
                        Some(Epoch::new(1_000)),
                    )),
                    vec![],
                ),
                7,
                Epoch::new(1_700_000_000),
            );
            tick.lists = vec![
                TickList {
                    kind: ListKind::Followers,
                    skipped: None,
                },
                TickList {
                    kind: ListKind::Following,
                    skipped,
                },
            ];
            payload(&tick, "run-1", "watch.changes")
        };

        let refused = body(Some(Skipped::NobodyLooked(Provenance::PollFailed)));
        let read = body(None);

        // Everything a receiver had to go on before, and it is the same in
        // both: the arrays cannot tell them apart and neither can the counts.
        assert_eq!(refused["lists"], read["lists"]);
        assert_eq!(refused["counts"], read["counts"]);
        assert_eq!(
            refused["run"]["looked"], read["run"]["looked"],
            "`looked` is `any`, so it says `true` for both"
        );

        assert_ne!(
            refused["run"]["lists"], read["run"]["lists"],
            "a receiver has no field to read the refusal from"
        );
        assert_eq!(refused["run"]["lists"][1]["kind"], "following");
        assert_eq!(refused["run"]["lists"][1]["skipped"], "not_verified");
        assert_eq!(
            read["run"]["lists"][1]["skipped"],
            serde_json::Value::Null,
            "a list that was read carries no refusal"
        );
    }

    /// An event line says when it happened.
    ///
    /// The README puts `snob watch --json >> events.ndjson` forward as a
    /// complete way to use the tool, and the line carried no time of its own.
    /// The only epoch fields belonged to the lists — the mark's moment and the
    /// capture's — and they go away with the list when it is refused. One 429
    /// opens a cooldown, both lists are served from storage for the next half
    /// hour, and every line in that window is byte-identical while the wire
    /// bodies for the same ticks differ at `run.at`. A file like that cannot be
    /// queried by time, windowed or deduplicated.
    #[test]
    fn an_event_line_says_when_it_happened() {
        let quiet_run_at = |at| {
            let mut tick = TickReport::for_test(report_with(None, vec![]), 0, at);
            tick.lists = vec![TickList {
                kind: ListKind::Followers,
                skipped: Some(Skipped::NobodyLooked(Provenance::Cooldown)),
            }];
            tick_json(&tick)
        };

        let first = quiet_run_at(Epoch::new(1_700_000_000));
        let second = quiet_run_at(Epoch::new(1_700_021_600));

        assert_eq!(first["run"]["at"], 1_700_000_000);
        assert_ne!(
            first, second,
            "six hours apart and the same bytes: nothing in the file can date a run"
        );

        // The moment is the tick's own, so the file and whatever the webhook
        // delivered can be joined on it.
        let tick = TickReport::for_test(report_with(None, vec![]), 0, Epoch::new(1_700_000_000));
        assert_eq!(
            tick_json(&tick)["run"]["at"],
            payload(&tick, "run-1", "watch.changes")["run"]["at"],
            "one event, one moment"
        );
    }

    /// A failed tick leaves a line in the stream.
    ///
    /// The JSON line is written after the `?` in `tick_one`, so a tick that
    /// failed printed nothing on standard output at all — @friend goes private,
    /// or DNS goes away for a named target, and `events.ndjson` simply has no
    /// line for that interval. The failure went to standard error as an English
    /// `error:` / `caused by:` / `hint:` paragraph, which is the one shape a
    /// consumer of the file is not reading.
    ///
    /// The line has to be readable by the same reader the successful lines
    /// have, which is why `run` is the same object. `error` is what tells them
    /// apart, and a successful line must not have one.
    #[test]
    fn a_failed_tick_leaves_a_line_in_the_stream() {
        let error = anyhow::anyhow!("@friend's account is private");
        let line = failed_tick_json(
            &Watched::consented("friend".into(), crate::engine::watch::Consent),
            &error,
            Epoch::new(1_700_000_000),
            1,
        );

        assert_eq!(
            line["run"]["at"], 1_700_000_000,
            "the gap in the file has to be datable, which is the whole of it"
        );
        assert_eq!(line["run"]["looked"], false);
        assert_eq!(line["run"]["requests"], 1, "the poll was charged");
        assert_eq!(line["account"]["username"], "friend");
        assert_eq!(
            line["error"]["code"], "error",
            "the vocabulary of the exit table, not free text"
        );

        // And the two kinds of line are told apart by the field itself, not by
        // what is missing from the rest of the object.
        let ok = tick_json(&TickReport::for_test(
            report_with(None, vec![]),
            0,
            Epoch::new(1_700_000_000),
        ));
        assert!(
            ok.get("error").is_none(),
            "a run that worked must not look like one that failed"
        );

        // One object, one line, in both modes. `run_accounts` prints one per
        // account with nothing wrapping them, so a line laid out over several
        // of them stops parsing the moment a second account is watched.
        assert!(!json_line(&line).contains('\n'));
        assert!(
            !json_line(&ok).contains('\n'),
            "the successful line is a line too"
        );
    }

    /// The shape a stranger's automation branches on, pinned to a literal.
    ///
    /// This is the one output of the tool that somebody else's workflow reads,
    /// and a key renamed by accident breaks something built months ago with no
    /// error anywhere. Comparing against a literal means a change to the
    /// contract has to be a change somebody made on purpose.
    ///
    /// Both of the fields that move in production are arguments here: `run.id`
    /// is random and `run.at` is the tick's own moment, so the literal can
    /// carry them rather than the comparison having to skip them.
    #[test]
    fn the_payload_has_the_shape_a_receiver_was_promised() {
        let tick = TickReport::for_test(
            report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff {
                        gained: vec![user(Pk::new(1), "arrived")],
                        lost: vec![user(Pk::new(2), "left")],
                    },
                    Some(Epoch::new(1_000)),
                )),
                vec![Rename {
                    pk: Pk::new(7),
                    history_id: 7,
                    from: "before".into(),
                    to: "after".into(),
                    at: Epoch::new(1_500),
                }],
            ),
            14,
            Epoch::new(1_700_000_000),
        );

        let payload = payload(&tick, "run-1", "watch.changes");

        assert_eq!(
            payload,
            serde_json::json!({
                "schema": 1,
                "event": "watch.changes",
                "run": {
                    "id": "run-1",
                    "at": 1_700_000_000,
                    "looked": false,
                    "requests": 14,
                    "lists": [],
                    "tool": { "name": "snob", "version": env!("CARGO_PKG_VERSION") },
                },
                "account": { "pk": 42, "username": "me", "is_self": true },
                "lists": {
                    "followers": {
                        "basis": "compared",
                        "since": 1_000,
                        "until": 2_000,
                        "count": 10,
                    },
                    "following": null,
                },
                "counts": {
                    "followers_gained": 1,
                    "followers_lost": 1,
                    "following_gained": 0,
                    "following_lost": 0,
                    "renamed": 1,
                    "total": 3,
                },
                "events": {
                    "followers_gained": [{
                        "pk": 1,
                        "username": "arrived",
                        "profile_url": "https://www.instagram.com/arrived/",
                    }],
                    "followers_lost": [{
                        "pk": 2,
                        "username": "left",
                        "profile_url": "https://www.instagram.com/left/",
                    }],
                    "following_gained": [],
                    "following_lost": [],
                    "renamed": [{
                        "pk": 7,
                        "from": "before",
                        "to": "after",
                        "changed_at": 1_500,
                    }],
                },
            })
        );
    }
}
