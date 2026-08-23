//! Fixtures shared by the integration tests.
//!
//! The credentials and the default `ListArgs` were copied into every test binary
//! that needed them, so adding one flag to `ListArgs` — which `--only`,
//! `--max-pages` and `--yes` all did — meant editing files that were each
//! testing something else. They live here once instead.
//!
//! Not every binary uses every item, and a `tests/common/mod.rs` is compiled
//! separately into each one that declares it, so anything one of them does not
//! touch is reported as dead code there. `allow` rather than `expect`: whether
//! anything is in fact unused differs per binary, and `expect` warns about
//! itself in the binaries that happen to use all of it.
#![allow(dead_code)]

use snob_cli::cli::{FilterArgs, ListArgs, OutputArgs, WalkArgs};

/// A plausible desktop Chrome User-Agent. The session is tied to one, and
/// Instagram checks that the two agree.
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

/// A `sessionid` in the shape Instagram issues: the account id, a token and a
/// version, percent-encoded as the cookie carries them.
pub const SID: &str = "42%3AAbCdEfGh%3A20";

/// The arguments a list command gets with no flags given.
///
/// `no_progress` and `yes` are the two that differ from the real defaults, and
/// deliberately: a bar drawing into the test harness is noise, and a test that
/// stops to ask for consent hangs.
pub fn args() -> ListArgs {
    ListArgs {
        target: None,
        filter: FilterArgs::default(),
        output: OutputArgs::default(),
        limit: None,
        walk: WalkArgs {
            no_progress: true,
            yes: true,
            ..WalkArgs::default()
        },
    }
}

/// The same arguments, aimed at somebody else's account.
pub fn args_for(target: &str) -> ListArgs {
    ListArgs {
        target: Some(target.to_string()),
        ..args()
    }
}
