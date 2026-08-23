//! Fixtures shared by the integration tests.
//!
//! The credentials and the default `ListArgs` were copied into every test binary
//! that needed them, so adding one flag to `ListArgs` — which `--only`,
//! `--max-pages` and `--yes` all did — meant editing files that were each
//! testing something else. They live here once instead.
//!
//! Not every binary uses every item, and a `tests/common/mod.rs` is compiled
//! separately into each one that declares it, so anything one of them does not
//! touch is reported as dead code there. `allow` rather than `expect`: whether
//! anything is in fact unused differs per binary, and `expect` warns about
//! itself in the binaries that happen to use all of it.
#![allow(dead_code)]

use std::sync::Arc;

use snob_cli::app::{App, Viewer};
use snob_cli::cli::{ConsentArgs, FilterArgs, ListArgs, OutputArgs, ProgressArgs, WalkArgs};
use snob_core::Pk;
use snob_core::budget::{RateBudget, UnlimitedRateBudget};
use snob_core::session::{Session, SessionOrigin};
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use snob_store::store::Store;
use url::Url;
use wiremock::MockServer;

/// A plausible desktop Chrome User-Agent. The session is tied to one, and
/// Instagram checks that the two agree.
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

/// A `sessionid` in the shape Instagram issues: the account id, a token and a
/// version, percent-encoded as the cookie carries them.
pub const SID: &str = "42%3AAbCdEfGh%3A20";

/// The arguments a list command gets with no flags given.
///
/// `no_progress` and `yes` are the two that differ from the real defaults, and
/// deliberately: a bar drawing into the test harness is noise, and a test that
/// stops to ask for consent hangs.
pub fn args() -> ListArgs {
    ListArgs {
        target: None,
        filter: FilterArgs::default(),
        output: OutputArgs::default(),
        limit: None,
        walk: WalkArgs {
            progress: ProgressArgs { no_progress: true },
            consent: ConsentArgs { yes: true },
            ..WalkArgs::default()
        },
    }
}

/// The same arguments, aimed at somebody else's account.
pub fn args_for(target: &str) -> ListArgs {
    ListArgs {
        target: Some(target.to_string()),
        ..args()
    }
}

/// The account every fixture signs in as: the id inside [`SID`].
pub const ME: Pk = Pk::new(42);

/// The session the fixtures run as, parsed the way a pasted one is.
pub fn session() -> Session {
    Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap()
}

/// An `App` aimed at the mock server, spending from the given budget.
///
/// This and [`app`] were written out in four test binaries, byte for byte
/// but for the budget expression and how the viewer's pk was spelled --
/// which is exactly the drift the header of this module describes for
/// `args()`.
pub fn app_with(server: &MockServer, db: Store, budget: Arc<dyn RateBudget>) -> App {
    let client = IgClient::new(session(), Pacer::new(budget))
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());
    App::for_test(
        client,
        db,
        Viewer {
            pk: ME,
            username: Some("me".into()),
        },
    )
}

/// [`app_with`] on an unlimited budget: what almost every test wants.
pub fn app(server: &MockServer, db: Store) -> App {
    app_with(server, db, Arc::new(UnlimitedRateBudget))
}

/// The database a test keeps under its own temporary root.
pub fn open_db(root: &std::path::Path) -> Store {
    Store::open_at(&root.join("test.db")).unwrap()
}

/// How many requests the mock server has answered.
pub async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}
