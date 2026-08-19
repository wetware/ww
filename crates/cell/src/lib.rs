//! Cell runtime implementation for Wetware
//!
//! This module provides cell execution capabilities using Wasmtime, supporting
//! per-stream instantiation with duplex pipe communication.

use anyhow::Result;
use async_trait::async_trait;

pub mod engine;
pub mod epoch;
pub mod fs_intercept;
pub mod image;
pub mod loaders;
pub mod mount;
pub mod proc;
pub mod sched;
pub mod vfs;

pub use proc::{Builder, Proc, Program};

/// Trait for loading bytecode from various sources (IPFS, filesystem, etc.)
///
/// This allows the cell package to be agnostic about how bytecode is resolved,
/// following the Go pattern where packages declare interfaces and callers
/// provide implementations.
#[async_trait]
pub trait Loader: Send + Sync {
    /// Load bytecode from the given path
    ///
    /// The path can be an IPFS path (/ipfs/, /ipns/, /ipld/), filesystem path,
    /// or any other format supported by the implementation.
    async fn load(&self, path: &str) -> Result<Vec<u8>>;
}
