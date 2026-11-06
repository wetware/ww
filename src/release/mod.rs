//! Shared authenticated release consumption.
//!
//! `auth` and `descriptor` remain available to installer and Host-upgrade
//! consumers. The infrastructure-only `package.rs` and `publisher.rs` files in
//! this directory are private modules of `ww-release-publisher`; the Host
//! library does not compile them. `ipfs::release::Owner` remains in the IPFS crate.

pub mod auth;
pub mod descriptor;
