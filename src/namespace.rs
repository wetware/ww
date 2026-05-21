//! Standard namespace CID
//!
//! The IPFS CID for the `ww` standard namespace tree, embedded at build time.
//! Written by `make publish-std` to Cargo's target directory as
//! `std-namespace.cid`; read by build.rs.
//!
//! Empty for local builds (no IPFS needed — HostPathLoader resolves from disk).
//! In release/CI builds, this points at the published IPFS tree containing
//! the Glia stdlib, kernel, shell, and MCP images.

/// IPFS path for the `ww` standard namespace tree (e.g. `/ipfs/bafyrei...`).
///
/// Empty string when built without `make publish-std` (local dev builds).
/// The runtime falls back to HostPathLoader → EmbeddedLoader in that case.
pub const WW_STD_CID: &str = env!("WW_STD_CID");
