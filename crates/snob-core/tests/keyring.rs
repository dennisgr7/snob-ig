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
        offenders.extend(offenders_in(&relative, &contents));
    }

    assert!(
        offenders.is_empty(),
        "{} test(s) build a `SecretStore` without `.with_service(...)`, so they point at \
         the real keyring and will delete whatever is stored there:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// The walk, over one file's text, so a test can hand it a fixture.
///
/// It was inline, and the test that claimed to check it asserted on two string
/// literals it declared itself — a fact about `str::contains`, not about this.
/// So the guard had no coverage at all, which is how the offset defect below
/// came to be written and to survive.
fn offenders_in(relative: &str, contents: &str) -> Vec<String> {
    let mut offenders = Vec::new();

    {
        // Everything under a `tests/` directory is test code. In `src/`, only
        // what comes after the first `#[cfg(test)]`.
        let in_a_test_file = relative.contains("/tests/");
        let mut reached_the_tests = in_a_test_file;

        // The offset is carried rather than searched for. `contents.find(line)`
        // answers with the **first** occurrence of that text, so two identical
        // `SecretStore::new(` lines were both checked against the first one's
        // statement — and one of them is already in the tree, because rustfmt
        // wraps a long builder chain and leaves the call on a line of its own.
        // A copy-paste of the offending shape would have passed the guard whose
        // whole job is to stop a test wiping a live Instagram session.
        // `split_inclusive` rather than `lines`, so the arithmetic is exact on
        // both line endings: `lines()` strips a trailing `\r` as well as the
        // `\n`, and adding a fixed one back drifts by a byte per line on a CRLF
        // file — which every file in this repository is, in the working tree.
        let mut offset = 0usize;
        for (number, raw) in contents.split_inclusive('\n').enumerate() {
            let at = offset;
            offset += raw.len();
            let line = raw.trim_end_matches(['\n', '\r']);

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
            let statement = contents[at..].split(';').next().unwrap_or("");
            if !statement.contains(".with_service(") {
                offenders.push(format!("{relative}:{}", number + 1));
            }
        }
    }

    offenders
}

/// The guard has to be able to fail, or it is a comment.
///
/// It could not. This asserted that one string literal contains
/// `.with_service(` and another does not — a fact about `str::contains`,
/// declared and checked in the same two lines, with the walk itself untouched.
/// It is handed a fixture now and has to say which lines are wrong.
#[test]
fn the_guard_notices_a_store_built_without_a_service() {
    let fixture = "\
fn setup() {
    let ok = SecretStore::new(paths.clone(), true)
        .with_service(&service(name));
    let bad = SecretStore::new(paths.clone(), true);
}
";
    assert_eq!(
        offenders_in("crates/x/tests/y.rs", fixture),
        vec!["crates/x/tests/y.rs:4".to_string()],
        "line 2 is fine across the wrap, line 4 is not"
    );
}

/// Two identical calls are two calls, and the second is checked against its own
/// statement.
///
/// `contents.find(line)` answered with the **first** occurrence of that text,
/// so a duplicated line was measured against the earlier statement's
/// `.with_service(` and passed. The shape is already in the tree — rustfmt
/// wraps a long builder chain and leaves the call alone on its line — so one
/// copy-paste would have walked past the guard whose whole job is to stop a
/// test wiping a live Instagram session.
#[test]
fn a_repeated_call_is_not_excused_by_an_earlier_one() {
    // The two calls are byte-identical, which is the whole point: that is what
    // a copy-paste produces, and what `find` cannot tell apart.
    let fixture = "\
fn setup() {
    let store = SecretStore::new(paths.clone(), true)
        .with_service(&service(name));
    let store = SecretStore::new(paths.clone(), true)
        .using(Backend::File);
}
";
    assert_eq!(
        offenders_in("crates/x/tests/y.rs", fixture),
        vec!["crates/x/tests/y.rs:4".to_string()],
        "only the second is an offender, and it must not inherit the first's service"
    );
}

/// In `src/`, only what comes after the first `#[cfg(test)]` is test code —
/// production building its own store is the ordinary case.
#[test]
fn production_code_is_not_asked_to_name_a_test_service() {
    let fixture = "\
fn main() {
    let store = SecretStore::new(paths, true);
}

#[cfg(test)]
mod tests {
    fn t() {
        let store = SecretStore::new(paths, true);
    }
}
";
    assert_eq!(
        offenders_in("crates/x/src/main.rs", fixture),
        vec!["crates/x/src/main.rs:8".to_string()]
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
