//! The pieces of the interactive views that are not drawing.
//!
//! Drawing belongs to `ratatui`, through the guard and chrome in `ui::tui`;
//! what remains here is what a renderer cannot do:
//!
//! - [`input`] reads keys, with the timeout, the resize event, the modified
//!   keys and the bracketed paste that `console` cannot deliver — its header
//!   carries the two defects that forced the split.
//! - [`scratch`] is the per-session directory a browser opens media out of,
//!   and the sweep for sessions that never got to clean up.
//!
//! Neither knows anything about stories or accounts; the views turn their
//! subjects into rows and these two hand them keys and disk. That is what
//! lets the awkward parts — binding a modified key, sweeping an abandoned
//! directory — be tested without a terminal at all.

pub mod input;
pub mod scratch;
