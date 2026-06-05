//! VatClient capability: open outgoing Cap'n Proto RPC connections to remote peers.
//!
//! Dialing returns a trusted host-side `VatConnection` wrapper. The caller can
//! inspect metadata with `describe()` or acquire the application capability with
//! `bind()`.

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
        let protocol_reader = pry!(params.get_protocol());
        let protocol = pry!(protocol_reader
            .to_str()
            .map_err(|e| capnp::Error::failed(format!("vat protocol is not UTF-8: {e}"))))
        .to_string();

        let peer_id = pry!(PeerId::from_bytes(&peer_bytes)
            .map_err(|e| capnp::Error::failed(format!("invalid peer ID: {e}"))));

        let stream_protocol = pry!(super::vat_protocol(&protocol));
        let mut control = self.stream_control.clone();

        Promise::from_future(async move {
            tracing::debug!(
                peer = %peer_id,
                protocol = %stream_protocol,
                "Dialing vat service"
            );

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

            let super::vat_dial::VatDial { bootstrap, driver } =
                super::vat_dial::connect::<_, system_capnp::vat_connection::Client>(stream);

            let driver_peer = peer_id;
            let driver_protocol = stream_protocol.clone();
            tokio::task::spawn_local(async move {
                match driver.await {
                    Ok(Ok(())) => tracing::debug!(
                        peer = %driver_peer,
                        protocol = %driver_protocol,
                        "Vat dial session ended cleanly"
                    ),
                    Ok(Err(e)) => tracing::warn!(
                        peer = %driver_peer,
                        protocol = %driver_protocol,
                        "Vat dial session ended with error: {e}"
                    ),
                    Err(e) => tracing::warn!(
                        peer = %driver_peer,
                        protocol = %driver_protocol,
                        "Vat dial driver task aborted: {e}"
                    ),
                }
            });

            results.get().set_connection(bootstrap);

            Ok(())
        })
    }
}
