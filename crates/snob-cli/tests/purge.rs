//! What `snob purge` leaves behind, which is meant to be nothing.
//!
//! The contract is uninstallation: a package manager removes the binary and
//! cannot touch anything else, so whatever this command misses stays on the
//! machine forever. The session matters most — it is a live credential — and
//! the tests below check it goes from every place it can be.
//!
//! Every store here points at a keyring service of its own. The real one
//! belongs to the operating system rather than to this process, and these tests
//! delete.

use snob_cli::cli::PurgeArgs;
use snob_cli::commands::purge;
use snob_cli::exit::ExitCode;
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;
use snob_core::session::{Session, SessionOrigin};
use snob_core::store::Store;

mod common;
use common::{SID, UA};

/// The keyring service this test uses, and the only place it is spelled.
///
/// Per test and per process: the real service belongs to the operating system
/// rather than to this process, and these tests delete.
fn service(name: &str) -> String {
    format!("snob-ig-test-purge-{name}-{}", std::process::id())
}

fn setup(name: &str) -> (tempfile::TempDir, AppPaths, SecretStore) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let store = SecretStore::new(paths.clone(), true).with_service(&service(name));
    (tmp, paths, store)
}

/// Everything a real install ends up with: a session, a database and the
/// browser profile `snob login` creates.
fn populate(paths: &AppPaths, store: &SecretStore) {
    store
        .save(&Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap())
        .unwrap();

    // Dropped straight away so nothing holds the file open: on Windows a live
    // handle is enough to make the removal fail.
    drop(Store::open(paths).unwrap());

    let profile = paths.browser_profile();
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("Cookies"), b"a logged-in profile").unwrap();
}

/// The only way to run the deletion path without a terminal.
fn purge_now() -> PurgeArgs {
    PurgeArgs {
        yes: true,
        dry_run: false,
    }
}

#[test]
fn purge_removes_the_session_the_database_and_the_browser_profile() {
    let (_tmp, paths, store) = setup("everything");
    populate(&paths, &store);

    assert!(paths.session_file().exists());
    assert!(paths.db_file().exists());
    assert!(paths.browser_profile().exists());

    purge::run(purge_now(), store, &paths).unwrap();

    assert!(!paths.session_file().exists(), "the credential survived");
    assert!(!paths.db_file().exists());
    assert!(!paths.browser_profile().exists());
    assert!(!paths.data_dir().exists());
}

/// An earlier version stored the session in the directory that roams with the
/// Windows profile. A purge that only cleared the current location would leave
/// a working credential in the older one.
#[test]
fn the_session_an_earlier_version_left_behind_goes_too() {
    let (_tmp, paths, store) = setup("legacy");
    let legacy = paths.legacy_session_file().unwrap();
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, b"{\"protection\":\"plain\",\"payload\":\"{}\"}").unwrap();

    purge::run(purge_now(), store, &paths).unwrap();

    assert!(!legacy.exists());
    assert!(!legacy.parent().unwrap().exists());
}

/// Reading is not deleting. Someone checking what a purge would take has to be
/// able to do it without taking it.
#[test]
fn a_dry_run_deletes_nothing() {
    let (_tmp, paths, store) = setup("dry-run");
    populate(&paths, &store);

    let args = PurgeArgs {
        yes: false,
        dry_run: true,
    };
    purge::run(args, store, &paths).unwrap();

    assert!(paths.session_file().exists());
    assert!(paths.db_file().exists());
    assert!(paths.browser_profile().exists());
}

/// Uninstalling a tool that never stored anything is a success, not an error.
#[test]
fn purging_a_machine_with_nothing_on_it_succeeds() {
    let (_tmp, paths, store) = setup("nothing");

    let plan = purge::survey(&store, &paths);
    assert!(plan.is_empty());

    purge::run(purge_now(), store, &paths).unwrap();
}

/// The listing shown before the question has to name what will actually go: an
/// answer given to an incomplete list is not consent to the rest.
#[test]
fn the_plan_names_the_session_and_every_directory_that_exists() {
    let (_tmp, paths, store) = setup("plan");
    populate(&paths, &store);

    let plan = purge::survey(&store, &paths);

    assert!(plan.session);
    assert!(plan.directories.contains(&paths.data_dir().to_path_buf()));
    assert!(
        !plan.directories.contains(&paths.config_dir().to_path_buf()),
        "a directory that does not exist is not listed as something to remove"
    );
    assert_eq!(plan.lines().len(), 1 + plan.directories.len());
}

/// A session too corrupt to parse is still a credential on the disk. Surveying
/// with `load()` alone would report nothing to do and leave it there.
#[test]
fn a_corrupt_session_is_still_something_to_remove() {
    let (_tmp, paths, store) = setup("corrupt");
    paths.ensure_dirs().unwrap();
    std::fs::write(paths.session_file(), b"this is not json").unwrap();

    assert!(purge::survey(&store, &paths).session);

    purge::run(purge_now(), store, &paths).unwrap();
    assert!(!paths.session_file().exists());
}

/// A session that would not go is the one failure this command cannot report as
/// success.
///
/// `SecretStore::delete` used to discard the keyring's answer, so a refusal
/// there reached `execute` as `Ok` and `run` printed "snob's files are gone from
/// this computer" and exited 0 over a live cookie. An uninstall script keyed on
/// that code then carried on to remove the binary. The keyring branch cannot be
/// driven from a test, but it and the file branch return through the same place,
/// which is what this pins — along with the other half: the rest goes even
/// though the credential refused.
///
/// A directory standing where the session file goes is how the refusal is
/// arranged: `remove_file` fails on one everywhere, unlike a permission bit.
#[test]
fn a_session_that_will_not_go_is_reported_and_the_exit_is_not_zero() {
    let (_tmp, paths, store) = setup("refused");
    populate(&paths, &store);

    let holding = paths.session_file();
    std::fs::remove_file(&holding).unwrap();
    std::fs::create_dir(&holding).unwrap();

    assert!(
        purge::survey(&store, &paths).session,
        "a session that cannot be read is still one to remove"
    );

    let error = purge::run(purge_now(), store, &paths)
        .expect_err("a credential that survived is not a success");
    assert_eq!(ExitCode::from_chain(&error), Some(ExitCode::Error));

    // And the other half: one item refusing does not spare the rest. The
    // browser profile holds a logged-in session of its own, so leaving it
    // because the keyring said no would be a second credential kept alive by
    // the first one's failure.
    assert!(!paths.browser_profile().exists());
}

/// With nobody to confirm at, nothing is deleted **and** it is not called a
/// success.
///
/// `confirm` keeps its default when nobody can answer, and the default here is
/// no — so an uninstall script used to be shown the whole plan, told "Nothing
/// was deleted.", and given exit 0. The one command whose purpose is that a live
/// credential does not outlive the tool reported success over an untouched
/// keyring.
#[test]
fn an_unattended_run_without_yes_is_refused_rather_than_assumed_no() {
    let (_tmp, paths, store) = setup("unattended");
    populate(&paths, &store);

    let args = PurgeArgs {
        yes: false,
        dry_run: false,
    };
    let error = purge::run_with(args, store, &paths, false)
        .expect_err("silence is not consent, and it is not success either");

    // The code the README's table and `--help` both promise for a confirmation
    // that was not given. It used to be the generic failure, so a script
    // branching on 130 to re-run with `--yes` never fired.
    assert_eq!(ExitCode::from_chain(&error), Some(ExitCode::Interrupted));
    assert!(
        error.to_string().contains("terminal"),
        "it has to say why it could not ask: {error}"
    );
    assert!(
        error.to_string().contains("--yes"),
        "and how to do it unattended: {error}"
    );
    assert!(
        paths.session_file().exists(),
        "nothing may be deleted without an answer"
    );
    assert!(paths.browser_profile().exists());
}

/// The other side of the same gate. `--yes` is the documented way to run this on
/// a machine with no terminal at all, which is the only machine that needs it.
#[test]
fn a_typed_yes_needs_nobody_to_confirm_at() {
    let (_tmp, paths, store) = setup("typed-yes");
    populate(&paths, &store);

    purge::run_with(purge_now(), store, &paths, false).unwrap();
    assert!(!paths.session_file().exists());
}

/// The monitor's secrets go even when there is no session left to find.
///
/// `execute` gated the workspace's only `delete_all()` call on `plan.session`,
/// which is `something_is_stored()`, which reads the session entry and nothing
/// else. `snob watch setup` needs no session and `snob logout` removes the one
/// there is by design, so setup → logout → purge deleted the directories,
/// printed "snob's files are gone from this computer.", exited 0, and left a
/// live webhook token and signing key in the keyring for good.
///
/// AGENTS.md read this rule as "every secret this tool stores is one `purge`
/// removes". It was true of `delete_all` and false of the command.
#[test]
fn purge_removes_the_monitors_secrets_with_no_session_stored() {
    use snob_core::secret::Secret;
    use snob_core::secrets::Kind;

    let (_tmp, paths, store) = setup("monitor-secrets");
    store
        .save_secret(Kind::WatchToken, &Secret::new("Bearer team"))
        .unwrap();
    store
        .save_secret(Kind::WatchSigningKey, &Secret::new("shared-secret"))
        .unwrap();

    let plan = purge::survey(&store, &paths);
    assert!(!plan.session, "this is the case with no session at all");
    assert!(
        !plan.is_empty(),
        "two live credentials are not nothing to remove"
    );
    assert!(
        plan.lines().iter().any(|l| l.contains("webhook token")),
        "an answer given to an incomplete list is not consent to the rest: {:?}",
        plan.lines()
    );

    purge::run(purge_now(), store, &paths).unwrap();

    // Re-opened rather than reusing the moved store, and pointed at the same
    // service name, which is what makes this a question about the keyring
    // rather than about a handle.
    let after = SecretStore::new(paths.clone(), true).with_service(&service("monitor-secrets"));
    assert!(after.load_secret(Kind::WatchToken).unwrap().is_none());
    assert!(after.load_secret(Kind::WatchSigningKey).unwrap().is_none());
}
