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

use snob_core::session::{Session, SessionOrigin};
use snob_core::store::Store;
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use url::Url;
use wiremock::MockServer;

use snob_cli::app::{App, Viewer};
use snob_cli::cli::ListArgs;
use snob_cli::engine;
use snob_cli::exit::ExitCode;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
const SID: &str = "42%3AAbCdEfGh%3A20";

/// Someone else's account, and no `-y`: the case that needs a question.
fn args(target: &str) -> ListArgs {
    ListArgs {
        target: Some(target.into()),
        hide: vec![],
        only: vec![],
        no_verified: false,
        exclude_list: None,
        format: None,
        output: None,
        limit: None,
        refresh: false,
        cache: false,
        max_age: std::time::Duration::from_secs(6 * 3600),
        no_resume: false,
        max_pages: None,
        no_progress: true,
        yes: false,
    }
}

/// No mock is mounted on purpose. Every test here asserts the answer arrives
/// without a request, so a request would be a connection refused rather than a
/// quietly served page.
async fn ask(args: &ListArgs, someone_is_there: bool) -> (anyhow::Result<()>, usize) {
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
            pk: 42,
            username: Some("me".into()),
        },
    );

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
