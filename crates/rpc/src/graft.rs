//! Membrane-based RPC bootstrap: epoch-scoped Host + Executor + node identity capabilities.
//!
//! Instead of bootstrapping a bare `Host`, the membrane's `graft()` returns
//! epoch-scoped `Host`, `Executor`, and a node `identity` signer directly as
//! result fields. All capabilities fail with `staleEpoch` when the epoch
//! advances.
//!
//! The `authority` crate owns the Membrane server and epoch machinery.
//! This module provides the `GraftBuilder` impl that injects wetware-specific
//! capabilities into the graft response, plus the epoch-guarded identity wrapper.

use std::sync::Arc;

use authority::{auth_capnp, membrane_capnp, Epoch, EpochGuard, GraftBuilder, MembraneServer};
use capnp::capability::Promise;
use capnp_rpc::pry;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use libp2p::identity::Keypair;
use libp2p_core::SignedEnvelope;
use tokio::io::{self, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{
    ByteStreamImpl, InitialAuthorityRecord, NamedCapabilities, NamedCapability, StreamMode,
    SwarmCommand,
};
use auth::SigningDomain;
use authority::http_capnp;
use authority::routing_capnp;
use authority::system_capnp;

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
    /// cell's graft response as `Export { name, cap }` entries.
    extras: NamedCapabilities,
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
            extras: NamedCapabilities::default(),
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
    pub fn with_extras(mut self, extras: NamedCapabilities) -> Self {
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
        let mut entries = Vec::new();

        if let Some(sk) = &self.signing_key {
            let keypair =
                crate::keys::to_libp2p(sk).map_err(|e| capnp::Error::failed(e.to_string()))?;
            let identity: auth_capnp::identity::Client =
                capnp_rpc::new_client(EpochGuardedIdentity::new(keypair, guard.clone()));
            entries.push(NamedCapability::new("identity", identity.client)?);
        }

        entries.push(NamedCapability::new("host", host.client)?);
        entries.push(NamedCapability::new(
            "runtime",
            self.runtime_client.clone().client,
        )?);
        entries.push(NamedCapability::new("routing", routing.client)?);
        let authority: auth_capnp::authority::Client =
            capnp_rpc::new_client(authority::AuthorityServer::new(guard.clone()));
        entries.push(NamedCapability::new("authority", authority.client)?);
        let ipfs_cap: system_capnp::ipfs::Client = capnp_rpc::new_client(EpochGuardedIpfs {
            guard: guard.clone(),
            ipfs_client: self.ipfs_client.clone(),
        });
        entries.push(NamedCapability::new("ipfs", ipfs_cap.client)?);

        // Only grant http-client if the operator explicitly opted in via --http-dial.
        if !self.allowed_hosts.is_empty() {
            let http_client: http_capnp::http_client::Client =
                capnp_rpc::new_client(super::http_client::EpochGuardedHttpProxy::new(
                    self.allowed_hosts.clone(),
                    guard.clone(),
                ));
            entries.push(NamedCapability::new("http-client", http_client.client)?);
        }

        // Append init.d-scoped extras.
        // Keep ambient graft entries and parent extras as independently
        // validated sets. Collision policy between those sets remains a graft
        // concern until the T3 bootstrap cutover.
        let ambient = NamedCapabilities::try_from_iter(entries)?;
        let count = ambient
            .len()
            .checked_add(self.extras.len())
            .and_then(|len| len.try_into().ok())
            .ok_or_else(|| {
                capnp::Error::failed("too many capability exports for Cap'n Proto".into())
            })?;
        let mut caps_builder = builder.reborrow().init_caps(count);
        for (index, capability) in ambient.iter().chain(self.extras.iter()).enumerate() {
            crate::named_capability::encode_export(
                capability,
                caps_builder.reborrow().get(index as u32),
            );
        }
        Ok(())
    }
}

// IPFS content access goes through the WASI virtual filesystem (CidTree).
// See src/vfs.rs and src/fs_intercept.rs.

// ---------------------------------------------------------------------------
// RPC bootstrap constructors
// ---------------------------------------------------------------------------

/// The Membrane type exported by WASM guests back to the host.
///
/// When a guest calls `runtime::serve(my_membrane, ...)`, the host
/// captures it here. The host can then re-serve it to external peers,
/// allowing the guest to attenuate or enrich the capability surface it exposes.
pub type GuestMembrane = authority::membrane_capnp::membrane::Client;

/// Host-provided bootstrap received by an ordinary child.
pub type ChildInitialGrants = authority::membrane_capnp::initial_grants::Client;

/// Guest-exported capability imported by the host and exposed only through the
/// parent-held `Process.bootstrap()` operation.
pub type GuestExport = capnp::capability::Client;

/// Closed-delivery server for one immutable child authority record.
struct InitialGrantsServer {
    record: InitialAuthorityRecord,
}

#[allow(refining_impl_trait)]
impl membrane_capnp::initial_grants::Server for InitialGrantsServer {
    fn get(
        self: capnp::capability::Rc<Self>,
        _params: membrane_capnp::initial_grants::GetParams,
        mut results: membrane_capnp::initial_grants::GetResults,
    ) -> Promise<(), capnp::Error> {
        let count = match self.record.grants().len().try_into() {
            Ok(count) => count,
            Err(_) => {
                return Promise::err(capnp::Error::failed(
                    "too many initial authority grants for Cap'n Proto".into(),
                ));
            }
        };
        let caps = results.get().init_caps(count);
        match self.record.encode(caps) {
            Ok(()) => Promise::ok(()),
            Err(error) => Promise::err(error),
        }
    }
}

/// Build the sole ordinary-child RPC/bootstrap path.
///
/// The bootstrap contains exactly `record`, including when the record is
/// empty. It has no host/runtime/routing/identity/IPFS/HTTP inputs and no
/// alternate constructor selected by missing epoch or stream wiring.
pub fn build_initial_authority_rpc<R, W>(
    reader: R,
    writer: W,
    record: InitialAuthorityRecord,
) -> (RpcSystem<Side>, GuestExport)
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let bootstrap: ChildInitialGrants = capnp_rpc::new_client(InitialGrantsServer { record });
    let rpc_network = VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        Side::Server,
        Default::default(),
    );
    let mut rpc_system = RpcSystem::new(Box::new(rpc_network), Some(bootstrap.client));
    let guest_export: GuestExport = rpc_system.bootstrap(Side::Client);
    (rpc_system, guest_export)
}

/// Build the trusted pid0 RPC system with its full graft-capable `Membrane`.
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
pub fn build_pid0_membrane_rpc<R, W>(
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
    extras: NamedCapabilities,
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
    use authority::{Epoch, Provenance};
    use capnp::traits::{Imbue, ImbueMut};
    use ed25519_dalek::Signer;
    use futures::FutureExt;

    struct RuntimeStub;
    impl system_capnp::runtime::Server for RuntimeStub {}

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

    #[test]
    fn pid0_builder_still_emits_the_full_host_graft() {
        let epoch = Epoch {
            seq: 1,
            head: b"pid0".to_vec(),
            provenance: Provenance::Block(1),
        };
        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(epoch);
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: epoch_rx,
        };
        let (swarm_tx, _swarm_rx) = mpsc::channel(1);
        let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
        let builder = HostGraftBuilder::new(
            NetworkState::from_peer_id(vec![1, 2, 3]),
            swarm_tx,
            false,
            Some(Arc::new(gen_signing_key())),
            libp2p_stream::Behaviour::new().new_control(),
            vec!["example.com".into()],
            runtime,
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
        );

        let mut message = capnp::message::Builder::new_default();
        let mut cap_table = Vec::new();
        {
            let mut results =
                message.init_root::<membrane_capnp::membrane::graft_results::Builder<'_>>();
            results.imbue_mut(&mut cap_table);
            builder.build(&guard, results).expect("build pid0 graft");
        }
        let mut results = message
            .get_root_as_reader::<membrane_capnp::membrane::graft_results::Reader<'_>>()
            .expect("read pid0 graft");
        results.imbue(&cap_table);
        let names: std::collections::HashSet<_> = results
            .get_caps()
            .expect("pid0 caps")
            .iter()
            .map(|entry| {
                entry
                    .get_name()
                    .expect("cap name")
                    .to_str()
                    .expect("UTF-8 cap name")
                    .to_owned()
            })
            .collect();
        for expected in [
            "identity",
            "host",
            "runtime",
            "routing",
            "authority",
            "ipfs",
            "http-client",
        ] {
            assert!(
                names.contains(expected),
                "trusted pid0 graft lost required capability {expected}: {names:?}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pid0_rpc_bootstrap_still_serves_the_full_graft() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let epoch = Epoch {
                    seq: 1,
                    head: b"pid0-rpc".to_vec(),
                    provenance: Provenance::Block(1),
                };
                let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(epoch);
                let (swarm_tx, _swarm_rx) = mpsc::channel(1);
                let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
                let (host_stream, guest_stream) = io::duplex(16 * 1024);
                let (host_reader, host_writer) = io::split(host_stream);
                let (guest_reader, guest_writer) = io::split(guest_stream);

                let (host_rpc, _guest_export) = build_pid0_membrane_rpc(
                    host_reader,
                    host_writer,
                    NetworkState::from_peer_id(vec![1, 2, 3]),
                    swarm_tx,
                    false,
                    epoch_rx,
                    Some(Arc::new(gen_signing_key())),
                    libp2p_stream::Behaviour::new().new_control(),
                    None,
                    runtime,
                    NamedCapabilities::default(),
                    ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                    vec!["example.com".into()],
                );
                tokio::task::spawn_local(host_rpc.map(|_| ()));

                let guest_network = VatNetwork::new(
                    guest_reader.compat(),
                    guest_writer.compat_write(),
                    Side::Client,
                    Default::default(),
                );
                let mut guest_rpc = RpcSystem::new(Box::new(guest_network), None);
                let membrane: GuestMembrane = guest_rpc.bootstrap(Side::Server);
                tokio::task::spawn_local(guest_rpc.map(|_| ()));

                let response = membrane
                    .graft_request()
                    .send()
                    .promise
                    .await
                    .expect("pid0 graft RPC");
                let names: std::collections::HashSet<_> = response
                    .get()
                    .expect("pid0 graft results")
                    .get_caps()
                    .expect("pid0 RPC caps")
                    .iter()
                    .map(|entry| {
                        entry
                            .get_name()
                            .expect("cap name")
                            .to_str()
                            .expect("UTF-8 cap name")
                            .to_owned()
                    })
                    .collect();
                for expected in [
                    "identity",
                    "host",
                    "runtime",
                    "routing",
                    "authority",
                    "ipfs",
                    "http-client",
                ] {
                    assert!(
                        names.contains(expected),
                        "pid0 RPC bootstrap lost {expected}: {names:?}"
                    );
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_child_rpc_serves_empty_grants_and_rejects_membrane_graft() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host_stream, guest_stream) = io::duplex(16 * 1024);
                let (host_reader, host_writer) = io::split(host_stream);
                let (guest_reader, guest_writer) = io::split(guest_stream);

                let (host_rpc, _guest_export) = build_initial_authority_rpc(
                    host_reader,
                    host_writer,
                    InitialAuthorityRecord::default(),
                );
                tokio::task::spawn_local(host_rpc.map(|_| ()));

                let guest_network = VatNetwork::new(
                    guest_reader.compat(),
                    guest_writer.compat_write(),
                    Side::Client,
                    Default::default(),
                );
                let mut guest_rpc = RpcSystem::new(Box::new(guest_network), None);
                let bootstrap: capnp::capability::Client = guest_rpc.bootstrap(Side::Server);
                tokio::task::spawn_local(guest_rpc.map(|_| ()));

                let grants = ChildInitialGrants {
                    client: bootstrap.clone(),
                };
                let response = grants
                    .get_request()
                    .send()
                    .promise
                    .await
                    .expect("InitialGrants.get RPC");
                assert_eq!(
                    response
                        .get()
                        .expect("initial grants results")
                        .get_caps()
                        .expect("initial grants")
                        .len(),
                    0
                );

                let membrane = GuestMembrane { client: bootstrap };
                let error = match membrane.graft_request().send().promise.await {
                    Ok(_) => panic!("ordinary child bootstrap must reject Membrane.graft"),
                    Err(error) => error,
                };
                assert!(
                    error
                        .to_string()
                        .to_ascii_lowercase()
                        .contains("unimplemented"),
                    "unexpected graft rejection: {error}"
                );
            })
            .await;
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
