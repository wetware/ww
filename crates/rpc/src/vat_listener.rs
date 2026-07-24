//! VatListener capability: publish existing capabilities over Cap'n Proto RPC.
//!
//! The `VatListener` capability publishes a caller-chosen service-name protocol
//! and bootstraps each incoming connection with an already-existing capability.
//! It does not spawn cells or per-connection handlers. Publishers own the
//! served capability's lifecycle.
//!
//! The protocol name is a locator only; authority comes from the capability
//! reference passed to `serve`.

use authority::EpochGuard;
use capnp::capability::Promise;
use capnp_rpc::pry;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use futures::StreamExt;

use crate::ConnectionBudget;
use authority::system_capnp;

pub struct VatListenerImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
    budget: ConnectionBudget,
}

impl VatListenerImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
            budget: ConnectionBudget::default(),
        }
    }

    pub fn with_budget(mut self, budget: ConnectionBudget) -> Self {
        self.budget = budget;
        self
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
        let bootstrap_cap = pry!(params
            .get_cap()
            .get_as_capability::<capnp::capability::Client>());
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
            "published vat service"
        );

        let mut epoch_rx = self.guard.receiver.clone();
        let issued_seq = self.guard.issued_seq;
        let budget = self.budget.clone();
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
                        let permit = match budget.try_acquire() {
                            Ok(permit) => permit,
                            Err(error) => {
                                tracing::warn!(
                                    capacity = error.capacity,
                                    active = budget.active(),
                                    "rejecting vat connection: service connection budget exhausted"
                                );
                                drop(stream);
                                continue;
                            }
                        };
                        let cap = bootstrap_cap.clone();
                        let protocol = protocol_name.clone();
                        tokio::task::spawn_local(async move {
                            let _permit = permit;
                            let _handle_span = tracing::info_span!(
                                "vat.handle",
                                service = protocol.as_str(),
                            ).entered();
                            if let Err(e) = handle_vat_connection_serve(cap, stream, &protocol).await {
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
    bootstrap_cap: capnp::capability::Client,
    stream: impl AsyncRead + AsyncWrite + 'static,
    protocol: &str,
) -> Result<(), capnp::Error> {
    let (reader, writer) = Box::pin(stream).split();
    let network = VatNetwork::new(reader, writer, Side::Server, Default::default());
    let peer_rpc = RpcSystem::new(Box::new(network), Some(bootstrap_cap));

    let _ = peer_rpc.await;
    tracing::debug!(protocol, "vat peer disconnected");

    Ok(())
}
