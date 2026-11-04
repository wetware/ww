//! VatClient capability: open outgoing Cap'n Proto RPC connections to remote peers.
//!
//! The `VatClient` capability lets a guest dial a remote peer on a named
//! subprotocol and receive the remote's bootstrap capability directly. The
//! host opens the libp2p stream, bootstraps a Cap'n Proto vat over it, and
//! returns the remote's exported capability to the guest.
//!
//! This is the capability-mode counterpart of `StreamDialer` (byte-stream mode).

use std::time::Duration;

use authority::EpochGuard;
use capnp::capability::Promise;
use capnp_rpc::pry;
use libp2p::PeerId;

use authority::system_capnp;

/// Timeout for establishing the libp2p stream to a remote peer.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

pub struct VatClientImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
    outbound: Option<crate::outbound::OutboundRegistry>,
}

impl VatClientImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
            outbound: None,
        }
    }

    pub fn with_outbound_registry(mut self, registry: crate::outbound::OutboundRegistry) -> Self {
        self.outbound = Some(registry);
        self
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_client::Server for VatClientImpl {
    fn dial(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_client::DialParams,
        mut results: system_capnp::vat_client::DialResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let origin = params.hook.invocation_origin();
        let params = pry!(params.get());
        let peer_bytes = pry!(params.get_peer()).to_vec();
        let protocol = pry!(pry!(params.get_protocol())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));
        let peer_id = pry!(PeerId::from_bytes(&peer_bytes)
            .map_err(|e| capnp::Error::failed(format!("invalid peer ID: {e}"))));

        let protocol_name = protocol.to_string();
        let stream_protocol = pry!(super::vat_protocol(&protocol_name));

        // Preserve epoch and argument validation errors, then require a trusted
        // invocation owner before starting any transport work.
        if origin.is_none() {
            return Promise::err(capnp::Error::failed("missing invocation origin".into()));
        }
        let registry = pry!(self
            .outbound
            .as_ref()
            .ok_or_else(|| capnp::Error::failed("outbound ownership registry unavailable".into())));
        let dial = pry!(super::vat_dial::connect_libp2p_owned_logged::<
            capnp::capability::Client,
        >(
            registry,
            origin,
            self.stream_control.clone(),
            peer_id,
            stream_protocol.clone(),
            Some(protocol_name),
        ));
        Promise::from_future(async move {
            // This bounds the caller's wait. The supervised native attempt keeps
            // its permit until the backend actually returns, even after timeout.
            let bootstrap = tokio::time::timeout(DIAL_TIMEOUT, dial)
                .await
                .map_err(|_| {
                    capnp::Error::failed(format!(
                        "timeout dialing {peer_id} on {stream_protocol} after {DIAL_TIMEOUT:?}"
                    ))
                })??;

            // `bootstrap` is the remote peer's served capability directly
            // (the vat bootstrap slot carries the cap; no Synapse shim).
            results.get().init_cap().set_as_capability(bootstrap.hook);

            Ok(())
        })
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn valid_unattributed_dial_rejects_before_transport_work() {
        tokio::task::LocalSet::new().run_until(async {
            let behaviour = libp2p_stream::Behaviour::new();
            let client: system_capnp::vat_client::Client = capnp_rpc::new_client(
                VatClientImpl::new(behaviour.new_control(), EpochGuard::fixed(authority::Epoch::zero()))
            );
            let mut request = client.dial_request();
            request.get().set_peer(&PeerId::random().to_bytes());
            request.get().set_protocol("ownership-test");
            let result = tokio::time::timeout(Duration::from_millis(100), request.send().promise).await;
            assert!(matches!(result, Ok(Err(error)) if error.extra.contains("missing invocation origin")),
                "valid ownerless dial must reject before opening a transport");
        }).await;
    }
}
