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
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;
use snob_core::session::{Session, SessionOrigin};
use snob_core::store::Store;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
const SID: &str = "42%3AAbCdEfGh%3A20";

fn setup(name: &str) -> (tempfile::TempDir, AppPaths, SecretStore) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let store = SecretStore::new(paths.clone(), true)
        .with_service(&format!("snob-ig-test-purge-{name}-{}", std::process::id()));
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
        "nothing writes configuration yet, so there is no such directory to list"
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
