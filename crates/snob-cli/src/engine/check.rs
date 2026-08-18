//! What a configured monitor would find if it ran now.
//!
//! `snob watch setup` asked its questions and wrote a file, and everything that
//! could be wrong with the answers was discovered later, in an unattended run
//! nobody was watching: a session that had gone, an account name with a typo, a
//! webhook whose token the receiver rejects, a schedule the file accepts and the
//! scheduler refuses. Each of those is cheap to find out while somebody is still
//! there, and expensive to find out from a log a week later — if anyone reads
//! it.
//!
//! **This writes nothing and can be repeated.** It takes `&App`, so it cannot
//! reach the `&mut Store` that recording needs — the same guard
//! [`super::watch::from_store`] rests on — and it walks no list. That is not
//! tidiness: it is meant to be usable as a monitoring probe, and a probe that
//! spends a walk every time it is polled is worse than no probe.
//!
//! What it does spend is bounded and named: one request to check the session,
//! and one per configured account for its counters. A `POST` to the user's own
//! webhook is not an Instagram request at all.
//!
//! It returns facts. Which of them is worth a red line, and what the sentence
//! says, is `commands::watch_setup`'s question.

use snob_core::Pk;
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;
use snob_core::store::{snapshots, watch as store};
use snob_core::watch::config::WatchConfig;
use snob_core::watch::schedule::{self, Schedule};

use crate::app::App;

/// How a single check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// It would work.
    Ok,
    /// It would work, but something about it will surprise somebody.
    Warned,
    /// A scheduled run would not do what the configuration says.
    Failed,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warned => "warning",
            Self::Failed => "failed",
        }
    }
}

/// One thing that was checked.
///
/// `problem` carries the underlying error as it was reported, which is data
/// rather than wording: it is whatever the schedule parser, Instagram or the
/// user's own server said, and inventing a sentence for it here would lose the
/// only detail that identifies the cause.
#[derive(Debug, Clone)]
pub struct Checked {
    pub what: What,
    pub verdict: Verdict,
    pub problem: Option<String>,
}

/// What was checked, and what was learned about it.
#[derive(Debug, Clone)]
pub enum What {
    /// No `watch.toml` at all, so there is nothing else to check.
    NotConfigured,
    /// The schedule, and the next few moments it fires at.
    Schedule { next: Vec<i64> },
    /// The session, and which backend the secret store landed on.
    Session {
        viewer: Option<String>,
        backend: &'static str,
    },
    /// One configured account.
    Account {
        /// As the file spells it. `None` is the session's own account.
        target: Option<String>,
        pk: Option<Pk>,
        followers: Option<u64>,
        following: Option<u64>,
        /// Whether an unattended run may read it, which for somebody else's
        /// account means a recorded answer.
        may_run_unattended: bool,
    },
    /// The address a report would be posted to.
    Webhook {
        destination: String,
        status: Option<u16>,
        signed: bool,
    },
    /// Whether there is anything to compare the first scheduled run against.
    Baseline {
        /// When the newest capture of each list was taken. Empty means none.
        taken_at: Vec<(snob_core::model::ListKind, i64)>,
    },
}

/// Everything that was checked, in the order it was checked.
#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    pub checked: Vec<Checked>,
}

impl CheckReport {
    /// The worst verdict in it, which is what the exit code is made of.
    pub fn verdict(&self) -> Verdict {
        self.checked
            .iter()
            .map(|c| c.verdict)
            .max()
            .unwrap_or(Verdict::Ok)
    }

    fn push(&mut self, what: What, verdict: Verdict, problem: Option<String>) {
        self.checked.push(Checked {
            what,
            verdict,
            problem,
        });
    }
}

/// How many upcoming moments to work out, so somebody can recognise their own
/// schedule in them. Three is enough to tell "every Monday" from "every day".
const MOMENTS_SHOWN: usize = 3;

/// Checks the schedule alone, which needs no session and no network.
///
/// Separate because it is the half that still has an answer on a machine with no
/// session at all, and because it is what catches a file the scheduler would
/// refuse at every run — `config::parse` reads TOML and a schema number, not
/// what the values mean.
pub fn schedule_of(schedule: &Schedule, now: i64) -> Checked {
    let mut next = Vec::new();
    let mut at = now;
    for _ in 0..MOMENTS_SHOWN {
        match schedule::next_moment(schedule, Some(at), at, &chrono::Local) {
            Some(moment) if moment > at => {
                next.push(moment);
                at = moment;
            }
            _ => break,
        }
    }

    let verdict = if next.is_empty() {
        Verdict::Failed
    } else {
        Verdict::Ok
    };
    let problem = next
        .is_empty()
        .then(|| "this schedule never fires: no moment it names exists".to_string());

    Checked {
        what: What::Schedule { next },
        verdict,
        problem,
    }
}

/// Everything that can be checked without a session.
pub fn without_a_session(
    configured: Option<&WatchConfig>,
    schedule: Option<&Schedule>,
    now: i64,
) -> CheckReport {
    let mut report = CheckReport::default();

    if configured.is_none() {
        report.push(
            What::NotConfigured,
            Verdict::Warned,
            Some(
                "nothing is configured, so a bare \"snob watch\" has no schedule to run on"
                    .to_string(),
            ),
        );
    }

    if let Some(schedule) = schedule {
        report.checked.push(schedule_of(schedule, now));
    }

    report
}

/// The session, the accounts and the baselines.
///
/// One request for the session and one per account, and nothing is written.
pub async fn with_a_session(
    app: &App,
    secrets: &SecretStore,
    watched: &[super::watch::Watched],
    report: &mut CheckReport,
) {
    // **Nothing is spent during a cooldown**, and this is the one request path
    // in the tool that did not say so. `Pacer::clear_to_send` charges the
    // budget but never reads the `cooldowns` table — every other caller gates
    // explicitly — so a command advertised as safe to poll as often as you like
    // was knocking on a door Instagram had just closed, once per configured
    // account, on whatever interval a monitoring system polls at.
    //
    // Reported rather than skipped in silence: a cooldown is exactly the sort
    // of thing somebody running `check` wants to be told about, and it lifts on
    // its own, so it is a warning rather than a failure.
    match app.client().pacer().cooldown() {
        Ok(Some(until_ms)) => {
            let waiting = |what: What| Checked {
                what,
                verdict: Verdict::Warned,
                problem: Some(format!(
                    "not checked: the account is in cooldown until {}",
                    crate::report::cooldown_ends_at(until_ms)
                )),
            };
            report.checked.push(waiting(What::Session {
                viewer: app.viewer().username.clone(),
                backend: secrets.backend().as_str(),
            }));
            for account in watched {
                report.checked.push(waiting(What::Account {
                    target: account.name().map(str::to_string),
                    pk: None,
                    followers: None,
                    following: None,
                    may_run_unattended: account.may_run_unattended(),
                }));
            }
            return;
        }
        Ok(None) => {}
        // The budget itself is unreadable. That is worth a line, and it is not
        // a reason to go and spend anyway.
        Err(e) => {
            report.checked.push(Checked {
                what: What::Session {
                    viewer: app.viewer().username.clone(),
                    backend: secrets.backend().as_str(),
                },
                verdict: Verdict::Failed,
                problem: Some(e.to_string()),
            });
            return;
        }
    }

    let session = match app.client().validate().await {
        Ok(()) => Checked {
            what: What::Session {
                viewer: app.viewer().username.clone(),
                backend: secrets.backend().as_str(),
            },
            verdict: Verdict::Ok,
            problem: None,
        },
        Err(e) => Checked {
            what: What::Session {
                viewer: app.viewer().username.clone(),
                backend: secrets.backend().as_str(),
            },
            verdict: Verdict::Failed,
            problem: Some(e.to_string()),
        },
    };
    let session_works = session.verdict == Verdict::Ok;
    report.checked.push(session);

    for account in watched {
        let checked = account_of(app, account, session_works).await;
        // The baseline is asked about with the id this check just resolved, so
        // it is the same account the scheduled run would compare.
        let pk = match &checked.what {
            What::Account { pk, .. } => *pk,
            _ => None,
        };
        report.checked.push(checked);
        if let Some(pk) = pk {
            report.checked.push(baseline_of(app, pk));
        }
    }
}

/// One configured account: does it resolve, may an unattended run read it, and
/// what do its counters say.
async fn account_of(app: &App, watched: &super::watch::Watched, ask: bool) -> Checked {
    let target = watched.name().map(str::to_string);
    let may_run_unattended = watched.may_run_unattended();

    let mut what = What::Account {
        target: target.clone(),
        pk: None,
        followers: None,
        following: None,
        may_run_unattended,
    };

    // A session that does not work cannot answer about anybody, and asking
    // would spend a request to learn what the line above already said.
    if !ask {
        return Checked {
            what,
            verdict: Verdict::Warned,
            problem: Some("not checked: the session is not responding".to_string()),
        };
    }

    let name = match &target {
        Some(name) => name.clone(),
        None => match &app.viewer().username {
            Some(name) => name.clone(),
            // The session carries an id but no name yet, and resolving one is
            // what `validate` above has just done for free. Nothing to check.
            None => {
                return Checked {
                    what,
                    verdict: Verdict::Ok,
                    problem: None,
                };
            }
        },
    };

    match app.client().web_profile_info(&name).await {
        Ok(profile) => {
            what = What::Account {
                target,
                pk: Some(profile.id),
                followers: profile.follower_count(),
                following: profile.following_count(),
                may_run_unattended,
            };
            let verdict = if may_run_unattended {
                Verdict::Ok
            } else {
                Verdict::Failed
            };
            let problem = (!may_run_unattended).then(|| {
                "no recorded consent, so an unattended run will refuse to read it".to_string()
            });
            Checked {
                what,
                verdict,
                problem,
            }
        }
        Err(e) => Checked {
            what,
            verdict: Verdict::Failed,
            problem: Some(e.to_string()),
        },
    }
}

/// Posts one message to the address a report would go to.
///
/// The only way to know a webhook works is to use it. A parsed URL says nothing
/// about whether the host resolves, the certificate verifies, the path is
/// registered or the token is the one the receiver wants — and every one of
/// those turns into a queued report and a retry schedule six hours later,
/// discovered from `status` if anybody looks.
///
/// It carries the configured headers and the configured signature, because a
/// preflight that skipped either would be checking a request nobody makes.
/// `event` is `watch.preflight`, so a receiver can branch on it exactly as it
/// branches on the rest, and **nothing is queued**: this is not a report, so
/// there is nothing to retry and nothing to deduplicate.
pub async fn webhook_of(
    client: &crate::watch::webhook::WebhookClient,
    destination: String,
    signed: bool,
    run_id: &str,
    body: &str,
) -> Checked {
    use crate::watch::webhook::Attempt;

    let what = What::Webhook {
        destination,
        status: None,
        signed,
    };

    match client.post(body, "watch.preflight", run_id, 1).await {
        Attempt::Delivered { status } => Checked {
            what: match what {
                What::Webhook {
                    destination,
                    signed,
                    ..
                } => What::Webhook {
                    destination,
                    status: Some(status),
                    signed,
                },
                other => other,
            },
            verdict: Verdict::Ok,
            problem: None,
        },
        other => Checked {
            what,
            verdict: Verdict::Failed,
            problem: Some(format!("{other:?}")),
        },
    }
}

/// Whether there is anything for the first scheduled run to compare against.
///
/// A first run is a `Basis::Baseline`: it reports nothing, by design, and
/// somebody who has just set the monitor up reads that as broken. Saying so
/// here is cheaper than explaining it afterwards.
pub fn baseline_of(app: &App, pk: Pk) -> Checked {
    let mut taken_at = Vec::new();
    for kind in [
        snob_core::model::ListKind::Followers,
        snob_core::model::ListKind::Following,
    ] {
        match snapshots::latest_complete(app.db().conn(), pk, kind) {
            Ok(Some(snapshot)) => taken_at.push((kind, snapshot.taken_at.unwrap_or_default())),
            Ok(None) => {}
            Err(e) => {
                return Checked {
                    what: What::Baseline { taken_at },
                    verdict: Verdict::Warned,
                    problem: Some(e.to_string()),
                };
            }
        }
    }

    let complete = taken_at.len() == 2;
    Checked {
        what: What::Baseline { taken_at },
        verdict: if complete {
            Verdict::Ok
        } else {
            Verdict::Warned
        },
        problem: (!complete).then(|| {
            "the first scheduled run lays the baseline down and reports no changes; \
             the second one onwards reports them"
                .to_string()
        }),
    }
}

/// Whether the monitor has ever run, for the line about what to expect first.
pub fn has_ever_run(paths: &AppPaths) -> bool {
    snob_core::store::Store::open(paths)
        .ok()
        .and_then(|db| store::last_runs(db.conn()).ok())
        .is_some_and(|runs| !runs.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The moments a schedule names, worked out through the evaluator that
    /// actually decides them.
    ///
    /// Not recomputed here in any other way, and that is the point: a preflight
    /// with arithmetic of its own would be checking a schedule nobody runs.
    #[test]
    fn a_schedule_names_the_moments_it_will_fire_at() {
        let schedule = Schedule::every(Duration::from_secs(6 * 3_600)).unwrap();
        let checked = schedule_of(&schedule, 1_700_000_000);

        assert_eq!(checked.verdict, Verdict::Ok);
        let What::Schedule { next } = checked.what else {
            panic!("a schedule check is about a schedule");
        };
        assert_eq!(next.len(), MOMENTS_SHOWN);
        assert!(
            next.windows(2).all(|pair| pair[1] > pair[0]),
            "they have to be in the future and in order: {next:?}"
        );
    }

    /// A calendar that never fires is the one answer worth a red line before
    /// anything is scheduled at all. `0 0 31 2 *` is the standing example:
    /// there is no thirty-first of February.
    #[test]
    fn a_schedule_that_never_fires_is_a_failure_rather_than_a_wait() {
        let schedule = Schedule::cron("0 0 31 2 *").unwrap();
        let checked = schedule_of(&schedule, 1_700_000_000);

        assert_eq!(checked.verdict, Verdict::Failed);
        assert!(checked.problem.is_some());
    }

    /// A machine with no `watch.toml` is not broken, but a bare `snob watch`
    /// there has no schedule to run on, and saying so is the whole job.
    #[test]
    fn nothing_configured_is_reported_rather_than_passed_over() {
        let report = without_a_session(None, None, 1_700_000_000);

        assert_eq!(report.verdict(), Verdict::Warned);
        assert!(matches!(
            report.checked.first().map(|c| &c.what),
            Some(What::NotConfigured)
        ));
    }

    /// The exit code is made of the worst line, so one failure among healthy
    /// ones is still a failure.
    #[test]
    fn the_verdict_is_the_worst_of_them() {
        let mut report = CheckReport::default();
        assert_eq!(
            report.verdict(),
            Verdict::Ok,
            "nothing checked is not a fail"
        );

        report.push(What::NotConfigured, Verdict::Ok, None);
        report.push(What::NotConfigured, Verdict::Failed, None);
        report.push(What::NotConfigured, Verdict::Warned, None);
        assert_eq!(report.verdict(), Verdict::Failed);
    }
}
