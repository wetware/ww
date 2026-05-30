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

pub(crate) fn schema_bytes_for_descriptor_cid(schema_cid: &str) -> Option<&'static [u8]> {
    if schema_cid == membrane::schema_registry::HOST_CID {
        return Some(membrane::schema_registry::HOST_SCHEMA);
    }
    if schema_cid == membrane::schema_registry::RUNTIME_CID {
        return Some(membrane::schema_registry::RUNTIME_SCHEMA);
    }
    if schema_cid == membrane::schema_registry::ROUTING_CID {
        return Some(membrane::schema_registry::ROUTING_SCHEMA);
    }
    if schema_cid == membrane::schema_registry::IDENTITY_CID {
        return Some(membrane::schema_registry::IDENTITY_SCHEMA);
    }
    if schema_cid == membrane::schema_registry::HTTP_CLIENT_CID {
        return Some(membrane::schema_registry::HTTP_CLIENT_SCHEMA);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_schema_lookup_resolves_known_cid() {
        let bytes = schema_bytes_for_descriptor_cid(membrane::schema_registry::HOST_CID)
            .expect("HOST_CID should resolve");
        assert_eq!(bytes, membrane::schema_registry::HOST_SCHEMA);
    }

    #[test]
    fn descriptor_schema_lookup_rejects_unknown_cid() {
        assert!(
            schema_bytes_for_descriptor_cid("bafkr4iunknowncid").is_none(),
            "unknown schema CID must not resolve"
        );
    }

    #[test]
    fn vat_client_dial_schema_lookup_bytes_decode_as_schema_node() {
        let bytes = schema_bytes_for_descriptor_cid(membrane::schema_registry::HOST_CID)
            .expect("HOST_CID should resolve");
        let aligned = crate::graft::bytes_to_aligned_words(bytes);
        let segments: &[&[u8]] = &[capnp::Word::words_to_bytes(&aligned)];
        let segment_array = capnp::message::SegmentArray::new(segments);
        let reader =
            capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
        let _node: capnp::schema_capnp::node::Reader<'_> = reader
            .get_root()
            .expect("lookup bytes should decode as schema.Node");
    }
}

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
        let descriptor = pry!(params.get_descriptor());
        let descriptor_schema_cid_bytes = pry!(descriptor.get_schema_cid()).to_vec();
        let descriptor_schema_cid = match std::str::from_utf8(&descriptor_schema_cid_bytes) {
            Ok(s) if !s.is_empty() => s.to_string(),
            Ok(_) => {
                return Promise::err(capnp::Error::failed(
                    "descriptor.schemaCid must not be empty".into(),
                ))
            }
            Err(e) => {
                return Promise::err(capnp::Error::failed(format!(
                    "descriptor.schemaCid is not utf8: {e}"
                )))
            }
        };
        let descriptor_bytes = pry!(super::canonicalize_vat_descriptor(descriptor));
        if descriptor_bytes.is_empty() {
            return Promise::err(capnp::Error::failed("descriptor must not be empty".into()));
        }
        let schema_bytes =
            if let Some(bytes) = schema_bytes_for_descriptor_cid(&descriptor_schema_cid) {
                bytes.to_vec()
            } else {
                return Promise::err(capnp::Error::failed(format!(
                "descriptor.schemaCid unresolved in local schema registry: {descriptor_schema_cid}"
            )));
            };

        let peer_id = pry!(PeerId::from_bytes(&peer_bytes)
            .map_err(|e| capnp::Error::failed(format!("invalid peer ID: {e}"))));

        let protocol_cid = super::descriptor_cid(&descriptor_bytes);
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

            // Start Cap'n Proto RPC directly on the stream.
            let super::vat_dial::VatDial { bootstrap, driver } =
                super::vat_dial::connect::<_, capnp::capability::Client>(stream);

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
            typed
                .reborrow()
                .init_cap()
                .set_as_capability(bootstrap.hook);
            let aligned = crate::graft::bytes_to_aligned_words(&schema_bytes);
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
