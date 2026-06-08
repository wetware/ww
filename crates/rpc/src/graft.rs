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
use membrane::{auth_capnp, membrane_capnp, Epoch, EpochGuard, GraftBuilder, MembraneServer};
use tokio::io::{self, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{ByteStreamImpl, StreamMode, SwarmCommand};
use auth::SigningDomain;
use membrane::http_capnp;
use membrane::routing_capnp;
use membrane::system_capnp;

use super::NetworkState;
use crate::synapse_abi::{write_owned_synapse, write_placeholder_synapse, OwnedSynapse};

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
impl auth_capnp::identity::Server for EpochGuardedIdentity {
    fn signer(
        self: capnp::capability::Rc<Self>,
        params: auth_capnp::identity::SignerParams,
        mut results: auth_capnp::identity::SignerResults,
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
        let signer: auth_capnp::signer::Client = capnp_rpc::new_client(EpochGuardedDomainSigner {
            domain,
            keypair: self.keypair.clone(),
            guard: self.guard.clone(),
        });
        results.get().set_signer(signer);
        Promise::ok(())
    }

    fn verify(
        self: capnp::capability::Rc<Self>,
        params: auth_capnp::identity::VerifyParams,
        mut results: auth_capnp::identity::VerifyResults,
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

const IPFS_STREAM_BRIDGE_BUFFER_BYTES: usize = 64 * 1024;

fn validate_ipfs_path(path: &str) -> Result<(), capnp::Error> {
    if ipfs::is_ipfs_path(path) {
        return Ok(());
    }
    Err(capnp::Error::failed(format!(
        "ipfs.read: expected /ipfs/, /ipns/, or /ipld/ path; got {path}"
    )))
}

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

        if let Err(err) = validate_ipfs_path(&path) {
            return Promise::err(err);
        }

        let (mut writer, reader) = io::duplex(IPFS_STREAM_BRIDGE_BUFFER_BYTES);
        let stream_client: system_capnp::byte_stream::Client =
            capnp_rpc::new_client(ByteStreamImpl::new(reader, StreamMode::ReadOnly));
        results.get().set_stream(stream_client);

        let client = self.ipfs_client.clone();
        tokio::spawn(async move {
            if let Err(err) = client.cat_to_writer(&path, &mut writer).await {
                tracing::warn!(path = %path, error = %err, "ipfs.read bridge failed");
            }
            let _ = writer.shutdown().await;
        });

        Promise::ok(())
    }
}

#[allow(refining_impl_trait)]
impl auth_capnp::signer::Server for EpochGuardedDomainSigner {
    fn sign(
        self: capnp::capability::Rc<Self>,
        params: auth_capnp::signer::SignParams,
        mut results: auth_capnp::signer::SignResults,
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
    /// Named capabilities from init.d `with` blocks, forwarded to the child
    /// cell's graft response as Synapse exports.
    extras: Vec<(String, OwnedSynapse)>,
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
    pub fn with_extras(mut self, extras: Vec<(String, OwnedSynapse)>) -> Self {
        self.extras = extras;
        self
    }
}

impl GraftBuilder for HostGraftBuilder {
    fn build(
        &self,
        guard: &EpochGuard,
        mut builder: membrane_capnp::membrane::graft_results::Builder<'_>,
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
            let identity: auth_capnp::identity::Client =
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
        let extras_owned: Vec<(String, OwnedSynapse)> = self
            .extras
            .iter()
            .map(|(name, synapse)| (name.clone(), synapse.clone()))
            .collect();

        let count = (entries.len() + extras_owned.len()) as u32;
        let mut caps_builder = builder.reborrow().init_caps(count);

        for (i, (name, client)) in entries.iter().enumerate() {
            let mut entry = caps_builder.reborrow().get(i as u32);
            entry.set_name(name);
            let _ = client;
            write_placeholder_synapse(entry.init_synapse(), *name);
        }

        let offset = entries.len();
        for (i, (name, synapse)) in extras_owned.iter().enumerate() {
            let mut entry = caps_builder.reborrow().get((offset + i) as u32);
            entry.set_name(name);
            write_owned_synapse(entry.init_synapse(), synapse);
        }

        Ok(())
    }
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
pub type GuestMembrane = membrane::membrane_capnp::membrane::Client;

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
    extras: Vec<(String, OwnedSynapse)>,
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
        auth_capnp::identity::Client,
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
        let client: auth_capnp::identity::Client =
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
}
