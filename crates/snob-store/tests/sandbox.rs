//! The seam that points this tool somewhere other than Instagram stays behind
//! its Cargo feature.
//!
//! A released binary is not supposed to contain it at all — not the flags, not
//! the code they reach. That is what the feature buys over a hidden flag, and
//! it is the reason the sandbox harness costs a whole feature rather than two
//! `#[arg(hide = true)]` lines: a flag that exists can be typed, and the thing
//! it would do is send a live session cookie to somebody else's server.
//!
//! The property is easy to lose by accident and impossible to notice. Deleting
//! one `#[cfg(feature = "testing")]` leaves every build green — the tests still
//! pass, because they run with the feature on, and the release build still
//! compiles, because the code is valid. Nothing would say anything until
//! somebody read `snob --help` on a shipped binary and found `--ig-base-url`
//! in it. So the check reads the source.
//!
//! **What it checks**: every mention of the seam — the two flag fields, the
//! function that redirects a client, and the static behind it — has a
//! `#[cfg(feature = "testing")]` above it **with no blank line in between**,
//! which is what makes the guard the item's own rather than the previous
//! item's. Test files are skipped, because a test is already the feature's own
//! build, and so is this file, which names all four in the sentence you are
//! reading.
//!
//! It is a source check and not a runtime one for the same reason
//! `tests/keyring.rs` is: a test binary cannot see how another crate was
//! compiled, and `cfg!(feature = "testing")` inside the test is true in exactly
//! the build where the answer does not matter.

// **The repository-walking helpers, shared across the crate boundary.**
//
// `#[path]` rather than a copy: these three functions decide what "the source of
// this repository" means, and the guards that read sources — the keyring rule
// here, the language and story-view rules in `snob-core` — have to agree about
// it exactly. Two copies would be two answers to "which files count", and the
// one that drifted would be a guard quietly walking less than it says.
//
// A `snob-core` dev-dependency would be the tidy way and is worse: it would put
// an edge from the storage crate back to the domain crate's *tests*, and the
// file is 135 lines of `std::fs` with no dependencies of its own.
#[path = "../../snob-core/tests/common/mod.rs"]
mod common;
use common::{relative, repo_root, source_files};

/// Every name that must not exist in a release build.
///
/// The flag fields, the function `main` calls to redirect a client, and the
/// static it writes to. `--ig-base-url` and `--sandbox-root` are the spellings
/// a user would type; the identifiers are what clap derives them from, so both
/// are listed and either one appearing unguarded is the same defect.
const SEAM: [&str; 5] = [
    "point_every_client_at",
    "SANDBOX_BASE",
    "sandbox_root",
    "ig_base_url",
    "--ig-base-url",
];

#[test]
fn the_sandbox_seam_stays_behind_its_feature() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let mut violations = Vec::new();
    for file in source_files(&root, &["rs"]) {
        let relative = relative(&root, &file);

        // A test is the feature's own build, so a mention there is not a
        // release-build mention. This file names all four in its own header.
        if relative.contains("/tests/") || relative.contains("\\tests\\") {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        violations.extend(unguarded_in(&relative, &contents));
    }

    assert!(
        violations.is_empty(),
        "{} line(s) put the sandbox seam in a released binary:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// The walk, over one file's text, so a test can hand it a fixture.
///
/// Split out for the reason `tests/keyring.rs` gives about its own: a guard
/// that walks the tree and finds nothing looks exactly like a guard that walks
/// nothing, so the matcher has to be drivable over text written here.
///
/// **The search upward stops at a blank line, and that is the whole of the
/// rule.** It used to count a fixed forty lines and stop there, which does not
/// respect where one item ends and the next begins — and the two flags sit
/// twenty-eight lines apart in `cli.rs`, well inside any reach large enough to
/// clear a long doc comment. So `sandbox_root`'s `#[cfg]` was answering for
/// `ig_base_url`, and deleting the only line keeping `--ig-base-url` out of a
/// released binary left this test green. An item carries its attributes and
/// doc comment directly above it with no blank line between; the blank line
/// before the next item's doc comment is the boundary, and it needs no
/// constant to describe it.
fn unguarded_in(relative: &str, contents: &str) -> Vec<String> {
    let lines: Vec<&str> = contents.lines().collect();
    let mut violations = Vec::new();

    let mut number = 0;
    while number < lines.len() {
        let line = lines[number];

        // Test code is not in a released binary either, so it is stepped over
        // rather than examined — the same halving `keyring.rs` does, and needed
        // here for the same reason: `cli.rs` parses both flags in its own unit
        // tests, which is exactly where they should be parsed.
        //
        // **What is stepped over is the gated item, not the rest of the file.**
        // This used to stop reading at the first `#[cfg(test)]` of any kind, on
        // the assumption that it was always the trailing test module. It is
        // not: `paths.rs` has two gated helpers and `progress.rs` one, and
        // everything below each of them was going unchecked — which is the same
        // stopped-reading-too-early defect this function's own doc comment
        // congratulates itself for having fixed in the other direction. A
        // trailing test module is just the case where the item happens to run
        // to the end of the file, so one rule covers both.
        if is_a_test_gate(line) {
            number += lines_in_the_item_after(&lines[number..]);
            continue;
        }

        number += 1;

        // Not a mention: the `#[cfg]` itself, and the comment prose that
        // explains the seam, which names the flags on purpose.
        if line.trim_start().starts_with("//") {
            continue;
        }
        if !SEAM.iter().any(|name| line.contains(name)) {
            continue;
        }

        let mut guarded = false;
        for above in lines[..number].iter().rev() {
            if above.trim().is_empty() {
                break; // the previous item's guard is not this item's guard
            }
            if is_the_feature_gate(above) {
                guarded = true;
                break;
            }
        }
        if !guarded {
            violations.push(format!("{relative}:{}: {}", number, line.trim()));
        }
    }

    violations
}

/// How many lines the item introduced at `rest[0]` occupies, attributes and
/// all, so the walk can step over it.
///
/// Braces rather than indentation, because `rustfmt` is not the authority here
/// and a string containing a brace is rarer in an item's signature than an
/// unusual layout is. An item with no braces at all — a gated `use`, a gated
/// `const` — ends at its semicolon. An item whose braces never close runs to
/// the end of the file, which is exactly what the trailing test module does.
fn lines_in_the_item_after(rest: &[&str]) -> usize {
    let mut depth = 0i32;
    let mut opened = false;
    for (offset, line) in rest.iter().enumerate() {
        for byte in line.chars() {
            match byte {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if opened && depth <= 0 {
            return offset + 1;
        }
        // A one-line item with no block: `#[cfg(test)] use x;` on the next line.
        if !opened && offset > 0 && line.trim_end().ends_with(';') {
            return offset + 1;
        }
    }
    rest.len()
}

/// Whether a line is a `cfg` that carries this feature.
///
/// Matched by its two parts rather than as one literal, because the gate is
/// written more than one way: `main.rs`'s own tests are behind
/// `#[cfg(all(test, feature = "testing"))]`, and a literal comparison calls
/// that unguarded.
fn is_the_feature_gate(line: &str) -> bool {
    line.contains("#[cfg(") && line.contains("feature = \"testing\"")
}

/// Whether a line opens a module a release build does not compile.
///
/// `#[cfg(test)]` and `#[cfg(all(test, ...))]` both. Matched on `test` as a
/// bare predicate rather than on the whole attribute, so the longer spelling
/// `main.rs` uses is recognized as the same thing.
fn is_a_test_gate(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("#[cfg(") && (line.contains("(test)") || line.contains("(test,"))
}

/// And the guard can see a breach, which is the half a guard usually cannot
/// prove about itself.
///
/// The last two cases are the defect this replaced: a guard belonging to the
/// item above, reached across the blank line that separates them.
#[test]
fn the_guard_would_notice_an_unguarded_flag() {
    let guarded = "\
#[cfg(feature = \"testing\")]
#[arg(long, global = true)]
pub ig_base_url: Option<Url>,
";
    let bare = "\
/// Ask this server instead
pub ig_base_url: Option<Url>,
";
    let gate_with_test = "\
#[cfg(all(test, feature = \"testing\"))]
fn only_a_test_reaches(cli: &Cli) -> bool { cli.ig_base_url.is_some() }
";
    let borrowed_from_the_item_above = "\
#[cfg(feature = \"testing\")]
pub sandbox_root: Option<PathBuf>,

/// Ask this server instead of Instagram
pub ig_base_url: Option<Url>,
";

    assert!(
        unguarded_in("x.rs", guarded).is_empty(),
        "a guarded field is not a breach"
    );
    assert_eq!(
        unguarded_in("x.rs", bare).len(),
        1,
        "an unguarded one is, and this has to see it"
    );
    assert!(
        unguarded_in("x.rs", gate_with_test).is_empty(),
        "`all(test, feature = ...)` is the same gate spelled longer"
    );
    assert_eq!(
        unguarded_in("x.rs", borrowed_from_the_item_above),
        vec!["x.rs:5: pub ig_base_url: Option<Url>,".to_string()],
        "the guard above the blank line belongs to the field above it"
    );

    // **The hole this walk had until the item skip replaced the file skip.**
    // One gated helper near the top, and everything below it stopped being
    // read. `paths.rs` has two of these and `progress.rs` one, so the tree was
    // already relying on nobody putting a seam mention underneath them.
    let a_gated_helper_then_an_unguarded_field = "#[cfg(test)]
fn a_helper() -> bool {
    true
}

/// Ask this server instead
pub ig_base_url: Option<Url>,
";
    assert_eq!(
        unguarded_in("x.rs", a_gated_helper_then_an_unguarded_field),
        vec!["x.rs:7: pub ig_base_url: Option<Url>,".to_string()],
        "a lone #[cfg(test)] item must not stop the walk for the rest of the file"
    );

    // And the trailing test module still ends it, because the item it gates
    // runs to the end of the file. Same rule, no special case.
    let the_trailing_test_module = "pub sandbox_root: Option<PathBuf>,

#[cfg(test)]
mod tests {
    fn uses_the_seam() -> &'static str {
        \"--ig-base-url\"
    }
}
";
    assert!(
        unguarded_in("x.rs", the_trailing_test_module)
            .iter()
            .all(|found| !found.contains("ig-base-url")),
        "the test module is test code and is not examined"
    );
}
