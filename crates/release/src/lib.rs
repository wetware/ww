//! Authenticated release formats and consumer freshness verification.

pub mod auth;
pub mod descriptor;

#[cfg(feature = "package")]
pub mod package;
