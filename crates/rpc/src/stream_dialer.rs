//! StreamDialer capability: open outgoing libp2p subprotocol streams to remote peers.
//!
//! The `StreamDialer` capability lets a guest open a libp2p stream to a specific peer
//! on a named subprotocol. The host opens the stream and returns a bidirectional
//! `ByteStream` capability — the guest reads/writes whatever wire protocol it
//! wants directly.

use authority::EpochGuard;
use capnp::capability::Promise;
use capnp_rpc::pry;
use futures::io::AsyncReadExt;
use libp2p::PeerId;
use std::time::Duration;
use tokio::io;
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

use authority::system_capnp;

use super::{ByteStreamImpl, StreamMode};

/// Timeout for establishing the libp2p stream to a remote peer.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

pub struct StreamDialerImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
}

impl StreamDialerImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
        }
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::stream_dialer::Server for StreamDialerImpl {
    fn dial(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::stream_dialer::DialParams,
        mut results: system_capnp::stream_dialer::DialResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let params = pry!(params.get());
        let peer_bytes = pry!(params.get_peer()).to_vec();
        let protocol_str = pry!(pry!(params.get_protocol())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));

        let peer_id = pry!(PeerId::from_bytes(&peer_bytes)
            .map_err(|e| capnp::Error::failed(format!("invalid peer ID: {e}"))));

        let stream_protocol = pry!(super::stream_protocol(protocol_str));

        let mut control = self.stream_control.clone();

        Promise::from_future(async move {
            tracing::debug!(
                peer = %peer_id,
                protocol = %stream_protocol,
                "Dialing stream subprotocol"
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

            // Create a duplex pair: guest_side ↔ host_side.
            // The guest reads/writes via ByteStream RPC on guest_side.
            // The host pumps host_side ↔ libp2p stream.
            // 64 KiB matches the RPC pipe buffer and the listener pump size.
            let (host_side, guest_side) = io::duplex(64 * 1024);

            // Split both sides for bidirectional pumping.
            let (stream_read, stream_write) = Box::pin(stream).split();
            let (mut host_read, mut host_write) = io::split(host_side);

            // Pump: libp2p stream → host_side (remote writes → guest reads)
            tokio::task::spawn_local(async move {
                if let Err(e) = io::copy(&mut stream_read.compat(), &mut host_write).await {
                    tracing::debug!("stream→host pump error: {e}");
                }
            });

            // Pump: host_side → libp2p stream (guest writes → remote reads)
            tokio::task::spawn_local(async move {
                let mut compat_write = stream_write.compat_write();
                if let Err(e) = io::copy(&mut host_read, &mut compat_write).await {
                    tracing::debug!("host→stream pump error: {e}");
                }
            });

            // Wrap guest_side as a bidirectional ByteStream capability.
            let stream_cap: system_capnp::byte_stream::Client =
                capnp_rpc::new_client(ByteStreamImpl::new(guest_side, StreamMode::Bidirectional));
            results.get().set_stream(stream_cap);

            Ok(())
        })
    }
}
