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
        let schema_bytes = pry!(params.get_schema()).to_vec();

        if schema_bytes.is_empty() {
            return Promise::err(capnp::Error::failed("schema must not be empty".into()));
        }
        let schema_bytes = pry!(super::canonicalize_schema_bytes(&schema_bytes));

        let peer_id = pry!(PeerId::from_bytes(&peer_bytes)
            .map_err(|e| capnp::Error::failed(format!("invalid peer ID: {e}"))));

        let protocol_cid = super::schema_cid(&schema_bytes);
        let stream_protocol = pry!(super::schema_protocol(&protocol_cid));

        let mut control = self.stream_control.clone();

        Promise::from_future(async move {
            tracing::debug!(
                peer = %peer_id,
                protocol = %stream_protocol,
                "Dialing vat subprotocol"
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

            // Verify producer-sourced schema attestation first, then start
            // Cap'n Proto RPC on the same stream.
            let (
                super::vat_dial::VatDial { bootstrap, driver },
                attested_schema,
            ) = super::vat_dial::connect_with_schema_attestation::<_, capnp::capability::Client>(
                stream,
                &protocol_cid,
            )
            .await?;

            // The driver runs detached. Cap'n Proto refcounting handles
            // shutdown: when the guest drops all capabilities obtained from
            // this connection, the RpcSystem drains and the task completes.
            // We log the eventual RpcSystem outcome for observability.
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

            let mut typed = results.get().init_typed();
            typed.reborrow().init_cap().set_as_capability(bootstrap.hook);
            let aligned = crate::graft::bytes_to_aligned_words(&attested_schema);
            let segments: &[&[u8]] = &[capnp::Word::words_to_bytes(&aligned)];
            let segment_array = capnp::message::SegmentArray::new(segments);
            let reader =
                capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
            let schema_node: capnp::schema_capnp::node::Reader<'_> = reader.get_root()?;
            let mut out_schema = typed.reborrow().init_schema();
            out_schema.set_root(schema_node)?;
            out_schema.init_deps(0);

            Ok(())
        })
    }
}
