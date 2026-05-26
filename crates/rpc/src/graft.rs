//! Membrane-based RPC bootstrap: epoch-scoped Host + Executor + node identity capabilities.
//!
//! Instead of bootstrapping a bare `Host`, the membrane's `graft()` returns
//! epoch-scoped `Host`, `Executor`, and a node `identity` signer directly as
//! result fields. All capabilities fail with `staleEpoch` when the epoch
//! advances.
//!
//! The `membrane` crate owns the Membrane server and epoch machinery.
//! This module provides the `GraftBuilder` impl that injects wetware-specific
//! capabilities into the graft response, plus the epoch-guarded identity wrapper.

use std::sync::Arc;

use capnp::capability::Promise;
use capnp_rpc::pry;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use libp2p::identity::Keypair;
use libp2p_core::SignedEnvelope;
use membrane::{stem_capnp, Epoch, EpochGuard, GraftBuilder, MembraneServer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::SwarmCommand;
use auth::SigningDomain;
use membrane::http_capnp;
use membrane::routing_capnp;
use membrane::system_capnp;

use super::NetworkState;

// ---------------------------------------------------------------------------
// EpochGuardedIdentity — host-side node identity hub
// ---------------------------------------------------------------------------

/// Host-side node identity hub provided to the kernel through the Session.
///
/// **Security invariant**: the identity secret key never leaves the host process.
/// The key is never copied into WASM memory or transmitted over the RPC channel.
/// The kernel receives only a capability reference; all signing happens host-side,
/// and the kernel's WASM sandbox cannot observe or extract the private key bytes.
///
/// Epoch-guarded: the hub and all domain signers it issues fail with `staleEpoch`
/// once the epoch advances.
///
/// Incoming domain strings are accepted if non-empty — the guest chooses
/// the signing context. Empty domains are rejected with an RPC error.
struct EpochGuardedIdentity {
    /// Pre-converted libp2p keypair (Ed25519 → Keypair done once at session construction).
    keypair: Keypair,
    guard: EpochGuard,
}

impl EpochGuardedIdentity {
    fn new(keypair: Keypair, guard: EpochGuard) -> Self {
        Self { keypair, guard }
    }
}

#[allow(refining_impl_trait)]
impl stem_capnp::identity::Server for EpochGuardedIdentity {
    fn signer(
        self: capnp::capability::Rc<Self>,
        params: stem_capnp::identity::SignerParams,
        mut results: stem_capnp::identity::SignerResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let domain_reader = pry!(pry!(params.get()).get_domain());
        let domain_str = pry!(domain_reader
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));
        if domain_str.is_empty() {
            return Promise::err(capnp::Error::failed(
                "signing domain must not be empty".into(),
            ));
        }
        // Accept any non-empty domain — the guest chooses the signing context.
        // The domain string is opaque to the host; it just constructs the
        // domain-separated signing buffer using whatever the guest requested.
        let domain = SigningDomain::new(domain_str);
        let signer: stem_capnp::signer::Client = capnp_rpc::new_client(EpochGuardedDomainSigner {
            domain,
            keypair: self.keypair.clone(),
            guard: self.guard.clone(),
        });
        results.get().set_signer(signer);
        Promise::ok(())
    }

    fn verify(
        self: capnp::capability::Rc<Self>,
        params: stem_capnp::identity::VerifyParams,
        mut results: stem_capnp::identity::VerifyResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let params = pry!(params.get());
        let data = pry!(params.get_data());
        let signature_bytes = pry!(params.get_signature());
        let pubkey_bytes = pry!(params.get_pubkey());

        // Parse the public key (32 bytes for Ed25519).
        let pubkey_arr: [u8; 32] = match pubkey_bytes.try_into() {
            Ok(arr) => arr,
            Err(_) => {
                return Promise::err(capnp::Error::failed("pubkey must be 32 bytes".into()));
            }
        };
        let pubkey = match VerifyingKey::from_bytes(&pubkey_arr) {
            Ok(key) => key,
            Err(_) => {
                results.get().set_valid(false);
                return Promise::ok(());
            }
        };

        // Parse the signature (64 bytes for Ed25519).
        let sig_arr: [u8; 64] = match signature_bytes.try_into() {
            Ok(arr) => arr,
            Err(_) => {
                return Promise::err(capnp::Error::failed("signature must be 64 bytes".into()));
            }
        };
        let signature = Signature::from_bytes(&sig_arr);

        // Verify with strict validation (rejects malleable signatures).
        let valid = pubkey.verify_strict(data, &signature).is_ok();
        results.get().set_valid(valid);
        Promise::ok(())
    }
}

// ---------------------------------------------------------------------------
// EpochGuardedDomainSigner — domain-scoped signer
// ---------------------------------------------------------------------------

/// Signs nonces for a specific [`SigningDomain`] (e.g. `terminal_membrane`, `membrane_graft`).
///
/// Constructed by [`EpochGuardedIdentity::signer()`] after validating the
/// requested domain.  Returns a protobuf-encoded `libp2p_core::SignedEnvelope`.
struct EpochGuardedDomainSigner {
    domain: SigningDomain,
    keypair: Keypair,
    guard: EpochGuard,
}

// ---------------------------------------------------------------------------
// EpochGuardedIpfs — daemon-side IPFS read proxy for non-WASI clients
// ---------------------------------------------------------------------------

struct EpochGuardedIpfs {
    guard: EpochGuard,
    ipfs_client: ipfs::HttpClient,
}

// Keep a conservative ceiling well below common Cap'n Proto default traversal
// limits (~64 MiB) so oversize reads fail with a clear contract error rather
// than an opaque decode failure on the client side.
const MAX_IPFS_READ_BYTES: usize = 32 * 1024 * 1024;

#[allow(refining_impl_trait)]
impl system_capnp::ipfs::Server for EpochGuardedIpfs {
    fn read(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::ipfs::ReadParams,
        mut results: system_capnp::ipfs::ReadResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let p = pry!(params.get());
        let path = pry!(p
            .get_path()
            .and_then(|t| t.to_str().map_err(|e| capnp::Error::failed(e.to_string()))))
        .to_string();

        if !ipfs::is_ipfs_path(&path) {
            return Promise::err(capnp::Error::failed(format!(
                "ipfs.read: expected /ipfs/, /ipns/, or /ipld/ path; got {path}"
            )));
        }

        let client = self.ipfs_client.clone();
        Promise::from_future(async move {
            let bytes = client
                .cat(&path)
                .await
                .map_err(|e| capnp::Error::failed(format!("ipfs.read failed: {e}")))?;
            if bytes.len() > MAX_IPFS_READ_BYTES {
                return Err(capnp::Error::failed(format!(
                    "ipfs.read: payload too large ({} bytes > {} bytes max); use smaller objects or a streaming API",
                    bytes.len(),
                    MAX_IPFS_READ_BYTES
                )));
            }
            results.get().set_data(&bytes);
            Ok(())
        })
    }
}

#[allow(refining_impl_trait)]
impl stem_capnp::signer::Server for EpochGuardedDomainSigner {
    fn sign(
        self: capnp::capability::Rc<Self>,
        params: stem_capnp::signer::SignParams,
        mut results: stem_capnp::signer::SignResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let p = pry!(params.get());
        let nonce = p.get_nonce();
        let epoch_seq = p.get_epoch_seq();
        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&nonce.to_be_bytes());
        payload.extend_from_slice(&epoch_seq.to_be_bytes());
        let envelope = pry!(SignedEnvelope::new(
            &self.keypair,
            self.domain.as_str().to_string(),
            self.domain.payload_type().to_vec(),
            payload,
        )
        .map_err(|e| capnp::Error::failed(e.to_string())));
        results.get().set_sig(&envelope.into_protobuf_encoding());
        Promise::ok(())
    }
}

// ---------------------------------------------------------------------------
// HostGraftBuilder — GraftBuilder for the concrete stem graft response
// ---------------------------------------------------------------------------

/// Fills the graft response with epoch-guarded Host, Runtime, Routing, HttpClient, and node identity.
///
/// **Runtime singleton**: the builder holds a pre-created `runtime::Client` that
/// points to a single `RuntimeImpl` backend. Every graft clones this client, so
/// all cells (including children) share the same compilation/executor cache.
pub struct HostGraftBuilder {
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    wasm_debug: bool,
    signing_key: Option<Arc<SigningKey>>,
    stream_control: libp2p_stream::Control,
    allowed_hosts: Vec<String>,
    route_registry: Option<crate::dispatch::RouteRegistry>,
    /// Pre-created Runtime client (singleton — same backend for every graft).
    runtime_client: system_capnp::runtime::Client,
    /// Named capabilities from init.d `with` block, forwarded to the child
    /// cell's graft response as `extras`. Each entry carries the cap name,
    /// the typed client, and the canonical Schema.Node bytes the original
    /// caller observed on the wire (or an empty Vec if the caller didn't
    /// have schema bytes — graft will then leave `Export.schema` empty and
    /// emit a warning).
    extras: Vec<(String, capnp::capability::Client, Vec<u8>)>,
    /// IPFS HTTP client for Kubo API calls (e.g. IPNS resolution).
    ipfs_client: ipfs::HttpClient,
}

impl HostGraftBuilder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        network_state: NetworkState,
        swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
        wasm_debug: bool,
        signing_key: Option<Arc<SigningKey>>,
        stream_control: libp2p_stream::Control,
        allowed_hosts: Vec<String>,
        runtime_client: system_capnp::runtime::Client,
        ipfs_client: ipfs::HttpClient,
    ) -> Self {
        Self {
            network_state,
            swarm_cmd_tx,
            wasm_debug,
            signing_key,
            stream_control,
            allowed_hosts,
            route_registry: None,
            runtime_client,
            extras: Vec::new(),
            ipfs_client,
        }
    }

    /// Set the HTTP route registry for WAGI integration.
    pub fn with_route_registry(mut self, registry: crate::dispatch::RouteRegistry) -> Self {
        self.route_registry = Some(registry);
        self
    }

    /// Set named capabilities from init.d `with` block to inject into graft.
    ///
    /// The third tuple element is the canonical `Schema.Node` bytes for the
    /// cap. The wire format already carries these (see `stem.capnp:Export`);
    /// callers that received the cap over `Executor.spawn` /
    /// `VatListener.listen` should preserve `entry.get_schema()` and pass
    /// it through. Pass an empty Vec only when the schema is genuinely
    /// unknown — graft will then leave `Export.schema` empty and warn.
    pub fn with_extras(
        mut self,
        extras: Vec<(String, capnp::capability::Client, Vec<u8>)>,
    ) -> Self {
        self.extras = extras;
        self
    }
}

impl GraftBuilder for HostGraftBuilder {
    fn build(
        &self,
        guard: &EpochGuard,
        mut builder: stem_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), capnp::Error> {
        // Build the core capabilities.
        let mut host_impl = super::HostImpl::new(
            self.network_state.clone(),
            self.swarm_cmd_tx.clone(),
            self.wasm_debug,
            Some(guard.clone()),
            Some(self.stream_control.clone()),
        );
        if let Some(ref registry) = self.route_registry {
            host_impl = host_impl.with_route_registry(registry.clone());
        }
        let host: system_capnp::host::Client = capnp_rpc::new_client(host_impl);

        let routing: routing_capnp::routing::Client =
            capnp_rpc::new_client(super::routing::RoutingImpl::new(
                self.swarm_cmd_tx.clone(),
                guard.clone(),
                self.ipfs_client.clone(),
            ));

        // Collect all capabilities into a flat list of Export entries.
        let mut entries: Vec<(&str, capnp::capability::Client)> = Vec::new();

        if let Some(sk) = &self.signing_key {
            let keypair =
                crate::keys::to_libp2p(sk).map_err(|e| capnp::Error::failed(e.to_string()))?;
            let identity: stem_capnp::identity::Client =
                capnp_rpc::new_client(EpochGuardedIdentity::new(keypair, guard.clone()));
            entries.push(("identity", identity.client));
        }

        entries.push(("host", host.client));
        entries.push(("runtime", self.runtime_client.clone().client));
        entries.push(("routing", routing.client));
        let ipfs_cap: system_capnp::ipfs::Client = capnp_rpc::new_client(EpochGuardedIpfs {
            guard: guard.clone(),
            ipfs_client: self.ipfs_client.clone(),
        });
        entries.push(("ipfs", ipfs_cap.client));

        // Only grant http-client if the operator explicitly opted in via --http-dial.
        if !self.allowed_hosts.is_empty() {
            let http_client: http_capnp::http_client::Client =
                capnp_rpc::new_client(super::http_client::EpochGuardedHttpProxy::new(
                    self.allowed_hosts.clone(),
                    guard.clone(),
                ));
            entries.push(("http-client", http_client.client));
        }

        // Append init.d-scoped extras.
        let extras_owned: Vec<(String, capnp::capability::Client, Vec<u8>)> = self
            .extras
            .iter()
            .map(|(name, client, schema)| (name.clone(), client.clone(), schema.clone()))
            .collect();

        let count = (entries.len() + extras_owned.len()) as u32;
        let mut caps_builder = builder.reborrow().init_caps(count);

        for (i, (name, client)) in entries.iter().enumerate() {
            let mut entry = caps_builder.reborrow().get(i as u32);
            entry.set_name(name);
            write_schema_for_core_cap(entry.reborrow(), name)?;
            entry.init_cap().set_as_capability(client.hook.clone());
        }

        let offset = entries.len();
        for (i, (name, client, schema_bytes)) in extras_owned.iter().enumerate() {
            let mut entry = caps_builder.reborrow().get((offset + i) as u32);
            entry.set_name(name);
            if schema_bytes.is_empty() {
                tracing::warn!(
                    name = name.as_str(),
                    "extra cap registered without schema bytes; Export.schema will be empty"
                );
                entry.reborrow().init_schema();
            } else {
                write_schema_bytes(entry.reborrow(), schema_bytes)?;
            }
            entry.init_cap().set_as_capability(client.hook.clone());
        }

        Ok(())
    }
}

/// Populate `Export.schema` with the canonical Schema.Node bytes for a core
/// capability name. Core caps (`identity`, `host`, `runtime`, `routing`,
/// `http-client`) have schemas baked into the binary at build time by
/// `crates/membrane/build.rs`. Unknown names get an empty Schema.Node —
/// callers then fall back to string-name lookup.
fn write_schema_for_core_cap(
    mut entry: stem_capnp::export::Builder<'_>,
    name: &str,
) -> Result<(), capnp::Error> {
    let Some(bytes) = membrane::schema_registry::schema_by_name(name) else {
        entry.init_schema();
        return Ok(());
    };
    write_schema_bytes(entry.reborrow(), bytes)
}

/// Populate `Export.schema` from canonical Schema.Node bytes.
///
/// Shared between the core-cap path (bytes baked in at build time) and the
/// extras path (bytes received from upstream over the wire and threaded
/// through `HostGraftBuilder::with_extras`). Caller is responsible for
/// handling the empty-bytes case before calling this.
///
/// Canonical encoding is the raw single-segment payload (no framing
/// header). Capnp segments must be 8-byte aligned, but byte slices may
/// only be byte-aligned, so copy into a Word-aligned buffer first.
fn write_schema_bytes(
    mut entry: stem_capnp::export::Builder<'_>,
    bytes: &[u8],
) -> Result<(), capnp::Error> {
    let aligned = bytes_to_aligned_words(bytes);
    let segments: &[&[u8]] = &[capnp::Word::words_to_bytes(&aligned)];
    let segment_array = capnp::message::SegmentArray::new(segments);
    let reader = capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
    let schema_node: capnp::schema_capnp::node::Reader = reader.get_root()?;
    entry.set_schema(schema_node)?;
    Ok(())
}

/// Copy a byte slice into an 8-byte-aligned `Vec<Word>` buffer. Trailing
/// bytes (if `bytes.len()` is not word-multiple) are zero-padded; canonical
/// capnp encodings are always word-aligned, so this should be lossless.
pub(super) fn bytes_to_aligned_words(bytes: &[u8]) -> Vec<capnp::Word> {
    let word_count = bytes.len().div_ceil(8);
    let mut words = vec![capnp::word(0, 0, 0, 0, 0, 0, 0, 0); word_count];
    capnp::Word::words_to_bytes_mut(&mut words)[..bytes.len()].copy_from_slice(bytes);
    words
}

// IPFS content access goes through the WASI virtual filesystem (CidTree).
// See src/vfs.rs and src/fs_intercept.rs.

// ---------------------------------------------------------------------------
// build_membrane_rpc — bootstrap Membrane instead of Host
// ---------------------------------------------------------------------------

/// The Membrane type exported by WASM guests back to the host.
///
/// When a guest calls `runtime::serve(my_membrane, ...)`, the host
/// captures it here. The host can then re-serve it to external peers,
/// allowing the guest to attenuate or enrich the capability surface it exposes.
pub type GuestMembrane = membrane::stem_capnp::membrane::Client;

/// Build an RPC system that bootstraps a `Membrane` instead of a bare `Host`.
///
/// The membrane provides epoch-scoped sessions containing `Host`, `Executor`,
/// and (when `signing_key` is `Some`) a host-side node identity signer.
///
/// When `signing_key` is `Some`, an [`EpochGuardedIdentity`] hub is injected into
/// every session so the kernel can request domain-scoped signers without holding
/// the private key. Auth (if needed) is handled by wrapping in `TerminalServer`
/// at the transport layer, not here.
///
/// Returns both the RPC system and the guest's exported [`GuestMembrane`], if
/// the guest called `runtime::serve()`. If the guest called `runtime::run()`
/// instead, the returned capability is broken and attempts to use it will fail.
#[allow(clippy::too_many_arguments)]
pub fn build_membrane_rpc<R, W>(
    reader: R,
    writer: W,
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    wasm_debug: bool,
    epoch_rx: watch::Receiver<Epoch>,
    signing_key: Option<Arc<SigningKey>>,
    stream_control: libp2p_stream::Control,
    route_registry: Option<crate::dispatch::RouteRegistry>,
    runtime_client: system_capnp::runtime::Client,
    extras: Vec<(String, capnp::capability::Client, Vec<u8>)>,
    ipfs_client: ipfs::HttpClient,
    http_dial: Vec<String>,
) -> (RpcSystem<Side>, GuestMembrane)
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut sess_builder = HostGraftBuilder::new(
        network_state,
        swarm_cmd_tx,
        wasm_debug,
        signing_key,
        stream_control,
        http_dial,
        runtime_client,
        ipfs_client,
    );
    if !extras.is_empty() {
        sess_builder = sess_builder.with_extras(extras);
    }
    if let Some(registry) = route_registry {
        sess_builder = sess_builder.with_route_registry(registry);
    }
    // The local kernel is a trusted process — no challenge-response auth needed.
    // Auth applies to external peers connecting via libp2p to the guest's exported membrane.
    let membrane_server = MembraneServer::new(epoch_rx, sess_builder);
    let membrane: GuestMembrane = capnp_rpc::new_client(membrane_server);

    let rpc_network = VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        Side::Server,
        Default::default(),
    );
    let mut rpc_system = RpcSystem::new(Box::new(rpc_network), Some(membrane.client));
    let guest_membrane: GuestMembrane = rpc_system.bootstrap(Side::Client);
    (rpc_system, guest_membrane)
}

// IPFS content access is tested in fs_intercept::tests and vfs::tests.

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;
    use membrane::{Epoch, Provenance};

    /// Generate a random Ed25519 signing key (compatible with the rand version
    /// used by the root crate, which may differ from ed25519_dalek's rand_core).
    fn gen_signing_key() -> ed25519_dalek::SigningKey {
        crate::keys::generate().expect("OS CSPRNG")
    }

    /// Helper: create an EpochGuardedIdentity client for testing.
    fn test_identity() -> (
        stem_capnp::identity::Client,
        tokio::sync::watch::Sender<Epoch>,
    ) {
        let sk = gen_signing_key();
        let keypair = crate::keys::to_libp2p(&sk).expect("valid ed25519 keypair");
        let epoch = Epoch {
            seq: 1,
            head: b"test".to_vec(),
            provenance: Provenance::Block(100),
        };
        let (tx, rx) = tokio::sync::watch::channel(epoch);
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: rx,
        };
        let client: stem_capnp::identity::Client =
            capnp_rpc::new_client(EpochGuardedIdentity::new(keypair, guard));
        (client, tx)
    }

    /// Helper: sign data with a given signing key (raw Ed25519, no envelope).
    fn sign_data(sk: &ed25519_dalek::SigningKey, data: &[u8]) -> ed25519_dalek::Signature {
        sk.sign(data)
    }

    #[tokio::test]
    async fn verify_valid_signature_returns_true() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (identity, _tx) = test_identity();
                let sk = gen_signing_key();
                let vk = sk.verifying_key();
                let data = b"hello world";
                let sig = sign_data(&sk, data);

                let mut req = identity.verify_request();
                req.get().set_data(data);
                req.get().set_signature(&sig.to_bytes());
                req.get().set_pubkey(&vk.to_bytes());

                let resp = req.send().promise.await.expect("verify RPC");
                assert!(resp.get().expect("verify results").get_valid());
            })
            .await;
    }

    #[tokio::test]
    async fn verify_wrong_data_returns_false() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (identity, _tx) = test_identity();
                let sk = gen_signing_key();
                let vk = sk.verifying_key();
                let sig = sign_data(&sk, b"correct data");

                let mut req = identity.verify_request();
                req.get().set_data(b"wrong data");
                req.get().set_signature(&sig.to_bytes());
                req.get().set_pubkey(&vk.to_bytes());

                let resp = req.send().promise.await.expect("verify RPC");
                assert!(!resp.get().expect("verify results").get_valid());
            })
            .await;
    }

    #[tokio::test]
    async fn verify_wrong_pubkey_returns_false() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (identity, _tx) = test_identity();
                let sk = gen_signing_key();
                let wrong_sk = gen_signing_key();
                let wrong_vk = wrong_sk.verifying_key();
                let data = b"hello world";
                let sig = sign_data(&sk, data);

                let mut req = identity.verify_request();
                req.get().set_data(data);
                req.get().set_signature(&sig.to_bytes());
                req.get().set_pubkey(&wrong_vk.to_bytes());

                let resp = req.send().promise.await.expect("verify RPC");
                assert!(!resp.get().expect("verify results").get_valid());
            })
            .await;
    }

    #[tokio::test]
    async fn verify_malformed_pubkey_returns_error() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (identity, _tx) = test_identity();

                let mut req = identity.verify_request();
                req.get().set_data(b"data");
                req.get().set_signature(&[0u8; 64]);
                req.get().set_pubkey(&[0u8; 16]); // wrong length

                let result = req.send().promise.await;
                match result {
                    Ok(resp) => match resp.get() {
                        Ok(_) => panic!("should fail with wrong pubkey length"),
                        Err(e) => assert!(
                            e.to_string().contains("pubkey must be 32 bytes"),
                            "unexpected error: {e}"
                        ),
                    },
                    Err(e) => assert!(
                        e.to_string().contains("pubkey must be 32 bytes"),
                        "unexpected error: {e}"
                    ),
                }
            })
            .await;
    }

    #[tokio::test]
    async fn verify_malformed_signature_returns_error() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (identity, _tx) = test_identity();
                let sk = gen_signing_key();
                let vk = sk.verifying_key();

                let mut req = identity.verify_request();
                req.get().set_data(b"data");
                req.get().set_signature(&[0u8; 32]); // wrong length (should be 64)
                req.get().set_pubkey(&vk.to_bytes());

                let result = req.send().promise.await;
                match result {
                    Ok(resp) => match resp.get() {
                        Ok(_) => panic!("should fail with wrong signature length"),
                        Err(e) => assert!(
                            e.to_string().contains("signature must be 64 bytes"),
                            "unexpected error: {e}"
                        ),
                    },
                    Err(e) => assert!(
                        e.to_string().contains("signature must be 64 bytes"),
                        "unexpected error: {e}"
                    ),
                }
            })
            .await;
    }

    #[tokio::test]
    async fn verify_empty_data_with_valid_signature() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (identity, _tx) = test_identity();
                let sk = gen_signing_key();
                let vk = sk.verifying_key();
                let data = b"";
                let sig = sign_data(&sk, data);

                let mut req = identity.verify_request();
                req.get().set_data(data);
                req.get().set_signature(&sig.to_bytes());
                req.get().set_pubkey(&vk.to_bytes());

                let resp = req.send().promise.await.expect("verify RPC");
                assert!(resp.get().expect("verify results").get_valid());
            })
            .await;
    }

    #[tokio::test]
    async fn verify_fails_after_epoch_advance() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (identity, tx) = test_identity();
                let sk = gen_signing_key();
                let vk = sk.verifying_key();
                let data = b"hello";
                let sig = sign_data(&sk, data);

                // Advance epoch.
                tx.send(Epoch {
                    seq: 2,
                    head: b"new".to_vec(),
                    provenance: Provenance::Block(101),
                })
                .unwrap();

                let mut req = identity.verify_request();
                req.get().set_data(data);
                req.get().set_signature(&sig.to_bytes());
                req.get().set_pubkey(&vk.to_bytes());

                let result = req.send().promise.await;
                match result {
                    Ok(resp) => match resp.get() {
                        Ok(_) => panic!("verify should fail after epoch advance"),
                        Err(e) => assert!(
                            e.to_string().contains("staleEpoch"),
                            "expected staleEpoch, got: {e}"
                        ),
                    },
                    Err(e) => assert!(
                        e.to_string().contains("staleEpoch"),
                        "expected staleEpoch, got: {e}"
                    ),
                }
            })
            .await;
    }

    // -----------------------------------------------------------------
    // Item 1a: Export.schema population end-to-end
    // -----------------------------------------------------------------
    //
    // These tests exercise the full pipeline:
    //   1. crates/membrane/build.rs extracts canonical Schema.Node bytes
    //   2. write_schema_for_core_cap reconstructs an aligned Reader and
    //      copies into Export.schema
    //   3. The freshly-built Export message is reparsed (mimicking the
    //      guest read path) and Schema.Node fields are inspected
    //
    // Catches regressions in: byte alignment, SegmentArray construction,
    // set_schema copy semantics, and build-script silent failures (e.g.,
    // empty bytes from a misconfigured cargo cache).

    /// Type IDs for each core cap interface, kept in sync with
    /// `crates/membrane/build.rs`. If the capnp interfaces change IDs
    /// these constants must be updated; the test then catches that
    /// renamed/regenerated bytes still resolve to the right interface.
    const HOST_TYPE_ID: u64 = 0x9ea7_0c8c_9aef_b70c;
    const RUNTIME_TYPE_ID: u64 = 0x8738_4748_df10_173c;
    const ROUTING_TYPE_ID: u64 = 0xc033_44a7_b0a3_17be;
    const IDENTITY_TYPE_ID: u64 = 0xa7c2_00e5_b472_6d89;
    const HTTP_CLIENT_TYPE_ID: u64 = 0xf00a_15d0_9fb8_f360;

    /// Build a fresh Export message, write the schema for `name` into
    /// it, then reparse the resulting bytes (round-tripping through
    /// capnp serialization just as a graft response would) and return
    /// the Schema.Node id along with whether the node is an Interface.
    fn write_then_read_schema(name: &str) -> (u64, bool) {
        let mut message = capnp::message::Builder::new_default();
        let entry: stem_capnp::export::Builder<'_> = message.init_root();
        write_schema_for_core_cap(entry, name).expect("write_schema_for_core_cap");

        // Round-trip via capnp serialization, mimicking what a guest sees
        // when the graft response arrives over the RPC channel.
        let mut buf = Vec::new();
        capnp::serialize::write_message(&mut buf, &message).expect("serialize export");
        let reader = capnp::serialize::read_message_from_flat_slice(
            &mut buf.as_slice(),
            capnp::message::ReaderOptions::new(),
        )
        .expect("deserialize export");
        let entry: stem_capnp::export::Reader<'_> = reader.get_root().expect("export root");
        let schema_node: capnp::schema_capnp::node::Reader =
            entry.get_schema().expect("schema field present");
        let id = schema_node.get_id();
        let is_interface = matches!(
            schema_node.which().expect("Which resolves"),
            capnp::schema_capnp::node::Which::Interface(_)
        );
        (id, is_interface)
    }

    #[test]
    fn item1a_host_schema_round_trips_with_correct_type_id() {
        let (id, is_interface) = write_then_read_schema("host");
        assert_eq!(id, HOST_TYPE_ID, "host schema type ID mismatch");
        assert!(is_interface, "host schema must be an interface node");
    }

    #[test]
    fn item1a_runtime_schema_round_trips_with_correct_type_id() {
        let (id, is_interface) = write_then_read_schema("runtime");
        assert_eq!(id, RUNTIME_TYPE_ID, "runtime schema type ID mismatch");
        assert!(is_interface, "runtime schema must be an interface node");
    }

    #[test]
    fn item1a_routing_schema_round_trips_with_correct_type_id() {
        let (id, is_interface) = write_then_read_schema("routing");
        assert_eq!(id, ROUTING_TYPE_ID, "routing schema type ID mismatch");
        assert!(is_interface, "routing schema must be an interface node");
    }

    #[test]
    fn item1a_identity_schema_round_trips_with_correct_type_id() {
        let (id, is_interface) = write_then_read_schema("identity");
        assert_eq!(id, IDENTITY_TYPE_ID, "identity schema type ID mismatch");
        assert!(is_interface, "identity schema must be an interface node");
    }

    #[test]
    fn item1a_http_client_schema_round_trips_with_correct_type_id() {
        let (id, is_interface) = write_then_read_schema("http-client");
        assert_eq!(
            id, HTTP_CLIENT_TYPE_ID,
            "http-client schema type ID mismatch"
        );
        assert!(is_interface, "http-client schema must be an interface node");
    }

    #[test]
    fn item1a_unknown_cap_yields_empty_schema() {
        let mut message = capnp::message::Builder::new_default();
        let entry: stem_capnp::export::Builder<'_> = message.init_root();
        write_schema_for_core_cap(entry, "not-a-real-cap").expect("returns Ok with empty");

        let mut buf = Vec::new();
        capnp::serialize::write_message(&mut buf, &message).expect("serialize");
        let reader = capnp::serialize::read_message_from_flat_slice(
            &mut buf.as_slice(),
            capnp::message::ReaderOptions::new(),
        )
        .expect("deserialize");
        let entry: stem_capnp::export::Reader<'_> = reader.get_root().expect("export root");
        let schema_node: capnp::schema_capnp::node::Reader =
            entry.get_schema().expect("schema field present");
        // Empty Schema.Node has id=0 and a default (uninterpreted) Which.
        assert_eq!(schema_node.get_id(), 0, "unknown cap should produce id=0");
    }

    #[test]
    fn item1a_methods_enumerable_post_round_trip() {
        // The whole point of populating Export.schema is so that consumers
        // (MCP tool description generator, future (schema cap) builtin) can
        // walk methods. Verify host's methods are non-empty and have names.
        let mut message = capnp::message::Builder::new_default();
        let entry: stem_capnp::export::Builder<'_> = message.init_root();
        write_schema_for_core_cap(entry, "host").expect("write host schema");

        let mut buf = Vec::new();
        capnp::serialize::write_message(&mut buf, &message).expect("serialize");
        let reader = capnp::serialize::read_message_from_flat_slice(
            &mut buf.as_slice(),
            capnp::message::ReaderOptions::new(),
        )
        .expect("deserialize");
        let entry: stem_capnp::export::Reader<'_> = reader.get_root().expect("export root");
        let schema_node: capnp::schema_capnp::node::Reader =
            entry.get_schema().expect("schema field present");

        let interface = match schema_node.which().expect("Which") {
            capnp::schema_capnp::node::Which::Interface(iface) => iface,
            _ => panic!("expected interface node, got a non-interface variant"),
        };
        let methods = interface.get_methods().expect("methods list");
        assert!(!methods.is_empty(), "host interface should have methods");
        // Sanity: each method has a non-empty name reachable from the
        // round-tripped schema. This is the property MCP tool generation
        // depends on.
        for method in methods.iter() {
            let name = method.get_name().expect("method name");
            assert!(!name.is_empty(), "method names should be non-empty");
        }
    }

    // -----------------------------------------------------------------
    // Item 1b: Export.schema population for init.d-scoped extras
    // -----------------------------------------------------------------
    //
    // These tests exercise the extras path of the graft loop. Extras
    // travel as `(name, client, schema_bytes)` tuples; each iteration of
    // the extras loop calls `write_schema_bytes` with the caller-provided
    // bytes, or falls back to an empty Schema.Node + warn log when the
    // bytes are empty.
    //
    // We exercise `write_schema_bytes` directly rather than driving a
    // full graft (constructing a `HostGraftBuilder` requires a long
    // tail of fixtures unrelated to the schema-population path under
    // test).

    /// Build a fresh Export message, populate its schema field via the
    /// generic `write_schema_bytes`, then round-trip through capnp
    /// serialization (mimicking the wire) and return the resulting
    /// `Schema.Node` id and whether it parses as an interface.
    fn write_then_read_extras_schema(bytes: &[u8]) -> (u64, bool) {
        let mut message = capnp::message::Builder::new_default();
        let entry: stem_capnp::export::Builder<'_> = message.init_root();
        write_schema_bytes(entry, bytes).expect("write_schema_bytes");

        let mut buf = Vec::new();
        capnp::serialize::write_message(&mut buf, &message).expect("serialize");
        let reader = capnp::serialize::read_message_from_flat_slice(
            &mut buf.as_slice(),
            capnp::message::ReaderOptions::new(),
        )
        .expect("deserialize");
        let entry: stem_capnp::export::Reader<'_> = reader.get_root().expect("export root");
        let schema_node: capnp::schema_capnp::node::Reader =
            entry.get_schema().expect("schema field present");
        let id = schema_node.get_id();
        let is_interface = matches!(
            schema_node.which().expect("Which resolves"),
            capnp::schema_capnp::node::Which::Interface(_)
        );
        (id, is_interface)
    }

    #[test]
    fn item1b_extras_path_writes_caller_provided_bytes() {
        // Drive the extras path with a known-good Schema.Node payload —
        // any of the core-cap schemas works (they're real interface nodes
        // available to host-side tests via the schema_registry constants).
        let bytes = membrane::schema_registry::HOST_SCHEMA;
        let (id, is_interface) = write_then_read_extras_schema(bytes);
        assert_eq!(id, HOST_TYPE_ID, "extras-path schema type ID mismatch");
        assert!(is_interface, "extras-path schema must parse as interface");
    }

    #[test]
    fn item1b_extras_path_handles_each_core_schema() {
        // Sanity: every byte slice schema_registry exposes round-trips
        // through the extras path. Catches regressions where the
        // extras path diverges from the core-cap path's handling.
        for (name, expected_id) in [
            ("host", HOST_TYPE_ID),
            ("runtime", RUNTIME_TYPE_ID),
            ("routing", ROUTING_TYPE_ID),
            ("identity", IDENTITY_TYPE_ID),
            ("http-client", HTTP_CLIENT_TYPE_ID),
        ] {
            let bytes = membrane::schema_registry::schema_by_name(name)
                .unwrap_or_else(|| panic!("registry missing '{name}'"));
            let (id, is_interface) = write_then_read_extras_schema(bytes);
            assert_eq!(
                id, expected_id,
                "extras path for '{name}' produced wrong id"
            );
            assert!(is_interface, "extras path for '{name}' lost interface tag");
        }
    }

    #[test]
    fn item1b_empty_bytes_path_yields_empty_schema() {
        // The HostGraftBuilder loop calls `init_schema()` directly when
        // bytes are empty and emits a warn log; the bytes path itself is
        // never invoked. Verify the runtime invariant that an empty
        // `init_schema()` produces a Schema.Node with id=0, which is the
        // shape consumers see for "extra cap registered without schema".
        let mut message = capnp::message::Builder::new_default();
        let entry: stem_capnp::export::Builder<'_> = message.init_root();
        entry.init_schema();

        let mut buf = Vec::new();
        capnp::serialize::write_message(&mut buf, &message).expect("serialize");
        let reader = capnp::serialize::read_message_from_flat_slice(
            &mut buf.as_slice(),
            capnp::message::ReaderOptions::new(),
        )
        .expect("deserialize");
        let entry: stem_capnp::export::Reader<'_> = reader.get_root().expect("export root");
        let schema_node: capnp::schema_capnp::node::Reader =
            entry.get_schema().expect("schema field present");
        assert_eq!(
            schema_node.get_id(),
            0,
            "empty init_schema must produce id=0"
        );
    }

    #[test]
    fn item1b_canonicalize_schema_node_matches_registry_bytes() {
        // The Executor.spawn / VatListener.listen handlers call
        // super::canonicalize_schema_node on the wire-side Schema.Node
        // reader to recover canonical bytes for forwarding. Ensure the
        // recipe matches what the build-time pipeline emits — a guest
        // round-tripping a core cap's schema through this path must
        // produce bytes that re-parse to the same type ID.
        for name in ["host", "runtime", "routing", "identity", "http-client"] {
            let canonical = membrane::schema_registry::schema_by_name(name)
                .unwrap_or_else(|| panic!("registry missing '{name}'"));

            // Re-parse the registry bytes as a Schema.Node...
            let aligned = bytes_to_aligned_words(canonical);
            let segments: &[&[u8]] = &[capnp::Word::words_to_bytes(&aligned)];
            let segment_array = capnp::message::SegmentArray::new(segments);
            let reader =
                capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
            let node: capnp::schema_capnp::node::Reader = reader.get_root().expect("node");

            // ...then re-canonicalize via the wire-side helper and compare.
            let recovered = crate::canonicalize_schema_node(node).expect("canonicalize succeeds");
            assert_eq!(
                canonical,
                &recovered[..],
                "canonicalize_schema_node must reproduce build-time bytes for '{name}'"
            );
        }
    }
}
