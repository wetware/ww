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
#[derive(Clone)]
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
    /// Host-internal view of the pid0 execution-generation lifetime. Every
    /// graft for that generation shares this receiver; unrelated graft calls
    /// therefore cannot invalidate pid0 registrations.
    registration_scope: Option<watch::Receiver<()>>,
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
            registration_scope: None,
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

    fn with_registration_scope(mut self, scope: watch::Receiver<()>) -> Self {
        self.registration_scope = Some(scope);
        self
    }
}

impl HostGraftBuilder {
    fn build_named_capabilities(
        &self,
        guard: &EpochGuard,
    ) -> Result<NamedCapabilities, capnp::Error> {
        // Build the core capabilities.
        let mut host_impl = super::HostImpl::new(
            self.network_state.clone(),
            self.swarm_cmd_tx.clone(),
            self.wasm_debug,
            Some(guard.clone()),
            Some(self.stream_control.clone()),
        );
        if let Some(scope) = self.registration_scope.clone() {
            host_impl = host_impl.with_registration_scope(scope);
        }
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
        NamedCapabilities::try_from_iter(entries)
    }
}

fn export_count(
    ambient: &NamedCapabilities,
    extras: &NamedCapabilities,
) -> Result<u32, capnp::Error> {
    ambient
        .len()
        .checked_add(extras.len())
        .ok_or_else(|| capnp::Error::failed("too many capability exports for Cap'n Proto".into()))?
        .try_into()
        .map_err(|_| capnp::Error::failed("too many capability exports for Cap'n Proto".into()))
}

fn encode_graft_capabilities(
    ambient: &NamedCapabilities,
    extras: &NamedCapabilities,
    mut builder: capnp::struct_list::Builder<'_, membrane_capnp::export::Owned>,
) {
    for (index, capability) in ambient.iter().chain(extras.iter()).enumerate() {
        crate::named_capability::encode_export(capability, builder.reborrow().get(index as u32));
    }
}

impl GraftBuilder for HostGraftBuilder {
    fn build(
        &self,
        guard: &EpochGuard,
        mut builder: membrane_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), capnp::Error> {
        let ambient = self.build_named_capabilities(guard)?;
        let count = export_count(&ambient, &self.extras)?;
        encode_graft_capabilities(&ambient, &self.extras, builder.reborrow().init_caps(count));
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
#[must_use = "dropping the scope owner invalidates this pid0 generation's HTTP registrations"]
pub struct Pid0RegistrationScope {
    _sender: watch::Sender<()>,
}

impl Pid0RegistrationScope {
    fn new() -> (Self, watch::Receiver<()>) {
        let (sender, receiver) = watch::channel(());
        (Self { _sender: sender }, receiver)
    }
}

const PID0_EXPORT_MEMBRANE_CAP: &str = "pid0-export-membrane";

/// Process-local graft wrapper. Only the trusted PID0 bootstrap receives a
/// client for this server; the separately constructed export membrane never
/// touches the readiness gate.
struct Pid0RootGraftBuilder {
    inner: HostGraftBuilder,
    readiness_gate: Arc<authority::KernelReadyGate>,
    export_membrane: GuestMembrane,
}

impl GraftBuilder for Pid0RootGraftBuilder {
    fn build(
        &self,
        guard: &EpochGuard,
        builder: membrane_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), capnp::Error> {
        let ambient = self.inner.build_named_capabilities(guard)?;
        let export_cap = NamedCapability::new(
            PID0_EXPORT_MEMBRANE_CAP,
            self.export_membrane.clone().client,
        )?;
        let extras = NamedCapabilities::try_from_iter(
            self.inner
                .extras
                .iter()
                .cloned()
                .chain(std::iter::once(export_cap)),
        )?;
        let count = export_count(&ambient, &extras)?;
        self.readiness_gate.bind_generation(guard.issued_seq);
        encode_graft_capabilities(&ambient, &extras, builder.init_caps(count));
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_pid0_membrane_rpc<R, W>(
    reader: R,
    writer: W,
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    wasm_debug: bool,
    epoch_rx: watch::Receiver<Epoch>,
    readiness_gate: Arc<authority::KernelReadyGate>,
    signing_key: Option<Arc<SigningKey>>,
    stream_control: libp2p_stream::Control,
    route_registry: Option<crate::dispatch::RouteRegistry>,
    runtime_client: system_capnp::runtime::Client,
    extras: NamedCapabilities,
    ipfs_client: ipfs::HttpClient,
    http_dial: Vec<String>,
) -> (RpcSystem<Side>, GuestMembrane, Pid0RegistrationScope)
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let (registration_scope, registration_scope_rx) = Pid0RegistrationScope::new();
    let mut export_builder = HostGraftBuilder::new(
        network_state,
        swarm_cmd_tx,
        wasm_debug,
        signing_key,
        stream_control,
        http_dial,
        runtime_client,
        ipfs_client,
    )
    .with_registration_scope(registration_scope_rx);
    if !extras.is_empty() {
        export_builder = export_builder.with_extras(extras);
    }
    if let Some(registry) = route_registry {
        export_builder = export_builder.with_route_registry(registry);
    }

    // PID0 receives a process-local root membrane whose graft binds readiness.
    // The root graft also provisions a distinct ordinary membrane for the
    // kernel to publish. Network-facing grafts therefore cannot retarget the
    // private PID0 readiness gate.
    let export_membrane: GuestMembrane = capnp_rpc::new_client(MembraneServer::new(
        epoch_rx.clone(),
        export_builder.clone(),
    ));
    let root_builder = Pid0RootGraftBuilder {
        inner: export_builder,
        readiness_gate,
        export_membrane,
    };
    let root_membrane: GuestMembrane =
        capnp_rpc::new_client(MembraneServer::new(epoch_rx, root_builder));

    let rpc_network = VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        Side::Server,
        Default::default(),
    );
    let mut rpc_system = RpcSystem::new(Box::new(rpc_network), Some(root_membrane.client));
    let guest_membrane: GuestMembrane = rpc_system.bootstrap(Side::Client);
    (rpc_system, guest_membrane, registration_scope)
}

// IPFS content access is tested in fs_intercept::tests and vfs::tests.

#[cfg(test)]
mod tests {
    use super::*;
    use authority::{Epoch, KernelReadyError, Provenance};
    use capnp::traits::{Imbue, ImbueMut};
    use ed25519_dalek::Signer;
    use futures::FutureExt;
    use std::cell::Cell;
    use std::rc::Rc;

    struct RuntimeStub;
    impl system_capnp::runtime::Server for RuntimeStub {}

    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone)]
    struct CountingGraftBuilder {
        grafts: Rc<Cell<u32>>,
    }

    impl GraftBuilder for CountingGraftBuilder {
        fn build(
            &self,
            _guard: &EpochGuard,
            mut builder: membrane_capnp::membrane::graft_results::Builder<'_>,
        ) -> Result<(), capnp::Error> {
            self.grafts.set(self.grafts.get() + 1);
            builder.reborrow().init_caps(0);
            Ok(())
        }
    }

    struct TestRootGraftBuilder {
        inner: CountingGraftBuilder,
        readiness_gate: Arc<authority::KernelReadyGate>,
    }

    impl GraftBuilder for TestRootGraftBuilder {
        fn build(
            &self,
            guard: &EpochGuard,
            builder: membrane_capnp::membrane::graft_results::Builder<'_>,
        ) -> Result<(), capnp::Error> {
            self.readiness_gate.bind_generation(guard.issued_seq);
            self.inner.build(guard, builder)
        }
    }

    fn test_epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: seq.to_be_bytes().to_vec(),
            provenance: Provenance::Block(seq),
        }
    }

    struct SplitTestMembranes {
        root: GuestMembrane,
        export: GuestMembrane,
        readiness_gate: Arc<authority::KernelReadyGate>,
        root_grafts: Rc<Cell<u32>>,
        export_grafts: Rc<Cell<u32>>,
    }

    fn split_test_membranes(
        epoch_rx: watch::Receiver<Epoch>,
        activated_seq: Arc<AtomicU64>,
    ) -> SplitTestMembranes {
        let root_grafts = Rc::new(Cell::new(0));
        let export_grafts = Rc::new(Cell::new(0));
        let readiness_gate = Arc::new(authority::KernelReadyGate::new(
            epoch_rx.clone(),
            activated_seq,
        ));
        let export = capnp_rpc::new_client(MembraneServer::new(
            epoch_rx.clone(),
            CountingGraftBuilder {
                grafts: export_grafts.clone(),
            },
        ));
        let root = capnp_rpc::new_client(MembraneServer::new(
            epoch_rx,
            TestRootGraftBuilder {
                inner: CountingGraftBuilder {
                    grafts: root_grafts.clone(),
                },
                readiness_gate: readiness_gate.clone(),
            },
        ));
        SplitTestMembranes {
            root,
            export,
            readiness_gate,
            root_grafts,
            export_grafts,
        }
    }

    struct ExecutorStub;
    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for ExecutorStub {
        fn cid(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::CidParams,
            mut results: system_capnp::executor::CidResults,
        ) -> capnp::capability::Promise<(), capnp::Error> {
            results
                .get()
                .set_cid("bafkr4if3s6yv23hd3hgfvftj2g2uwdrqazv53p36p5lqyy7n77d5t5p54a");
            capnp::capability::Promise::ok(())
        }
    }

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

    #[test]
    fn external_graft_names_remain_exactly_unchanged() {
        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(test_epoch(1));
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
            Vec::new(),
            runtime,
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
        );
        let mut message = capnp::message::Builder::new_default();
        let mut cap_table = Vec::new();
        {
            let mut results =
                message.init_root::<membrane_capnp::membrane::graft_results::Builder<'_>>();
            results.imbue_mut(&mut cap_table);
            builder
                .build(&guard, results)
                .expect("build external graft");
        }
        let mut results = message
            .get_root_as_reader::<membrane_capnp::membrane::graft_results::Reader<'_>>()
            .expect("read external graft");
        results.imbue(&cap_table);
        let names: Vec<_> = results
            .get_caps()
            .expect("external caps")
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
        assert_eq!(
            names,
            [
                "identity",
                "host",
                "runtime",
                "routing",
                "authority",
                "ipfs"
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn foreign_export_graft_cannot_retarget_pid0_readiness() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (epoch_tx, epoch_rx) = watch::channel(test_epoch(1));
                let activated_seq = Arc::new(AtomicU64::new(0));
                let split = split_test_membranes(epoch_rx, activated_seq.clone());

                split
                    .root
                    .graft_request()
                    .send()
                    .promise
                    .await
                    .expect("bind local PID0 E1 graft");
                epoch_tx.send_replace(test_epoch(2));
                split
                    .export
                    .graft_request()
                    .send()
                    .promise
                    .await
                    .expect("foreign ordinary E2 graft");

                assert_eq!(
                    split.readiness_gate.kernel_ready(),
                    Err(KernelReadyError::StaleGeneration)
                );
                assert_eq!(activated_seq.load(Ordering::Acquire), 0);
                assert_eq!(split.root_grafts.get(), 1);
                assert_eq!(split.export_grafts.get(), 1);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_pid0_regraft_rebinds_readiness_idempotently() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (epoch_tx, epoch_rx) = watch::channel(test_epoch(1));
                let activated_seq = Arc::new(AtomicU64::new(0));
                let split = split_test_membranes(epoch_rx, activated_seq.clone());

                split.root.graft_request().send().promise.await.unwrap();
                epoch_tx.send_replace(test_epoch(2));
                assert_eq!(
                    split.readiness_gate.kernel_ready(),
                    Err(KernelReadyError::StaleGeneration)
                );
                split.root.graft_request().send().promise.await.unwrap();

                assert_eq!(activated_seq.load(Ordering::Acquire), 0);

                for _ in 0..2 {
                    split
                        .readiness_gate
                        .kernel_ready()
                        .expect("duplicate current E2 commit is idempotent");
                }
                assert_eq!(activated_seq.load(Ordering::Acquire), 2);

                epoch_tx.send_replace(test_epoch(3));
                assert_eq!(
                    split.readiness_gate.kernel_ready(),
                    Err(KernelReadyError::StaleGeneration)
                );
                assert_eq!(activated_seq.load(Ordering::Acquire), 2);
                assert_eq!(split.root_grafts.get(), 2);
                assert_eq!(split.export_grafts.get(), 0);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retained_issuing_host_cannot_keep_previous_registration_live() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let epoch = Epoch {
                    seq: 1,
                    head: b"pid0-session".to_vec(),
                    provenance: Provenance::Block(1),
                };
                let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(epoch);
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: epoch_rx,
                };
                let (swarm_tx, _swarm_rx) = mpsc::channel(1);
                let registry = crate::dispatch::new_registry();
                let (registration_scope, registration_scope_rx) = Pid0RegistrationScope::new();
                let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
                let builder = HostGraftBuilder::new(
                    NetworkState::from_peer_id(vec![1, 2, 3]),
                    swarm_tx,
                    false,
                    None,
                    libp2p_stream::Behaviour::new().new_control(),
                    Vec::new(),
                    runtime,
                    ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                )
                .with_route_registry(registry.clone())
                .with_registration_scope(registration_scope_rx);

                let mut first_message = capnp::message::Builder::new_default();
                let mut first_cap_table = Vec::new();
                {
                    let mut results = first_message
                        .init_root::<membrane_capnp::membrane::graft_results::Builder<'_>>();
                    results.imbue_mut(&mut first_cap_table);
                    builder.build(&guard, results).expect("build first graft");
                }
                let mut first_results = first_message
                    .get_root_as_reader::<membrane_capnp::membrane::graft_results::Reader<'_>>()
                    .expect("read first graft");
                first_results.imbue(&first_cap_table);
                let first_caps =
                    crate::decode_exports(first_results.get_caps().expect("first graft caps"))
                        .expect("decode first graft caps");
                let host = first_caps
                    .iter()
                    .find(|entry| entry.name() == "host")
                    .map(|entry| system_capnp::host::Client {
                        client: entry.capability().clone(),
                    })
                    .expect("first graft host");

                let network = host
                    .network_request()
                    .send()
                    .promise
                    .await
                    .expect("network request")
                    .get()
                    .expect("network response")
                    .get_http_listener()
                    .expect("HTTP listener");
                let retained_host =
                    NamedCapabilities::try_from_pairs([("issuing-host", host.clone().client)])
                        .expect("retained issuing host grant");
                let executor: system_capnp::executor::Client = capnp_rpc::new_client(ExecutorStub);
                let mut listen = network.listen_request();
                listen.get().set_executor(executor);
                listen.get().set_prefix("/status");
                crate::encode_exports(
                    &retained_host,
                    listen.get().init_caps(retained_host.len() as u32),
                )
                .expect("encode retained host");
                listen
                    .send()
                    .promise
                    .await
                    .expect("register first graft route");
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while crate::dispatch::live_route_count(&registry) != Ok(1) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("route target readiness preflight");
                assert_eq!(crate::dispatch::live_route_count(&registry), Ok(1));

                // Readiness probes and external clients may perform additional
                // grafts during one pid0 generation. Those grafts must not
                // invalidate the generation's live route.
                let mut replacement_message = capnp::message::Builder::new_default();
                let mut replacement_cap_table = Vec::new();
                {
                    let mut results = replacement_message
                        .init_root::<membrane_capnp::membrane::graft_results::Builder<'_>>(
                    );
                    results.imbue_mut(&mut replacement_cap_table);
                    builder
                        .build(&guard, results)
                        .expect("build replacement graft");
                }
                assert_eq!(
                    crate::dispatch::live_route_count(&registry),
                    Ok(1),
                    "an unrelated graft must not invalidate pid0 registrations"
                );

                // Failed init or pid0 exit drops the execution-generation
                // owner. The route retains its issuing Host as a grant, but
                // that back-reference owns only a receiver and therefore
                // cannot prolong readiness.
                drop(registration_scope);
                assert_eq!(
                    crate::dispatch::live_route_count(&registry),
                    Ok(0),
                    "a retained issuing Host must not keep the old session ready"
                );

                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while !registry.read().expect("registry lock").is_empty() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("old registration cleanup");
            })
            .await;
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
                let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(epoch);
                let (swarm_tx, _swarm_rx) = mpsc::channel(1);
                let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
                let (host_stream, guest_stream) = io::duplex(16 * 1024);
                let (host_reader, host_writer) = io::split(host_stream);
                let (guest_reader, guest_writer) = io::split(guest_stream);
                let activated_seq = Arc::new(AtomicU64::new(0));
                let readiness_gate = Arc::new(authority::KernelReadyGate::new(
                    epoch_rx.clone(),
                    activated_seq.clone(),
                ));

                let (host_rpc, _guest_export, _registration_scope) = build_pid0_membrane_rpc(
                    host_reader,
                    host_writer,
                    NetworkState::from_peer_id(vec![1, 2, 3]),
                    swarm_tx,
                    false,
                    epoch_rx,
                    readiness_gate.clone(),
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
                    .expect("process-local PID0 graft RPC");
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
                    PID0_EXPORT_MEMBRANE_CAP,
                ] {
                    assert!(
                        names.contains(expected),
                        "pid0 RPC bootstrap lost {expected}: {names:?}"
                    );
                }
                assert_eq!(names.len(), 8);
                readiness_gate
                    .kernel_ready()
                    .expect("commit current pid0 generation");
                assert_eq!(activated_seq.load(Ordering::Acquire), 1);

                let caps = response.get().unwrap().get_caps().unwrap();
                let export = caps
                    .iter()
                    .find(|entry| {
                        entry.get_name().unwrap().to_str().unwrap() == PID0_EXPORT_MEMBRANE_CAP
                    })
                    .expect("process-local graft exports safe membrane");
                let export_membrane = GuestMembrane {
                    client: export
                        .get_cap()
                        .get_as_capability::<capnp::capability::Client>()
                        .unwrap(),
                };
                epoch_tx.send_replace(test_epoch(2));
                let export_response = export_membrane
                    .graft_request()
                    .send()
                    .promise
                    .await
                    .expect("export-safe E2 graft");
                let export_names: std::collections::HashSet<_> = export_response
                    .get()
                    .unwrap()
                    .get_caps()
                    .unwrap()
                    .iter()
                    .map(|entry| entry.get_name().unwrap().to_str().unwrap().to_owned())
                    .collect();
                assert!(
                    !export_names.contains(PID0_EXPORT_MEMBRANE_CAP),
                    "the process-local membrane handoff must not leak into public grafts"
                );
                assert_eq!(
                    readiness_gate.kernel_ready(),
                    Err(KernelReadyError::StaleGeneration),
                    "export-safe graft must not rebind PID0 readiness"
                );
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
