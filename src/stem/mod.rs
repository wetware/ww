//! Mutable deployment-reference sources.
//!
//! A [`Source`] applies backend consistency rules before it emits an
//! authoritative [`Update`]. Backend revision data remains private to the
//! adapter and never becomes `authority::Epoch.seq`.

use anyhow::Result;
use async_trait::async_trait;
use cid::Cid;

pub mod atom;

/// One deployable authoritative Stem head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Head {
    pub cid: Cid,
}

impl Head {
    pub fn bytes(&self) -> Vec<u8> {
        self.cid.to_bytes()
    }
}

/// Evidence for an authoritative Stem head that cannot select a deployment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidHead {
    pub selected: Vec<u8>,
    pub reason: String,
}

/// An authoritative Stem state change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Update {
    Head(Head),
    InvalidHead(InvalidHead),
}

/// A running backend adapter for one configured Stem.
#[async_trait]
pub trait Source: Send {
    /// Establish the current authoritative baseline.
    async fn current(&mut self) -> Result<Update>;

    /// Return the next authoritative state that differs from the last update.
    ///
    /// Implementations must be cancel-safe. Dropping an in-flight call must
    /// not lose a backend update that the adapter already accepted.
    async fn next(&mut self) -> Result<Update>;
}
