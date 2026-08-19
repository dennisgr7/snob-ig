//! The walk the source-reading guards share.
//!
//! Two tests in this directory read the repository's own source rather than
//! calling into the library: one checks that no test points at the real
//! keyring, the other that no Spanish and no punched-out sentence is left in
//! anything shipped. Neither can be done any other way — a runtime assertion
//! cannot see a file that was never compiled into this binary — and both need
//! the same awkward part: find the repository, walk it, skip the same
//! directories, and name a file the same way on every platform.
//!
//! That part was written out twice, with `repo_root` and `SKIP_DIRS`
//! byte-identical and the walkers differing only in which extensions they
//! wanted. The normalization was written three times. Only one of the two
//! copies documented why `exports` is skipped, which is the half worth keeping.
//!
//! **The allowlists stay where they are.** Each names files exempt from one
//! guard for that guard's own reason, and a shared list would read as though
//! the exemptions were about the file rather than about the rule.
//!
//! A `tests/common/mod.rs` is compiled separately into each binary that
//! declares it, so anything one of them does not touch is dead code there.
//! `allow` rather than `expect`, for the reason the CLI's copy records: whether
//! something is in fact unused differs per binary.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Walks up from the manifest until a `Cargo.lock` shows up.
///
/// `None` from a packaged build, where there is no repository to walk and the
/// guard has nothing to say rather than something to fail about.
pub fn repo_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|d| d.join("Cargo.lock").is_file())
        .map(Path::to_path_buf)
}

/// Directories the walk never descends into.
///
/// `target` and `.git` are the obvious ones. The rest are where a developer's
/// own files live, and since `json` joined the extension list the walk reaches
/// things that are nobody's source: an editor's settings, and — the one that
/// matters — an Instagram data export or a `snob lists -o out.json` written
/// from the repository root. Those are full of real names with real accents,
/// so a guard would fail on them **and print them into the assertion
/// message**. That is the "permanent noise and someone switches it off"
/// outcome the language rule warns about, arriving with someone else's
/// personal data attached.
pub const SKIP_DIRS: [&str; 6] = ["target", ".git", ".claude", ".vscode", ".idea", "exports"];

/// Every file under `root` whose extension is one of `extensions`.
pub fn source_files(root: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    pending.push(path);
                }
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| extensions.contains(&e))
            {
                found.push(path);
            }
        }
    }
    found
}

/// How a guard names a file: relative to the repository root, forward slashes
/// whichever platform it ran on.
///
/// Both halves matter. The allowlists are written with forward slashes, so on
/// Windows an unnormalized path matches none of them and every exempt file is
/// reported; and the assertion message is read by a person who wants a path
/// they can open, not one rooted at somebody's home directory.
pub fn relative(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The walk reaches this repository's own source, and stops where it says.
///
/// Every guard in this directory is worth exactly what the walk finds, and a
/// walk that finds nothing passes all of them in silence — which is the shape
/// of defect they exist to catch, arriving in the one place nothing was
/// watching. Both binaries that include this file run it.
#[test]
fn the_walk_reaches_the_source_it_is_meant_to_read() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let found: Vec<String> = source_files(&root, &["rs"])
        .iter()
        .map(|file| relative(&root, file))
        .collect();

    for anchor in [
        "crates/snob-core/src/lib.rs",
        "crates/snob-cli/src/main.rs",
        "crates/snob-ig/src/client.rs",
    ] {
        assert!(
            found.iter().any(|f| f == anchor),
            "the walk did not reach {anchor}, so it is not reading this repository"
        );
    }

    assert!(
        !found.iter().any(|f| f.starts_with("target/")),
        "the walk descended into a build directory"
    );

    // The extension filter is a filter rather than a suggestion.
    assert!(
        source_files(&root, &["rs"])
            .iter()
            .all(|f| f.extension().is_some_and(|e| e == "rs")),
        "the walk returned something that is not what was asked for"
    );
}
