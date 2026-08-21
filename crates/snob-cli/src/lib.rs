//! snob-ig command line interface.
//!
//! Exposed as a library as well as a binary so the commands can be tested end
//! to end against a mock server. Integration tests cannot import modules from a
//! binary.

pub mod app;
pub mod browser;
pub mod cdp;
pub mod cli;
pub mod commands;
pub mod engine;
pub mod exit;
pub mod interrupt;
pub mod output;
/// Starting the browser with the debugging protocol on a pipe rather than on a
/// loopback port. See the module for why it cannot be `std::process::Command`.
pub mod pipe;
pub mod progress;
pub mod report;
pub mod ui;
pub mod watch;
