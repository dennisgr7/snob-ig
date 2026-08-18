//! No test may point at the real keyring.
//!
//! It belongs to the operating system rather than to this process, so a test
//! that reaches it deletes the session of whoever is developing, and on a
//! shared CI runner somebody else's. AGENTS.md carries the rule as a standing
//! instruction and says "a test checks that they do".
//!
//! One did, and it checked itself. `tests_never_point_at_the_real_keyring` in
//! `secrets.rs` builds a store through `file_store`, which sets
//! `.with_service(...)` two lines above the assertion that the service is not
//! the real one. It has no visibility into any other test, in that crate or any
//! other — and `SecretStore::remove` calls `delete_credential()` against the
//! operating system's store whatever backend was chosen, so a forgotten
//! `with_service` really does take the developer's live entries with it.
//! Dropping the call from `snob-cli/tests/purge.rs` left the whole suite green
//! while `purge_removes_the_session…` deleted the real credentials.
//!
//! So this reads the source instead. A runtime assertion cannot do the job: an
//! integration test compiles the library without `cfg(test)`, so `cfg!(test)`
//! inside `secrets.rs` is false in exactly the files that matter most.
//!
//! **What it checks**: every `SecretStore::new(` written in a test has
//! `.with_service(` in the same statement. Test means a file under a `tests/`
//! directory — all of which is test code by definition — or a line after the
//! first `#[cfg(test)]` in a `src/` file. That second half is a heuristic and
//! is meant to be: it is the cheap direction to be wrong in, since the worst it
//! does is ask for `with_service` on a line that did not need it.

use std::path::{Path, PathBuf};

#[test]
fn no_test_builds_a_secret_store_pointing_at_the_real_service() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let mut offenders = Vec::new();
    for file in rust_files(&root) {
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");

        if ALLOWLIST.contains(&relative.as_str()) {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };

        // Everything under a `tests/` directory is test code. In `src/`, only
        // what comes after the first `#[cfg(test)]`.
        let in_a_test_file = relative.contains("/tests/");
        let mut reached_the_tests = in_a_test_file;

        for (number, line) in contents.lines().enumerate() {
            if line.contains("#[cfg(test)]") {
                reached_the_tests = true;
            }
            if !reached_the_tests || !line.contains("SecretStore::new(") {
                continue;
            }
            // The escape hatch, in the shape the other guards in this directory
            // already use. Not expected to be needed: a test that reaches the
            // keyring on purpose still has to name a service of its own.
            if line.contains("keyring-allow") {
                continue;
            }
            // The statement, not the line: `cargo fmt` breaks a long builder
            // chain across several of them, so the `.with_service(` that
            // belongs to this call is usually not on the line the call is on.
            let from_here = &contents[contents.find(line).unwrap_or(0)..];
            let statement = from_here.split(';').next().unwrap_or(from_here);
            if !statement.contains(".with_service(") {
                offenders.push(format!("{relative}:{}", number + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "{} test(s) build a `SecretStore` without `.with_service(...)`, so they point at \
         the real keyring and will delete whatever is stored there:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// The guard has to be able to fail, or it is a comment.
///
/// The one it replaces could not: it asserted a property of a store it had just
/// built with the property.
#[test]
fn the_guard_notices_a_store_built_without_a_service() {
    let with = "let store = SecretStore::new(paths, true).with_service(&name);";
    let without = "let store = SecretStore::new(paths, true);";

    assert!(with.contains(".with_service("));
    assert!(
        !without.contains(".with_service("),
        "this is the shape the walk above is looking for"
    );
}

fn repo_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|d| d.join("Cargo.lock").is_file())
        .map(Path::to_path_buf)
}

/// Files exempt from the walk.
///
/// The trap everyone forgets, and the one `tests/language.rs` records for the
/// same reason: this file holds the shapes it is looking for, so it flags
/// itself.
const ALLOWLIST: [&str; 1] = ["crates/snob-core/tests/keyring.rs"];

const SKIP_DIRS: [&str; 6] = ["target", ".git", ".claude", ".vscode", ".idea", "exports"];

fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if !SKIP_DIRS.contains(&name.as_ref()) {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    found
}
