//! Getting the monitor's reports to wherever the user wants them.
//!
//! The report itself is `engine::watch`'s answer and the sentences a person
//! reads are `commands::watch`'s. What is here is the third destination: an
//! address somebody configured, and everything that goes with talking to a
//! server this project knows nothing about.

pub mod webhook;
