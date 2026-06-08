//! VatListener capability: publish existing capabilities over Cap'n Proto RPC.
//!
//! The `VatListener` capability registers a caller-chosen service-name protocol
//! and bootstraps each incoming connection with an already-existing capability.
//! It does not spawn cells. Publishers own the served capability's lifecycle.
//!
//! The protocol name is a locator only; authority comes from the capability
//! reference passed to `serve`.

use capnp::capability::Promise;
use capnp_rpc::pry;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use futures::StreamExt;
use membrane::EpochGuard;

use membrane::system_capnp;

use crate::synapse_abi::{read_owned_synapse, BootstrapServer, OwnedSynapse};

pub struct VatListenerImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
}

impl VatListenerImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
        }
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_listener::Server for VatListenerImpl {
    fn serve(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_listener::ServeParams,
        _results: system_capnp::vat_listener::ServeResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let params = pry!(params.get());
        let bootstrap_synapse: OwnedSynapse =
            pry!(params.get_synapse().and_then(read_owned_synapse));
        let protocol = pry!(pry!(params.get_protocol())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));
        let protocol_name = protocol.to_string();
        let stream_protocol = pry!(super::vat_protocol(&protocol_name));

        let mut control = self.stream_control.clone();
        let mut incoming =
            pry!(control
                .accept(stream_protocol.clone())
                .map_err(|e| capnp::Error::failed(format!(
                    "failed to register vat service '{protocol_name}': {e}"
                ))));

        tracing::info!(
            protocol = %stream_protocol,
            service = %protocol_name,
            "registered vat service"
        );

        let mut epoch_rx = self.guard.receiver.clone();
        let issued_seq = self.guard.issued_seq;
        tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    conn = incoming.next() => {
                        let Some((peer_id, stream)) = conn else {
                            tracing::warn!(protocol = %stream_protocol, "vat accept loop ended unexpectedly");
                            break;
                        };
                        let _accept_span = tracing::info_span!(
                            "vat.accept",
                            peer = %peer_id,
                            protocol = %stream_protocol,
                            service = %protocol_name,
                        ).entered();
                        tracing::debug!("incoming vat connection");
                        let synapse = bootstrap_synapse.clone();
                        let protocol = protocol_name.clone();
                        tokio::task::spawn_local(async move {
                            let _handle_span = tracing::info_span!(
                                "vat.handle",
                                service = protocol.as_str(),
                            ).entered();
                            if let Err(e) = handle_vat_connection_serve(synapse, stream, &protocol).await {
                                tracing::error!("vat service connection error: {e}");
                            }
                        });
                    }
                    _ = epoch_rx.changed() => {
                        if epoch_rx.borrow().seq != issued_seq {
                            tracing::warn!(
                                protocol = %stream_protocol,
                                "epoch became stale, closing vat accept loop"
                            );
                            break;
                        }
                    }
                }
            }
        });

        Promise::ok(())
    }
}

/// Bootstrap one remote peer with a persistent capability.
///
/// Generic over stream type for testability.
pub async fn handle_vat_connection_serve(
    bootstrap_synapse: OwnedSynapse,
    stream: impl AsyncRead + AsyncWrite + 'static,
    protocol: &str,
) -> Result<(), capnp::Error> {
    let (reader, writer) = Box::pin(stream).split();
    let network = VatNetwork::new(reader, writer, Side::Server, Default::default());
    let bootstrap: membrane::synapse_capnp::bootstrap::Client =
        capnp_rpc::new_client(BootstrapServer::new(bootstrap_synapse));
    let peer_rpc = RpcSystem::new(Box::new(network), Some(bootstrap.client));

    let _ = peer_rpc.await;
    tracing::debug!(protocol, "vat peer disconnected");

    Ok(())
}
