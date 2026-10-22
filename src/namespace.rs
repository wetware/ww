//! Standard namespace CID
//!
//! The IPFS CID for the `ww` standard namespace tree, embedded at build time.
//! Written by `make publish-std` to Cargo's target directory as
//! `std-namespace.cid`; read by build.rs.
//!
//! Empty for local builds. Installation then publishes its bundled standard
//! components to Kubo. Release/CI builds can supply the published namespace CID
//! as installation bootstrap configuration.

/// IPFS path for the `ww` standard namespace tree (e.g. `/ipfs/bafyrei...`).
///
/// Empty string when built without `make publish-std` (local dev builds).
/// This value configures installation; runtime PID0 selection uses the effective FHS.
pub const WW_STD_CID: &str = env!("WW_STD_CID");
