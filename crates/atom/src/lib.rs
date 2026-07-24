//! Off-chain Atom runtime: head-following, indexing, and finalization for the Atom contract.
//!
//! - **AtomIndexer**: observed-only indexing of HeadUpdated events (WebSocket + HTTP backfill;
//!   no reorg safety or confirmations in the indexer itself).
//! - **Finalizer**: consumes indexer output and emits only events that are eligible per a
//!   configurable [Strategy] (e.g. [ConfirmationDepth]) and pass the canonical cross-check
//!   (`Atom.head()`), giving reorg-safe finalized output.

pub use authority::auth_capnp;
pub use authority::membrane_capnp;
pub use authority::stem_capnp;
pub use authority::system_capnp;
pub use authority::{
    membrane_client, Epoch, EpochGuard, GraftBuilder, MembraneServer, NoExtension, TerminalServer,
};

pub mod abi;
pub mod config;
pub mod cursor;
pub mod finalizer;
pub mod indexer;

pub use abi::{CurrentHead, HeadUpdatedObserved};
pub use config::{IndexerConfig, ReconnectionConfig};
pub use cursor::Cursor;
pub use finalizer::{
    ConfirmationDepth, FinalizedEvent, Finalizer, FinalizerBuilder, FinalizerError, Strategy,
};
pub use indexer::{current_block_number, AtomIndexer};

/// Current head state (alias for ABI CurrentHead).
pub type Head = CurrentHead;

#[cfg(test)]
mod tests {
    #[test]
    fn stub() {}
}
