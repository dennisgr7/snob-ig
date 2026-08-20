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
//! `#[cfg(feature = "testing")]` within a few lines above it. Test files are
//! skipped, because a test is already the feature's own build, and so is this
//! file, which names all four in the sentence you are reading.
//!
//! It is a source check and not a runtime one for the same reason
//! `tests/keyring.rs` is: a test binary cannot see how another crate was
//! compiled, and `cfg!(feature = "testing")` inside the test is true in exactly
//! the build where the answer does not matter.

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

/// How far above a mention the guard may sit.
///
/// A field carries its doc comment between the `#[cfg]` and the name, and those
/// doc comments are long here on purpose — the argument for the pairing is
/// written into them. Forty lines covers the longest of them with room, and it
/// is still far too short to reach the previous item's guard by accident.
const REACH: usize = 40;

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
        let lines: Vec<&str> = contents.lines().collect();

        for (number, line) in lines.iter().enumerate() {
            // Not a mention: the `#[cfg]` itself, and the comment prose that
            // explains the seam, which names the flags on purpose.
            if line.trim_start().starts_with("//") {
                continue;
            }
            if !SEAM.iter().any(|name| line.contains(name)) {
                continue;
            }

            let from = number.saturating_sub(REACH);
            let guarded = lines[from..=number]
                .iter()
                .any(|above| above.contains("#[cfg(feature = \"testing\")]"));
            if !guarded {
                violations.push(format!("{relative}:{}: {}", number + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "{} line(s) put the sandbox seam in a released binary:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// And the guard can see a breach, which is the half a guard usually cannot
/// prove about itself.
///
/// `tests/keyring.rs` records the same problem in its own header: a check that
/// walks source and finds nothing is indistinguishable from a check that walks
/// nothing. This drives the matcher over two lines written here rather than
/// over the tree.
#[test]
fn the_guard_would_notice_an_unguarded_flag() {
    let guarded = [
        "#[cfg(feature = \"testing\")]",
        "    pub ig_base_url: bool,",
    ];
    let bare = [
        "    /// Ask this server instead",
        "    pub ig_base_url: bool,",
    ];

    let breached = |lines: &[&str; 2]| {
        let last = lines[1];
        SEAM.iter().any(|name| last.contains(name))
            && !lines
                .iter()
                .any(|l| l.contains("#[cfg(feature = \"testing\")]"))
    };

    assert!(!breached(&guarded), "a guarded field is not a breach");
    assert!(
        breached(&bare),
        "an unguarded one is, and this has to see it"
    );
}
