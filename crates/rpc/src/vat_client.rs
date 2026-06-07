//! VatClient capability: open outgoing Cap'n Proto RPC connections to remote peers.
//!
//! The `VatClient` capability lets a guest dial a remote peer on a named
//! subprotocol and receive the remote's bootstrap capability directly. The
//! host opens the libp2p stream, bootstraps a Cap'n Proto vat over it, and
//! returns the remote's exported capability to the guest.
//!
//! This is the capability-mode counterpart of `StreamDialer` (byte-stream mode).

use std::time::Duration;

use capnp::capability::Promise;
use capnp_rpc::pry;
use libp2p::PeerId;
use membrane::EpochGuard;

use membrane::system_capnp;

/// Timeout for establishing the libp2p stream to a remote peer.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

pub struct VatClientImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
}

impl VatClientImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
        }
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

        let params = pry!(params.get());
        let peer_bytes = pry!(params.get_peer()).to_vec();
        let protocol = pry!(pry!(params.get_protocol())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));
        let peer_id = pry!(PeerId::from_bytes(&peer_bytes)
            .map_err(|e| capnp::Error::failed(format!("invalid peer ID: {e}"))));

        let protocol_name = protocol.to_string();
        let stream_protocol = pry!(super::vat_protocol(&protocol_name));

        let mut control = self.stream_control.clone();

        Promise::from_future(async move {
            tracing::debug!(
                peer = %peer_id,
                protocol = %stream_protocol,
                service = %protocol_name,
                "dialing vat service"
            );

            // Open stream with timeout to avoid hanging on unreachable peers.
            let stream = tokio::time::timeout(
                DIAL_TIMEOUT,
                control.open_stream(peer_id, stream_protocol.clone()),
            )
            .await
            .map_err(|_| {
                capnp::Error::failed(format!(
                    "timeout dialing {peer_id} on {stream_protocol} after {DIAL_TIMEOUT:?}"
                ))
            })?
            .map_err(|e| {
                capnp::Error::failed(format!(
                    "failed to open stream to {peer_id} on {stream_protocol}: {e}"
                ))
            })?;

            // Bootstrap Cap'n Proto RPC over the libp2p stream via the
            // paved-path helper, which spawns the RpcSystem driver before
            // returning. The driver flushes Bootstrap and receives the
            // remote Return on its own.
            //
            // We don't await an explicit handshake check: `when_resolved()`
            // on a bootstrap pipeline client doesn't fire reliably in
            // capnp-rpc-rust 0.25 (see vat_dial docs).  The guest's first
            // method call through the returned cap observes any remote
            // failure via that call's own response timeout.
            let super::vat_dial::VatDial { bootstrap, driver } =
                super::vat_dial::connect::<_, capnp::capability::Client>(stream);

            // The driver runs detached. Cap'n Proto refcounting handles
            // shutdown: when the guest drops all capabilities obtained from
            // this connection, the RpcSystem drains and the task completes.
            // We log the eventual RpcSystem outcome for observability.
            let driver_peer = peer_id;
            let driver_protocol = stream_protocol.clone();
            let driver_service = protocol_name.clone();
            tokio::task::spawn_local(async move {
                match driver.await {
                    Ok(Ok(())) => tracing::debug!(
                        peer = %driver_peer,
                        protocol = %driver_protocol,
                        service = %driver_service,
                        "vat dial session ended cleanly"
                    ),
                    Ok(Err(e)) => tracing::warn!(
                        peer = %driver_peer,
                        protocol = %driver_protocol,
                        service = %driver_service,
                        "vat dial session ended with error: {e}"
                    ),
                    Err(e) => tracing::warn!(
                        peer = %driver_peer,
                        protocol = %driver_protocol,
                        service = %driver_service,
                        "vat dial driver task aborted: {e}"
                    ),
                }
            });

            results.get().init_cap().set_as_capability(bootstrap.hook);

            Ok(())
        })
    }
}
