//! Membrane server: issues epoch-scoped capabilities via `graft()`.
//!
//! Pure capability provisioning (ocap model): having a Membrane reference IS
//! authorization. For authentication, wrap in `Terminal(Membrane)` — see
//! [`TerminalServer`].

use crate::epoch::{Epoch, EpochGuard};
use crate::stem_capnp;
use capnp::capability::Promise;
use capnp::Error;
use capnp_rpc::new_client;
use tokio::sync::watch;

/// Callback trait for populating the graft response with capabilities.
///
/// Implementors receive the EpochGuard and a builder for the graft results,
/// allowing platform-specific capabilities (Host, Executor, IPFS) to be
/// injected into the response fields.
pub trait GraftBuilder: 'static {
    fn build(
        &self,
        guard: &EpochGuard,
        builder: stem_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), Error>;
}

/// No-op graft builder: leaves all result fields empty.
///
/// Useful for testing or guests that don't need platform capabilities.
pub struct NoExtension;

impl GraftBuilder for NoExtension {
    fn build(
        &self,
        _guard: &EpochGuard,
        _builder: stem_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), Error> {
        Ok(())
    }
}

/// Membrane server: stable across epochs, backed by a watch receiver for the adopted epoch.
///
/// The `graft_builder` callback fills the result fields when `graft()` is called.
/// No authentication — having a reference to the Membrane IS authorization (ocap).
/// To gate access, wrap in [`TerminalServer`].
pub struct MembraneServer<F: GraftBuilder> {
    receiver: watch::Receiver<Epoch>,
    graft_builder: F,
}

impl<F: GraftBuilder> MembraneServer<F> {
    pub fn new(receiver: watch::Receiver<Epoch>, graft_builder: F) -> Self {
        Self {
            receiver,
            graft_builder,
        }
    }

    fn get_current_epoch(&self) -> Epoch {
        self.receiver.borrow().clone()
    }

    /// Build epoch-guarded capabilities into the graft results.
    fn build_graft(&self, results: &mut stem_capnp::membrane::GraftResults) -> Result<(), Error> {
        let epoch = self.get_current_epoch();
        let guard = EpochGuard {
            issued_seq: epoch.seq,
            receiver: self.receiver.clone(),
        };
        self.graft_builder.build(&guard, results.get())
    }
}

#[allow(refining_impl_trait)]
impl<F: GraftBuilder> stem_capnp::membrane::Server for MembraneServer<F> {
    fn graft(
        self: capnp::capability::Rc<Self>,
        _params: stem_capnp::membrane::GraftParams,
        mut results: stem_capnp::membrane::GraftResults,
    ) -> Promise<(), Error> {
        tracing::debug!("Membrane graft() called");
        if let Err(e) = self.build_graft(&mut results) {
            return Promise::err(e);
        }
        tracing::debug!("Membrane graft() completed");
        Promise::ok(())
    }
}

/// Builds a Membrane capability client from a watch receiver (for use over capnp-rpc).
///
/// Uses `NoExtension` — all result fields are left empty (null capabilities).
/// For platform-specific graft responses, construct
/// `MembraneServer::new(receiver, your_graft_builder)` directly.
pub fn membrane_client(receiver: watch::Receiver<Epoch>) -> stem_capnp::membrane::Client {
    new_client(MembraneServer::new(receiver, NoExtension))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn test_epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: vec![0xAB, 0xCD],
            provenance: crate::epoch::Provenance::Block(42),
        }
    }

    #[test]
    fn membrane_server_constructs_with_no_extension() {
        let (_tx, rx) = watch::channel(test_epoch(1));
        let server = MembraneServer::new(rx, NoExtension);
        let epoch = server.get_current_epoch();
        assert_eq!(epoch.seq, 1);
        assert_eq!(epoch.head, vec![0xAB, 0xCD]);
        assert_eq!(epoch.provenance, crate::epoch::Provenance::Block(42));
    }

    #[test]
    fn membrane_server_tracks_epoch_updates() {
        let (tx, rx) = watch::channel(test_epoch(1));
        let server = MembraneServer::new(rx, NoExtension);
        assert_eq!(server.get_current_epoch().seq, 1);

        tx.send(test_epoch(2)).unwrap();
        assert_eq!(server.get_current_epoch().seq, 2);

        tx.send(test_epoch(5)).unwrap();
        assert_eq!(server.get_current_epoch().seq, 5);
    }

    /// Custom GraftBuilder that records whether build() was called.
    struct RecordingBuilder {
        called: Rc<std::cell::Cell<bool>>,
    }

    impl GraftBuilder for RecordingBuilder {
        fn build(
            &self,
            _guard: &EpochGuard,
            _builder: stem_capnp::membrane::graft_results::Builder<'_>,
        ) -> Result<(), capnp::Error> {
            self.called.set(true);
            Ok(())
        }
    }

    #[test]
    fn membrane_server_constructs_with_custom_graft_builder() {
        let (_tx, rx) = watch::channel(test_epoch(1));
        let called = Rc::new(std::cell::Cell::new(false));
        let builder = RecordingBuilder {
            called: Rc::clone(&called),
        };
        let _server = MembraneServer::new(rx, builder);
        assert!(
            !called.get(),
            "graft builder should not run during server construction"
        );
    }

    /// GraftBuilder that always fails.
    struct FailingBuilder;

    impl GraftBuilder for FailingBuilder {
        fn build(
            &self,
            _guard: &EpochGuard,
            _builder: stem_capnp::membrane::graft_results::Builder<'_>,
        ) -> Result<(), capnp::Error> {
            Err(capnp::Error::failed("intentional failure".into()))
        }
    }

    #[test]
    fn membrane_server_constructs_with_failing_builder() {
        let (_tx, rx) = watch::channel(test_epoch(1));
        let _server = MembraneServer::new(rx, FailingBuilder);
    }

    #[test]
    fn membrane_client_constructs_without_panic() {
        let (_tx, rx) = watch::channel(test_epoch(1));
        let _client = membrane_client(rx);
    }

    #[test]
    fn no_extension_build_succeeds() {
        let (_tx, rx) = watch::channel(test_epoch(1));
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: rx,
        };
        let mut message = capnp::message::Builder::new_default();
        let builder = message.init_root::<stem_capnp::membrane::graft_results::Builder<'_>>();
        let result = NoExtension.build(&guard, builder);
        assert!(result.is_ok());
    }
}
