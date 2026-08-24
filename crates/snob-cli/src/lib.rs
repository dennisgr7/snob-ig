//! snob-ig command line interface.
//!
//! Exposed as a library as well as a binary so the commands can be tested end
//! to end against a mock server. Integration tests cannot import modules from a
//! binary.

// `pub(crate)` where nothing outside the crate reaches in: the integration
// tests and `main` are the two consumers. Exporting a module they never use
// switches off dead-code detection inside it, which is the one thing the
// export of a binary's module costs.
pub mod app;
pub mod browser;
pub mod cdp;
pub mod cli;
pub mod commands;
pub mod engine;
pub mod exit;
pub(crate) mod interrupt;
pub mod output;
/// Starting the browser with the debugging protocol on a pipe rather than on a
/// loopback port. See the module for why it cannot be `std::process::Command`.
pub mod pipe;
pub(crate) mod progress;
pub mod report;
pub mod ui;
pub mod watch;
