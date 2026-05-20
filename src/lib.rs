//! Wetware - P2P sandbox for Web3 applications
//!
//! This library provides cell execution capabilities using Wasmtime, supporting
//! per-stream instantiation with duplex pipe communication.

// Host-only modules (not available for WASM guests)
#[cfg(not(target_arch = "wasm32"))]
pub use cell;
#[cfg(not(target_arch = "wasm32"))]
pub mod daemon_config;
#[cfg(not(target_arch = "wasm32"))]
pub mod discovery;
#[cfg(not(target_arch = "wasm32"))]
pub mod dispatcher;
#[cfg(not(target_arch = "wasm32"))]
pub mod executor;
#[cfg(not(target_arch = "wasm32"))]
pub mod host;
#[cfg(not(target_arch = "wasm32"))]
pub use ipfs;
#[cfg(not(target_arch = "wasm32"))]
pub mod launcher;
#[cfg(not(target_arch = "wasm32"))]
pub mod metrics;
#[cfg(not(target_arch = "wasm32"))]
pub mod ns;
#[cfg(not(target_arch = "wasm32"))]
pub use rpc;
#[cfg(not(target_arch = "wasm32"))]
pub use rpc::keys;
#[cfg(not(target_arch = "wasm32"))]
pub mod services;

// Re-export capnp schema modules from the membrane crate so host code can
// use `crate::system_capnp`, `crate::routing_capnp`, `crate::stem_capnp`, etc.
#[cfg(not(target_arch = "wasm32"))]
pub use membrane::cell_capnp;
#[cfg(not(target_arch = "wasm32"))]
pub use membrane::http_capnp;
#[cfg(not(target_arch = "wasm32"))]
pub use membrane::routing_capnp;
#[cfg(not(target_arch = "wasm32"))]
pub use membrane::stem_capnp;
#[cfg(not(target_arch = "wasm32"))]
pub use membrane::system_capnp;

// Example schemas compiled by build.rs for integration tests.
#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
pub mod greeter_capnp {
    include!(concat!(env!("OUT_DIR"), "/greeter_capnp.rs"));
}
#[allow(dead_code)]
pub mod shell_capnp {
    include!(concat!(env!("OUT_DIR"), "/shell_capnp.rs"));
}

// Modules available for both host and guest
pub mod config;
pub mod default_kernel;
pub mod namespace;

// Re-export the Glia language crate
pub use glia;

// Re-export commonly used types for convenience
#[cfg(not(target_arch = "wasm32"))]
pub use cell::{Loader, Proc, ProcBuilder};
#[cfg(not(target_arch = "wasm32"))]
pub use executor::{Cell, CellBuilder, SpawnResult};
