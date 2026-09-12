//! Membrane server: issues epoch-scoped capabilities via `graft()`.
//!
//! Pure capability provisioning (ocap model): having a Membrane reference IS
//! authorization. For authentication, wrap in `Terminal(Membrane)` — see
//! [`TerminalServer`].

use crate::epoch::{Epoch, EpochGuard};
use crate::system_capnp;
use capnp::capability::{FromClientHook, Promise};
use capnp::Error;
use capnp_rpc::new_client;
use tokio::sync::watch;

/// Look up a typed application capability by name in `Membrane.extras`.
pub fn get_extra<T: FromClientHook>(
    caps: &capnp::struct_list::Reader<'_, system_capnp::export::Owned>,
    name: &str,
) -> Result<T, capnp::Error> {
    for entry in caps.iter() {
        let entry_name = entry
            .get_name()?
            .to_str()
            .map_err(|error| capnp::Error::failed(error.to_string()))?;
        if entry_name == name {
            return entry.get_cap().get_as_capability::<T>();
        }
    }

    Err(capnp::Error::failed(format!(
        "capability '{name}' not found in graft response"
    )))
}

/// Callback trait for populating the graft response with capabilities.
///
/// Implementors receive the EpochGuard and a builder for the graft results,
/// allowing platform-specific capabilities such as network, runtime, and IPFS
/// access to be injected into the response fields.
pub trait GraftBuilder: 'static {
    fn build(
        &self,
        guard: &EpochGuard,
        builder: system_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), Error>;
}

/// Minimal graft builder: sets required stable metadata and withholds all authority.
///
/// Useful for testing or guests that don't need platform capabilities.
pub struct NoExtension {
    peer_id: Vec<u8>,
}

impl NoExtension {
    pub fn new(peer_id: impl Into<Vec<u8>>) -> Self {
        Self {
            peer_id: peer_id.into(),
        }
    }
}

impl GraftBuilder for NoExtension {
    fn build(
        &self,
        _guard: &EpochGuard,
        mut builder: system_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), Error> {
        builder.set_peer_id(&self.peer_id);
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
    fn build_graft(&self, results: &mut system_capnp::membrane::GraftResults) -> Result<(), Error> {
        let epoch = self.get_current_epoch();
        let guard = EpochGuard {
            issued_seq: epoch.seq,
            receiver: self.receiver.clone(),
        };
        let mut builder = results.get();
        self.graft_builder.build(&guard, builder.reborrow())?;
        if !builder.has_peer_id() {
            return Err(Error::failed(
                "Membrane.graft result is missing required peerId".into(),
            ));
        }
        if builder.reborrow().get_peer_id()?.is_empty() {
            return Err(Error::failed(
                "Membrane.graft result contains an empty peerId".into(),
            ));
        }
        Ok(())
    }
}

#[allow(refining_impl_trait)]
impl<F: GraftBuilder> system_capnp::membrane::Server for MembraneServer<F> {
    fn graft(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::membrane::GraftParams,
        mut results: system_capnp::membrane::GraftResults,
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
/// Uses `NoExtension` — only `peerId` is set. All authority fields remain null.
/// For platform-specific graft responses, construct
/// `MembraneServer::new(receiver, your_graft_builder)` directly.
pub fn membrane_client(
    receiver: watch::Receiver<Epoch>,
    peer_id: impl Into<Vec<u8>>,
) -> system_capnp::membrane::Client {
    new_client(MembraneServer::new(receiver, NoExtension::new(peer_id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn test_epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: vec![0xAB, 0xCD],
            root: None,
        }
    }

    #[test]
    fn membrane_server_constructs_with_no_extension() {
        let (_tx, rx) = watch::channel(test_epoch(1));
        let server = MembraneServer::new(rx, NoExtension::new(b"test-peer"));
        let epoch = server.get_current_epoch();
        assert_eq!(epoch.seq, 1);
        assert_eq!(epoch.head, vec![0xAB, 0xCD]);
    }

    #[test]
    fn membrane_server_tracks_epoch_updates() {
        let (tx, rx) = watch::channel(test_epoch(1));
        let server = MembraneServer::new(rx, NoExtension::new(b"test-peer"));
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
            _builder: system_capnp::membrane::graft_results::Builder<'_>,
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

    #[tokio::test(flavor = "current_thread")]
    async fn membrane_server_rejects_missing_peer_id() {
        let (_tx, rx) = watch::channel(test_epoch(1));
        let membrane: system_capnp::membrane::Client = new_client(MembraneServer::new(
            rx,
            RecordingBuilder {
                called: Rc::new(std::cell::Cell::new(false)),
            },
        ));

        let error = match membrane.graft_request().send().promise.await {
            Ok(_) => panic!("missing peerId must reject the graft"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("peerId"));
    }

    struct EmptyPeerIdBuilder;

    impl GraftBuilder for EmptyPeerIdBuilder {
        fn build(
            &self,
            _guard: &EpochGuard,
            mut builder: system_capnp::membrane::graft_results::Builder<'_>,
        ) -> Result<(), capnp::Error> {
            builder.set_peer_id(&[]);
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn membrane_server_rejects_empty_peer_id() {
        let (_tx, rx) = watch::channel(test_epoch(1));
        let membrane: system_capnp::membrane::Client =
            new_client(MembraneServer::new(rx, EmptyPeerIdBuilder));

        let error = match membrane.graft_request().send().promise.await {
            Ok(_) => panic!("empty peerId must reject the graft"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("peerId"));
    }

    /// GraftBuilder that always fails.
    struct FailingBuilder;

    impl GraftBuilder for FailingBuilder {
        fn build(
            &self,
            _guard: &EpochGuard,
            _builder: system_capnp::membrane::graft_results::Builder<'_>,
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
        let _client = membrane_client(rx, b"test-peer");
    }

    #[test]
    fn no_extension_build_sets_only_required_peer_id() {
        let (_tx, rx) = watch::channel(test_epoch(1));
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: rx,
        };
        let mut message = capnp::message::Builder::new_default();
        let builder = message.init_root::<system_capnp::membrane::graft_results::Builder<'_>>();
        let result = NoExtension::new(b"test-peer").build(&guard, builder);
        assert!(result.is_ok());
        let graft = message
            .get_root_as_reader::<system_capnp::membrane::graft_results::Reader<'_>>()
            .expect("minimal graft results");
        assert_eq!(graft.get_peer_id().expect("required peerId"), b"test-peer");
        assert!(!graft.has_stat());
        assert!(!graft.has_network());
        assert!(!graft.has_routing());
        assert!(!graft.has_runtime());
        assert!(!graft.has_authority());
        assert!(!graft.has_identity());
        assert!(!graft.has_ipfs());
        assert!(!graft.has_extras());
    }
}
