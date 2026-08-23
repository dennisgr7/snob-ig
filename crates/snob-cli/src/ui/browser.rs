//! The pieces the interactive story list is drawn with.
//!
//! Three small modules rather than a terminal-UI framework, and the reason is
//! measured rather than felt. `ratatui` with `crossterm` is the obvious answer
//! and it costs **106,496 bytes and 27 crates** in this binary; these three
//! files cost **18,432 bytes and no new crate on Windows or macOS**, because
//! `crossterm` was already being compiled by `comfy-table` and the drawing side
//! is `console`, which was already here. Most of what the difference buys is a
//! cell buffer and a constraint solver for layout, and a list of rows of text
//! uses neither. Both numbers, and the case in which the trade would go the
//! other way, are written down in AGENTS.md.
//!
//! - [`screen`] draws a block of rows and rewrites only what changed.
//! - [`viewport`] decides which slice of a long list is on screen.
//! - [`input`] reads keys, and is the half that is not `console`.
//!
//! None of them knows anything about stories; `ui::stories` turns stories into
//! rows and these three put rows on a terminal. That is what lets the awkward
//! parts -- cutting to a display width, following a selection, binding a
//! modified key -- be tested without a terminal at all.

pub mod input;
pub mod scratch;
pub mod screen;
pub mod viewport;
