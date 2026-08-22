//! `snob watch once`: one run of the monitor, by hand or on a timer.

use anyhow::Result;
use snob_store::config;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::WatchOnceArgs;
use crate::commands::common;
use crate::exit::ExitCode;
use crate::report;
use crate::ui;

use super::delivery::delivery_from;
use super::run::{Printing, run_accounts};
use super::say::say_what_was_given_up;
use super::watched::watched_from;

/// One run of the monitor: look, report, and remember having reported.
///
/// **The scheduled run without the loop**, which is what the README's "one run,
/// for cron or a systemd timer" describes and what `once` already was for the
/// webhook half. It was not for the accounts: it read `watch.toml` for the
/// address and then built the watched set from the command line alone, so
/// somebody who ran `watch setup`, added @friend and put this on a timer never
/// had @friend walked, marked or reported — and typing the name instead failed
/// every unattended run, because `Watched::asking` discards the recorded
/// consent that is the only thing such a run accepts.
pub(super) async fn once(
    args: WatchOnceArgs,
    secrets: SecretStore,
    paths: &AppPaths,
) -> Result<ExitCode> {
    // Settled before anything can refuse. This mode had the settle below and the
    // scheduled one had none, and the two doors above them closed first in both:
    // a session that has gone leaves owed reports aging past `MAX_AGE_SECS`,
    // where `due` no longer returns them and `failed` — the only thing that
    // expires one — is never reached, while `status` goes on promising the next
    // run will try them. A webhook address `webhook::check` refuses does the
    // same thing one line earlier. One call, above both, so the pair cannot
    // drift a third time.
    say_what_was_given_up(crate::engine::watch::settle_without_a_session(
        paths,
        snob_core::clock::now(),
    ));

    // Before the session is opened and long before a request is spent, so a
    // webhook address that could never work costs nothing to find out about.
    // The file is read here too: `once` on a timer should need no more
    // arguments than the scheduled mode does.
    let configured = config::load(paths)?;
    let delivery = delivery_from(&args.delivery, configured.as_ref(), &secrets)?;

    // Already settled, at the top: this run is the one that most needs it,
    // and it is not the only door that closes before `run_accounts`.
    let mut app = common::app(&secrets, paths, !args.no_progress)?;
    // The monitor takes an answer in advance from `watch.toml`, never from a
    // flag, so the refusal when nobody is at a terminal has to say so.
    app.consent_comes_from_the_config();

    let watched = watched_from(args.target.clone(), configured.as_ref());

    let outcome = run_accounts(
        &mut app,
        &watched,
        delivery.as_ref(),
        Printing::watched(args.json),
    )
    .await;

    // Said whether or not an account failed: a run that stopped halfway still
    // spent requests, and this is the mode somebody is watching.
    ui::info(&format!(
        "{} - {}",
        report::stored_on(snob_core::clock::now()),
        report::requests(outcome.spent)
    ));

    if let Some(e) = outcome.failed {
        return Err(e);
    }

    // The run's own verdict, which was computed, written into `watch_runs`, and
    // then thrown away in favor of a literal zero. The exit codes exist so a
    // caller on a timer can tell "wait" from "log in again" without parsing
    // text, and `once` is the mode that is put on a timer.
    Ok(outcome.code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::fixtures::owed_long_ago;
    use snob_store::config::WatchConfig;
    use snob_store::store::deliveries;

    /// And neither does a webhook the run refuses.
    ///
    /// `delivery_from` ends in `webhook::check`, and both modes call it before
    /// anything is opened -- deliberately, so an address that could never work
    /// costs nothing to find out about. The cost was that a refused address took
    /// retention with it. A header this tool already sends is a natural thing to
    /// write into `[webhook.headers]`, since every header it sends starts
    /// `X-Snob-`, and it stopped every run of both modes before either settle
    /// site -- with `status` still saying the next run would try what was
    /// queued.
    #[tokio::test]
    async fn a_webhook_the_run_refuses_still_lets_the_queue_settle() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
        let now = snob_core::clock::now();
        let id = owed_long_ago(&paths, now);

        let secrets = snob_store::secrets::SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-refused-{}", std::process::id()));
        let args = WatchOnceArgs {
            delivery: crate::cli::WebhookArgs {
                webhook: Some("https://receiver.example/hook".to_string()),
                header: vec!["X-Snob-Source: homelab".to_string()],
                sign_with: None,
                heartbeat: false,
            },
            target: None,
            json: false,
            no_progress: true,
        };

        let refused = once(args, secrets, &paths).await;
        assert!(
            refused.is_err(),
            "that header is part of what snob sends, so the address is refused"
        );

        let db = snob_store::store::Store::open(&paths).unwrap();
        assert_eq!(
            deliveries::state(db.conn(), id).unwrap().as_deref(),
            Some("expired"),
            "the address was refused, and the database still has to be tidied"
        );
    }

    /// `snob watch once` watches what the file says to watch.
    ///
    /// It read `watch.toml` for the webhook address and then built the watched
    /// set from the command line alone, so somebody who ran `watch setup`, added
    /// @friend and followed the README's "one run, for cron or a systemd timer"
    /// never had @friend walked, marked or reported. Typing the name instead was
    /// no answer either: `Watched::asking` carries no consent, and an unattended
    /// run accepts only a recorded one.
    ///
    /// Both modes go through `watched_from` now, so there is one answer to
    /// "which accounts" rather than two that disagree.
    ///
    /// What this pins is that answer. That `once` asks for it rather than
    /// building its own is not reachable from here — it would take an
    /// integration test that opens a session and a keyring to drive the command
    /// — so it is said in the doc-comment on `once` instead, where somebody
    /// changing it will read it.
    #[test]
    fn once_watches_the_accounts_the_file_lists() {
        let file = WatchConfig {
            schema: 1,
            every: Some(std::time::Duration::from_secs(6 * 3600)),
            at: vec![],
            on: vec![],
            cron: None,
            jitter: None,
            webhook: None,
            accounts: vec![
                snob_store::config::AccountConfig {
                    target: "self".to_string(),
                    consent: None,
                },
                snob_store::config::AccountConfig {
                    target: "friend".to_string(),
                    consent: Some(snob_store::config::ConsentConfig {
                        agreed_at: snob_core::Epoch::new(1_700_000_000),
                    }),
                },
            ],
        };

        let watched = watched_from(None, Some(&file));
        let names: Vec<Option<&str>> = watched.iter().map(|w| w.name()).collect();
        assert_eq!(
            names,
            vec![None, Some("friend")],
            "the file's own account and the one it lists"
        );
        assert!(
            watched.iter().all(|w| w.may_run_unattended()),
            "the recorded consent is what makes the second one legal on a timer"
        );

        // And a name typed on the command line still picks up the answer on
        // record, rather than discarding it and failing every unattended run.
        let typed = watched_from(Some("friend".to_string()), Some(&file));
        assert_eq!(typed.len(), 1);
        assert!(typed[0].may_run_unattended());
    }
}
