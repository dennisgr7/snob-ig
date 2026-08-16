//! Client for Instagram's private API.
//!
//! Read-only. This crate must never expose write operations (follow, unfollow,
//! block, remove-follower): that is where the risk of an action block sits, and
//! it is out of scope for the project.
//!
//! Each endpoint is documented above the method that calls it, in `client.rs`.

pub mod client;
pub mod client_hints;
pub mod error;
pub mod login;
pub mod model;
pub mod pace;
pub mod pager;

/// Identifies the request as coming from Instagram's web app. Without this
/// header several endpoints answer 401 or 403.
pub const IG_APP_ID: &str = "936619743392459";

pub const BASE_URL: &str = "https://www.instagram.com";
