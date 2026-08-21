//! Nothing in this tool tells anybody you looked at their story.
//!
//! `snob stories` reads a reel and downloads media from the CDN. Registering a
//! view is a **separate** call — Instagram's own clients make it explicitly,
//! which is why anonymous story viewers exist at all — and this project does
//! not make it. That is a promise to the person whose story it is, not to the
//! person running the tool, and it is the one promise here that nobody running
//! the tool would ever notice being broken.
//!
//! It is easy to break by accident and impossible to see. Adding `media/seen`
//! to the client is four lines that compile, pass every test, and read as an
//! obvious courtesy — mark what was viewed as viewed — while quietly turning a
//! read into a write. So the check reads the source: if any spelling of the
//! call appears anywhere in the three crates, this fails and says where.
//!
//! A source check rather than a runtime one, for the reason `tests/keyring.rs`
//! and `tests/sandbox.rs` both give: an integration test compiles the library
//! without `cfg(test)`, so an assertion inside the process is blind in exactly
//! the files that matter. There is also nothing to assert *about* — the defect
//! being guarded against is the presence of code, not the behavior of code that
//! is there.
//!
//! **This file is exempt from itself.** It names every spelling in the list
//! below, which is the whole point of it.

mod common;
use common::{relative, repo_root, source_files};

/// Every spelling of "tell them I looked".
///
/// `media/seen` is the endpoint; `reels/seen` is what the mobile clients call
/// the same thing on a reel; `mark_seen` and `mark_as_seen` are what somebody
/// would name the function before they wrote the path. The last two are here
/// because the guard has to catch the change at the moment it is made, and the
/// function usually arrives before the URL does.
const SEEN: [&str; 5] = [
    "media/seen",
    "reels/seen",
    "mark_seen",
    "mark_as_seen",
    "MarkSeen",
];

/// This file, which names all five above.
const EXEMPT: [&str; 1] = ["crates/snob-core/tests/no_seen.rs"];

#[test]
fn nothing_marks_a_story_as_seen() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let mut violations = Vec::new();
    for file in source_files(&root, &["rs"]) {
        let name = relative(&root, &file);
        if EXEMPT.contains(&name.as_str()) {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        violations.extend(mentions_in(&name, &contents));
    }

    assert!(
        violations.is_empty(),
        "{} line(s) would tell somebody their story was viewed:\n{}\n\n\
         Reading a reel does not register a view. Sending one of these does, \
         and AGENTS.md forbids it.",
        violations.len(),
        violations.join("\n")
    );
}

/// The walk over one file's text, so a test can hand it a fixture.
///
/// Split out for the reason the sibling guards give about their own: a guard
/// that walks the tree and finds nothing looks exactly like a guard that walks
/// nothing, and the two have to be told apart by something.
fn mentions_in(name: &str, contents: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        for spelling in SEEN {
            if line.contains(spelling) {
                found.push(format!("{name}:{}: {}", index + 1, line.trim()));
            }
        }
    }
    found
}

/// The guard finds what it is looking for when it is there.
///
/// Without this, a typo in `SEEN` — or a walk that reads no files — is a test
/// that passes for ever while watching nothing, which is the failure mode every
/// source-reading guard in this directory shares.
#[test]
fn the_guard_sees_the_call_it_exists_to_stop() {
    let planted = r#"
        // An innocent-looking courtesy.
        self.post("/api/v1/media/seen/", &fields, referer).await?;
    "#;
    let caught = mentions_in("crates/snob-ig/src/client.rs", planted);
    assert_eq!(
        caught.len(),
        1,
        "the guard missed a planted call: {caught:?}"
    );
    assert!(caught[0].contains("client.rs:3"));

    assert!(
        mentions_in("x.rs", "let seen = already_downloaded.contains(&pk);").is_empty(),
        "the word \"seen\" on its own is not the call, and must not be reported"
    );
}
