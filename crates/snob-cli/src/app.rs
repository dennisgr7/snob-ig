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
    /// How to name this account to a person: `@someone`, or the id when the
    /// name has not been learned yet.
    pub fn label(&self) -> String {
        match &self.username {
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

    /// A warning, through the progress bar when there is one so it does not
    /// land in the middle of a drawn line.
    pub fn warn(&self, text: &str) {
        self.progress.warn(text);
    }
}
