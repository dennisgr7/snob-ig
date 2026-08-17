//! What every command hangs off.
//!
//! One place assembles the four things the tool is made of — who we are talking
//! as, where the data is kept, what the requests cost, and how the run reports
//! itself — and hands them out already wired together. Commands take an `App`
//! and orchestrate; they never build a client or open a database themselves.
//!
//! The order below is not arbitrary. The progress bar has to exist before the
//! pacer, because the pacer is what announces a wait and the bar is where that
//! announcement goes; and the pacer has to exist before the client, because a
//! client without one cannot be built at all.

use std::sync::Arc;

use anyhow::Result;
use snob_core::Pk;
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;
use snob_core::store::Store;
use snob_core::store::rate_budget::SqliteRateBudget;
use snob_ig::client::IgClient;
use snob_ig::pace::{CancelToken, Pacer};

use crate::engine::target;
use crate::interrupt;
use crate::progress::Progress;

/// The account the run is acting as, when there is one.
#[derive(Debug, Clone)]
pub struct Viewer {
    pub pk: Pk,
    /// Absent until the first run resolves it. Cosmetic: nothing blocks on it.
    pub username: Option<String>,
}

impl Viewer {
    /// The name, filtered for anything that is going to draw it.
    ///
    /// It came from Instagram — `resolve_username`, or the browser handing the
    /// session over — not from the person running the tool, so it is treated
    /// like any other name off the wire. `engine::target::label` filters the
    /// name that was **typed** and used to hand this one straight through,
    /// which had the filtering on the safe half and not on the other one.
    pub fn safe_username(&self) -> Option<String> {
        self.username.as_deref().map(snob_core::model::printable)
    }

    /// How to name this account to a person: `@someone`, or the id when the
    /// name has not been learned yet.
    pub fn label(&self) -> String {
        match self.safe_username() {
            Some(name) => format!("@{name}"),
            None => format!("account {}", self.pk),
        }
    }
}

pub struct App {
    client: IgClient,
    db: Store,
    progress: Progress,
    cancel: CancelToken,
    viewer: Viewer,
    consented: bool,
    /// What was asked for, what it resolved to, and the request count at the
    /// moment it did.
    ///
    /// A crossing calls `engine::list` twice, and each call used to resolve from
    /// scratch: two identical `web_profile_info` requests about the same account
    /// seconds apart. On a session whose username has never been resolved it was
    /// four, because resolving the name and polling the counters are separate
    /// requests there.
    ///
    /// **The question is part of the key, not just the answer.** This used to
    /// hold the target alone, which was indistinguishable from correct while
    /// one `App` meant one account. `snob watch` broke that: it walks several
    /// configured accounts through a single `App`, and the second account was
    /// silently handed the first one's target — never resolved, never walked,
    /// and its changes committed under the first account's marks. Keying on what
    /// was asked makes reuse mean "the same question", which is what the memo
    /// was always for.
    ///
    /// The count is what makes reuse safe in time. Anything that spends a
    /// request may have changed the account — a walk, a retry, a `--refresh` —
    /// so the memo is only good while `Pacer::spent()` has not moved. That is
    /// strictly stronger than asking whether the previous list came from cache,
    /// and it reuses the number AGENTS.md already names as the truth about
    /// requests.
    resolved: Option<Memo>,
}

/// One resolution, and the question it answers.
struct Memo {
    /// The `--target` this was resolved for, exactly as it was given. `None` is
    /// the viewer's own account, which is a different question from any name.
    asked: Option<String>,
    target: target::Target,
    spent: u32,
}

impl App {
    /// Opens everything for a run that acts as the stored account.
    ///
    /// `None` means there is no session stored, which every command turns into
    /// the same message and the same exit code.
    pub fn open(
        secrets: &SecretStore,
        paths: &AppPaths,
        with_progress: bool,
    ) -> Result<Option<Self>> {
        let Some(mut session) = secrets.load()? else {
            return Ok(None);
        };

        // Done here rather than at login because a browser updates long after
        // the session was created, and this is the only moment every command
        // passes through. Failing to store the fresher one is not worth an
        // error: the session still works, and the next run tries again.
        if crate::browser::refresh_user_agent(&mut session)
            && let Err(e) = secrets.save(&session)
        {
            tracing::debug!(error = %e, "could not store the refreshed User-Agent");
        }

        let viewer = Viewer {
            pk: session.ds_user_id,
            username: session.username.clone(),
        };

        // The store goes first: it is what creates the schema, and the budget
        // opens its own connection to a file that has to have tables already.
        let db = Store::open(paths)?;
        let budget = Arc::new(SqliteRateBudget::open(paths)?);

        let progress = Progress::new(with_progress);
        let cancel = interrupt::install();

        let announce = {
            let progress = progress.clone();
            // `waiting` rather than `note`: the number counts down on the bar
            // instead of being frozen into the message at the moment the wait
            // began.
            Arc::new(move |waited: std::time::Duration| {
                progress.waiting("the request budget is rationing", waited);
            })
        };

        let pacer = Pacer::new(budget)
            .with_cancel(cancel.clone())
            .announcing(announce);

        Ok(Some(Self {
            client: IgClient::new(session, pacer)?,
            db,
            progress,
            cancel,
            viewer,
            consented: false,
            resolved: None,
        }))
    }

    /// An app wired by hand, so the engine can be driven against a mock server.
    ///
    /// **Tests only.** It skips the signal handler and the progress bar, which
    /// are the two things a test has no use for and one of which would spawn a
    /// task per test.
    #[doc(hidden)]
    pub fn for_test(client: IgClient, db: Store, viewer: Viewer) -> Self {
        Self {
            client,
            db,
            progress: Progress::new(false),
            cancel: CancelToken::default(),
            viewer,
            consented: false,
            resolved: None,
        }
    }

    pub fn client(&self) -> &IgClient {
        &self.client
    }

    /// The three pieces a walk needs at once: it reads through the client and
    /// writes through the store while both are borrowed. Handing them out
    /// together is what lets the compiler see they are different fields.
    pub fn parts(&mut self) -> (&IgClient, &mut Store, &Progress) {
        (&self.client, &mut self.db, &self.progress)
    }

    pub fn db(&self) -> &Store {
        &self.db
    }

    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    pub fn cancel(&self) -> &CancelToken {
        &self.cancel
    }

    /// The account this run acts as. There is always one: an `App` cannot be
    /// built without a session.
    pub fn viewer(&self) -> &Viewer {
        &self.viewer
    }

    /// Whether the user has already agreed, in this run, to enumerate somebody
    /// else's account.
    ///
    /// It lives here rather than in the arguments because a crossing asks the
    /// engine for two lists and a summary for two more, and being asked the
    /// same question twice about the same account reads as the tool not having
    /// listened. Consent is a property of the run, so it belongs to the thing
    /// that *is* the run.
    ///
    /// Only a real answer sets it. The cooldown path never asks, so it can
    /// never vouch for one.
    pub fn has_consent(&self) -> bool {
        self.consented
    }

    pub fn record_consent(&mut self) {
        self.consented = true;
    }

    /// The target this run already worked out, if anything did.
    ///
    /// Who the account is does not go stale inside one run: the id is stable,
    /// and a rename mid-run would not change which account was meant. So the
    /// identity comes back whatever has happened since.
    ///
    /// **The counters do go stale**, and they are handed back only while
    /// nothing has been spent — which is to say, only while no time has passed
    /// that the account could have moved in. A walk takes minutes. Reusing a
    /// number read before it, to decide a stored list is still current, would
    /// serve a snapshot that missed everything those minutes contained and call
    /// it counter-verified: the exact shape of failure `Provenance` exists to
    /// stop. Cheaper is not worth wrong.
    ///
    /// One consequence worth naming: the private-account refusal in
    /// `target::resolve` runs once per run rather than once per list. That is
    /// fine — the first list already passed it, and the account cannot have
    /// become private in between in a way that matters — but it is a skip, not
    /// an oversight.
    /// What `asked` resolved to earlier in this run, if it was the same
    /// question.
    ///
    /// Compared raw, on the string that was given. Two spellings of one account
    /// simply miss the memo and resolve again, which costs a request; a memo
    /// handed to the wrong account costs correctness, and that is not a trade.
    pub fn resolved_target(&self, asked: Option<&str>) -> Option<target::Target> {
        let memo = self.resolved.as_ref()?;
        if memo.asked.as_deref() != asked {
            return None;
        }
        let mut target = memo.target.clone();
        if memo.spent != self.client.pacer().spent() {
            target.counters = None;
        }
        Some(target)
    }

    /// Remembers what a question resolved to, stamped with what had been spent.
    pub fn remember_target(&mut self, asked: Option<&str>, target: target::Target) {
        self.resolved = Some(Memo {
            asked: asked.map(str::to_string),
            target,
            spent: self.client.pacer().spent(),
        });
    }

    /// Adds the counters a poll just obtained, and re-stamps.
    ///
    /// Re-stamping is the point. The poll is itself a request, so without this
    /// the memo would be invalidated by the very call that completed it, and
    /// the second list of a crossing would resolve and poll all over again.
    /// What must invalidate it is a request spent **after** the counters were
    /// read — a walk, a retry — because that is time in which the account can
    /// have moved.
    pub fn remember_counters(&mut self, counters: target::Counters) {
        let spent = self.client.pacer().spent();
        if let Some(memo) = &mut self.resolved {
            memo.target.counters = Some(counters);
            memo.spent = spent;
        }
    }

    /// A warning, through the progress bar when there is one so it does not
    /// land in the middle of a drawn line.
    pub fn warn(&self, text: &str) {
        self.progress.warn(text);
    }
}

#[cfg(test)]
mod tests {
    use super::Viewer;

    /// The name here came from Instagram — `whoami` writes it out of
    /// `resolve_username` — not from the command line, and it ends up as the
    /// progress bar's prefix, redrawn several times a second. `target::label`
    /// filtered the name the user **typed** and handed this one straight
    /// through, which put the filtering on the safe half and not the other one.
    ///
    /// What is left is the brackets as text, which is `printable`'s contract:
    /// it removes what a terminal obeys, not what it prints. Without the escape
    /// in front of them they are three characters in a name.
    #[test]
    fn a_viewer_label_does_not_carry_what_a_terminal_would_obey() {
        let viewer = Viewer {
            pk: 42,
            username: Some(format!("me{esc}[2K{esc}[A", esc = '\x1b')),
        };

        let label = viewer.label();
        assert!(!label.contains('\x1b'), "{label:?}");
        assert_eq!(label, "@me[2K[A");
    }

    /// Without a name there is nothing to filter and the id stands in.
    #[test]
    fn an_unnamed_viewer_is_still_nameable() {
        let viewer = Viewer {
            pk: 42,
            username: None,
        };

        assert_eq!(viewer.label(), "account 42");
    }
}
