//! Asking before enumerating somebody else's account, and what happens when
//! there is nobody to ask.
//!
//! Two properties, and they pull in opposite directions. Nothing may be spent
//! on a run the user never authorized — so the refusal has to arrive before the
//! account is even resolved, which is already a request. And a run whose
//! *output* is redirected is not a run with nobody at it: `snob scan someone |
//! jq` is a shape the README promises, and the question it needs is asked on
//! standard error and answered on standard input, neither of which a pipe on
//! standard output touches.
//!
//! Whether a stream is a terminal is a property of the process, and `cargo test`
//! answers it differently depending on where the suite was started from, so the
//! answer is passed in rather than asked for. That is what
//! `engine::ask_consent_with` exists for.

use snob_core::Pk;
use snob_core::session::{Session, SessionOrigin};
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use snob_store::store::Store;
use url::Url;
use wiremock::MockServer;

use snob_cli::app::{App, Viewer};
use snob_cli::cli::ListArgs;
use snob_cli::engine;
use snob_cli::exit::ExitCode;

mod common;
use common::{SID, UA, args_for};

/// Someone else's account, and no `-y`: the case that needs a question.
///
/// `yes` is the one field that has to differ from the shared fixture — the tests
/// there would hang on the prompt, and these exist to reach it.
fn args(target: &str) -> ListArgs {
    ListArgs {
        yes: false,
        ..args_for(target)
    }
}

/// No mock is mounted on purpose. Every test here asserts the answer arrives
/// without a request, so a request would be a connection refused rather than a
/// quietly served page.
async fn ask(args: &ListArgs, someone_is_there: bool) -> (anyhow::Result<()>, usize) {
    ask_as(args, someone_is_there, false).await
}

/// The same question from the monitor, which takes its answer from the file
/// rather than from a flag.
async fn ask_as_the_monitor(
    args: &ListArgs,
    someone_is_there: bool,
) -> (anyhow::Result<()>, usize) {
    ask_as(args, someone_is_there, true).await
}

async fn ask_as(
    args: &ListArgs,
    someone_is_there: bool,
    from_the_config: bool,
) -> (anyhow::Result<()>, usize) {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let db = Store::open_at(&tmp.path().join("test.db")).unwrap();

    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());

    let mut app = App::for_test(
        client,
        db,
        Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        },
    );

    if from_the_config {
        app.consent_comes_from_the_config();
    }

    let result = engine::ask_consent_with(&mut app, args, someone_is_there).await;
    let spent = server.received_requests().await.unwrap().len();
    (result, spent)
}

/// The rule from AGENTS.md: consent before enumerating someone else, **before**
/// resolving. Resolving is itself a request, so asking afterwards means a "no"
/// has already cost one.
#[tokio::test]
async fn with_nobody_to_ask_a_named_account_is_refused_before_anything_is_spent() {
    let (result, spent) = ask(&args("@ghost"), false).await;

    let error = result.expect_err("an unanswerable question is not a yes");
    assert_eq!(ExitCode::from_chain(&error), Some(ExitCode::Interrupted));
    assert!(
        error.to_string().contains("-y"),
        "the refusal has to say how to run it unattended: {error}"
    );
    assert_eq!(spent, 0, "nothing may be spent on a run nobody authorized");
}

/// And the monitor is told the way the monitor takes an answer.
///
/// One sentence used to name `-y` for every caller, but `watch once` has no
/// `-y` and the reasoning for that is written at `WatchOnceArgs`: consent
/// handed over on a command line is consent from whoever wrote the cron entry.
/// So the refusal sent the operator to `snob watch once someone -y`, which
/// clap rejects with `error: unexpected argument` and exit 2 — advice that
/// cannot be followed, on the one path where nobody is watching.
#[tokio::test]
async fn the_monitor_is_pointed_at_the_answer_it_can_actually_take() {
    let (result, spent) = ask_as_the_monitor(&args("@ghost"), false).await;

    let error = result.expect_err("an unanswerable question is not a yes");
    let message = error.to_string();
    assert_eq!(ExitCode::from_chain(&error), Some(ExitCode::Interrupted));
    assert!(
        message.contains("snob watch setup"),
        "the monitor's answer is recorded once, in the file: {message}"
    );
    assert!(
        !message.contains("-y"),
        "`watch once` has no -y, so it must not be advertised: {message}"
    );
    assert_eq!(spent, 0, "nothing may be spent on a run nobody authorized");
}

/// Being unable to ask and being told no are two different events. They were
/// reported as one: `confirm` answers with its default the moment nobody can
/// answer, so the run failed with "canceled" and blamed the user for something
/// nobody did.
#[tokio::test]
async fn a_question_nobody_could_answer_is_not_read_as_a_no() {
    let (result, _) = ask(&args("@ghost"), false).await;
    let message = result.unwrap_err().to_string();

    assert!(
        message.contains("no terminal"),
        "it has to say why it could not ask: {message}"
    );
    assert!(
        !message.contains("canceled") && !message.contains("was not confirmed"),
        "nobody said no here: {message}"
    );
}

/// The whole point of the change. In an ordinary terminal with the results
/// going somewhere else — `snob scan someone | jq`, which the README
/// advertises — there is still somebody at the keyboard, and this used to stop
/// with exit 130 before spending a request. The gate now asks about standard
/// input alone, because the question is written to standard error.
#[tokio::test]
async fn output_going_somewhere_else_does_not_mean_nobody_is_there() {
    let (result, _) = ask(&args("@ghost"), true).await;

    // It gets as far as the question rather than refusing outright. The
    // question itself cannot be answered from a test, so what is pinned is that
    // the refusal is no longer the *unanswerable* one.
    if let Err(e) = result {
        assert!(
            !e.to_string().contains("no terminal"),
            "there is somebody there; it must not claim otherwise: {e}"
        );
    }
}

/// Your own account is not somebody else's, so there is nothing to agree to and
/// no reason to need a terminal.
#[tokio::test]
async fn your_own_name_never_needs_confirming_even_with_nobody_there() {
    let (result, spent) = ask(&args("@me"), false).await;
    result.expect("your own lists need no consent");
    assert_eq!(spent, 0);
}

/// The documented way to run it unattended. It has to work on the machine that
/// has no terminal at all, which is the only machine that needs it.
#[tokio::test]
async fn a_yes_given_in_advance_needs_no_terminal() {
    let mut args = args("@ghost");
    args.yes = true;

    let (result, spent) = ask(&args, false).await;
    result.expect("-y is consent");
    assert_eq!(spent, 0);
}

/// `--cache` is not asked about, because there is nothing to agree to.
///
/// Consent governs enumerating somebody else's lists. `--cache` reads a list
/// that was already walked — with permission — off this machine's own disk, and
/// resolves the name from the store rather than over the network, so the rule
/// that consent comes before resolution is not in play either.
///
/// Asking anyway cost more than a redundant prompt: with no terminal the
/// question cannot be put, so `snob unfollowers someone --cache` from cron or
/// down a pipe exited 130 over an answer that spends nothing and touches
/// nobody. It also disagreed with `cooldown::serve`, which hands back the
/// identical stored list with no question at all and says so in as many words.
#[tokio::test]
async fn a_cached_answer_about_somebody_else_needs_no_terminal() {
    use snob_core::model::{ListKind, StopReason, User};
    use snob_store::store::{accounts, snapshots, users};

    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Store::open_at(&tmp.path().join("test.db")).unwrap();

    // A list of theirs that was walked at some point, which is what `--cache`
    // is for reading back.
    let ghost = User {
        pk: Pk::new(7),
        username: "ghost".into(),
        full_name: None,
        is_private: None,
        is_verified: None,
        pfp_url: None,
    };
    users::upsert(db.conn(), &ghost).unwrap();
    accounts::upsert(db.conn(), Pk::new(7), false).unwrap();
    let opened = snapshots::begin(db.conn(), Pk::new(7), ListKind::Followers, Some(1)).unwrap();
    snapshots::save_page(
        &mut db,
        opened.id,
        &[User {
            pk: Pk::new(8),
            username: "someone".into(),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }],
        None,
    )
    .unwrap();
    snapshots::close(db.conn(), opened.id, StopReason::Completed).unwrap();

    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());
    let mut app = App::for_test(
        client,
        db,
        Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        },
    );

    // No `-y`, no terminal — `cargo test` is not one — and somebody else's
    // account. Every ingredient of the refusal, and it must not come.
    let args = ListArgs {
        cache: true,
        ..args("@ghost")
    };
    let (users, outcome) = engine::list(&mut app, &args, ListKind::Followers)
        .await
        .expect("a local answer needs nobody's permission and nobody's terminal");

    assert_eq!(users.len(), 1);
    assert_eq!(outcome.requests, 0, "nothing may be spent to answer this");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "and nothing may be asked of Instagram either"
    );
}

/// A yes about one account does not cover the next one.
///
/// `run_accounts` walks every configured account through one `App`, deliberately
/// — they share a session and a request budget. The consent flag on it was a
/// bare `bool`, which is indistinguishable from correct while one `App` means
/// one account, and is exactly the shape the `resolved` memo one field below it
/// records as having already bitten here once.
///
/// So an attended `snob watch once` over a hand-edited file naming two
/// unconsented strangers asked about the first, and enumerated the second one's
/// followers *and* following with no question printed.
#[tokio::test]
async fn an_answer_about_one_account_does_not_cover_another() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let db = Store::open_at(&tmp.path().join("test.db")).unwrap();

    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    let client = IgClient::new(session, Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());
    let mut app = App::for_test(
        client,
        db,
        Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        },
    );

    // Somebody answered yes about alice, the way the prompt does.
    app.record_consent("alice");

    // Asking again about alice is not asking again — that is what the flag is
    // for, and a crossing wants two lists and a summary four.
    engine::ask_consent_with(&mut app, &args("alice"), false)
        .await
        .expect("the same account was already answered for");

    // Bob was not.
    let error = engine::ask_consent_with(&mut app, &args("bob"), false)
        .await
        .expect_err("an answer about alice says nothing about bob");
    assert_eq!(ExitCode::from_chain(&error), Some(ExitCode::Interrupted));

    // And the at sign is not a different account.
    engine::ask_consent_with(&mut app, &args("@alice"), false)
        .await
        .expect("@alice and alice are one account");

    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "none of this may cost a request"
    );
}
