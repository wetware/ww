//! Membrane-based RPC bootstrap for typed epoch-scoped authority.
//!
//! `graft()` returns stable peer metadata, live node status, grouped network
//! and routing references, and other typed authority directly. Host-issued
//! capabilities fail with `staleEpoch` when the epoch advances.
//!
//! The `authority` crate owns the Membrane server and epoch machinery.
//! This module provides the `GraftBuilder` impl that injects wetware-specific
//! capabilities into the graft response, plus the epoch-guarded identity wrapper.

use std::sync::Arc;

use authority::{auth_capnp, Epoch, EpochGuard, GraftBuilder, MembraneServer};
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

use crate::{ByteStreamImpl, NamedCapabilities, StreamMode, SwarmCommand};
use auth::SigningDomain;
use authority::http_capnp;
use authority::routing_capnp;
use authority::system_capnp;

use super::NetworkState;

/// Stable structured event codes for host-owned PID0 lifecycle events.
///
/// Integer values are an external log contract. Never renumber or reuse one.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelEventCode {
    InitialInitFailed = 1,
    EpochRestartInitFailed = 2,
    GenerationReplaced = 3,
    GraftSuperseded = 4,
    InteractiveReplaced = 5,
    TeardownTimeout = 6,
}

// ---------------------------------------------------------------------------
// EpochGuardedIdentity — host-side node identity hub
// ---------------------------------------------------------------------------

/// Host-side node identity hub provided through the root Membrane.
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
    /// Pre-converted libp2p keypair built once for each root graft.
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
// EpochGuardedStat — coherent node observations without peer topology
// ---------------------------------------------------------------------------
struct EpochGuardedStat {
    network_state: NetworkState,
    guard: EpochGuard,
}

#[allow(refining_impl_trait)]
impl system_capnp::stat::Server for EpochGuardedStat {
    fn snapshot(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::stat::SnapshotParams,
        mut results: system_capnp::stat::SnapshotResults,
    ) -> Promise<(), capnp::Error> {
        if let Err(error) = self.guard.check() {
            return Promise::err(error);
        }
        let network_state = self.network_state.clone();
        let guard = self.guard.clone();
        Promise::from_future(async move {
            let snapshot = network_state.snapshot().await;
            guard.check()?;
            let mut stat = results.get().init_stat();
            let mut addrs = stat
                .reborrow()
                .init_listen_addrs(snapshot.listen_addrs.len() as u32);
            for (index, addr) in snapshot.listen_addrs.iter().enumerate() {
                addrs.set(index as u32, addr);
            }
            stat.set_connected_peer_count(snapshot.connected_peer_count);
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// RootMembraneBuilder — root authority construction
// ---------------------------------------------------------------------------

/// Builds the root Membrane's epoch-guarded typed authority.
///
/// **Runtime singleton**: the builder holds a pre-created `runtime::Client` that
/// points to a single `RuntimeImpl` backend. Every graft clones this client, so
/// every graft recipient that receives this reference shares the same
/// compilation and executor cache.
#[derive(Clone)]
pub struct RootMembraneBuilder {
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    signing_key: Option<Arc<SigningKey>>,
    stream_control: libp2p_stream::Control,
    allowed_hosts: Vec<String>,
    route_registry: Option<crate::dispatch::RouteRegistry>,
    /// Pre-created Runtime client (singleton — same backend for every graft).
    runtime_client: system_capnp::runtime::Client,
    /// Application-defined named capabilities configured for `extras`.
    extras: NamedCapabilities,
    /// IPFS HTTP client for Kubo API calls (e.g. IPNS resolution).
    ipfs_client: ipfs::HttpClient,
    /// Host-internal view of the pid0 execution-generation lifetime. Every
    /// graft for that generation shares this receiver; unrelated graft calls
    /// therefore cannot invalidate pid0 registrations.
    registration_scope: Option<watch::Receiver<()>>,
}

impl RootMembraneBuilder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        network_state: NetworkState,
        swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
        signing_key: Option<Arc<SigningKey>>,
        stream_control: libp2p_stream::Control,
        allowed_hosts: Vec<String>,
        runtime_client: system_capnp::runtime::Client,
        ipfs_client: ipfs::HttpClient,
    ) -> Self {
        Self {
            network_state,
            swarm_cmd_tx,
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

    /// Set additional named capabilities to inject into the graft.
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

impl RootMembraneBuilder {
    fn build_graft(
        &self,
        guard: &EpochGuard,
        mut builder: system_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), capnp::Error> {
        builder.set_peer_id(self.network_state.local_peer_id());

        let stat: system_capnp::stat::Client = capnp_rpc::new_client(EpochGuardedStat {
            network_state: self.network_state.clone(),
            guard: guard.clone(),
        });
        builder.set_stat(stat);

        let stream_listener: system_capnp::stream_listener::Client =
            capnp_rpc::new_client(super::stream_listener::StreamListenerImpl::new(
                self.stream_control.clone(),
                guard.clone(),
            ));
        let stream_dialer: system_capnp::stream_dialer::Client = capnp_rpc::new_client(
            super::stream_dialer::StreamDialerImpl::new(self.stream_control.clone(), guard.clone()),
        );
        let vat_listener: system_capnp::vat_listener::Client = capnp_rpc::new_client(
            super::vat_listener::VatListenerImpl::new(self.stream_control.clone(), guard.clone()),
        );
        let vat_dialer: system_capnp::vat_client::Client = capnp_rpc::new_client(
            super::vat_client::VatClientImpl::new(self.stream_control.clone(), guard.clone()),
        );
        let http_listener: Option<system_capnp::http_listener::Client> =
            match (&self.route_registry, self.registration_scope.clone()) {
                (Some(registry), Some(scope)) => Some(capnp_rpc::new_client(
                    super::http_listener::HttpListenerImpl::new_scoped(
                        guard.clone(),
                        registry.clone(),
                        scope,
                    ),
                )),
                (Some(registry), None) => Some(capnp_rpc::new_client(
                    super::http_listener::HttpListenerImpl::new(guard.clone(), registry.clone()),
                )),
                (None, _) => None,
            };
        let http_dialer: Option<http_capnp::http_client::Client> = (!self.allowed_hosts.is_empty())
            .then(|| {
                capnp_rpc::new_client(super::http_client::EpochGuardedHttpProxy::new(
                    self.allowed_hosts.clone(),
                    guard.clone(),
                ))
            });

        {
            let mut network = builder.reborrow().init_network();
            let mut stream = network.reborrow().init_stream();
            stream.set_listener(stream_listener);
            stream.set_dialer(stream_dialer);
            let mut vat = network.reborrow().init_vat();
            vat.set_listener(vat_listener);
            vat.set_dialer(vat_dialer);
            let mut http = network.init_http();
            if let Some(listener) = http_listener {
                http.set_listener(listener);
            }
            if let Some(dialer) = http_dialer {
                http.set_dialer(dialer);
            }
        }

        let finder: routing_capnp::finder::Client = capnp_rpc::new_client(
            super::routing::FinderImpl::new(self.swarm_cmd_tx.clone(), guard.clone()),
        );
        let mut announcer_impl =
            super::routing::AnnouncerImpl::new(self.swarm_cmd_tx.clone(), guard.clone());
        if let Some(scope) = self.registration_scope.clone() {
            announcer_impl = announcer_impl.with_registration_scope(scope);
        }
        let announcer: routing_capnp::announcer::Client = capnp_rpc::new_client(announcer_impl);
        let mut routing = builder.reborrow().init_routing();
        routing.set_finder(finder);
        routing.set_announcer(announcer);

        builder.set_runtime(self.runtime_client.clone());
        let authority: auth_capnp::authority::Client =
            capnp_rpc::new_client(authority::AuthorityServer::new(guard.clone()));
        builder.set_authority(authority);

        if let Some(signing_key) = &self.signing_key {
            let keypair = crate::keys::to_libp2p(signing_key)
                .map_err(|error| capnp::Error::failed(error.to_string()))?;
            let identity: auth_capnp::identity::Client =
                capnp_rpc::new_client(EpochGuardedIdentity::new(keypair, guard.clone()));
            builder.set_identity(identity);
        }

        let ipfs: system_capnp::ipfs::Client = capnp_rpc::new_client(EpochGuardedIpfs {
            guard: guard.clone(),
            ipfs_client: self.ipfs_client.clone(),
        });
        builder.set_ipfs(ipfs);

        let extras = builder.reborrow().init_extras(self.extras.len() as u32);
        crate::encode_exports(&self.extras, extras)
    }
}

impl GraftBuilder for RootMembraneBuilder {
    fn build(
        &self,
        guard: &EpochGuard,
        builder: system_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), capnp::Error> {
        self.build_graft(guard, builder)
    }
}

// IPFS content access goes through the WASI virtual filesystem (CidTree).
// See src/vfs.rs and src/fs_intercept.rs.

// ---------------------------------------------------------------------------
// RPC bootstrap constructors
// ---------------------------------------------------------------------------

/// A graft-capable Membrane client used by the host and guest RPC constructors.
pub type GuestMembrane = system_capnp::membrane::Client;

/// Guest-exported capability imported by the host and exposed only through the
/// parent-held `Process.bootstrap()` operation.
pub type GuestExport = capnp::capability::Client;

/// Build the ordinary-child RPC path around the exact delegated authority.
#[doc(hidden)]
pub fn build_child_membrane_rpc<R, W>(
    reader: R,
    writer: W,
    membrane: GuestMembrane,
) -> (RpcSystem<Side>, GuestExport)
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let rpc_network = VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        Side::Server,
        Default::default(),
    );
    let mut rpc_system = RpcSystem::new(Box::new(rpc_network), Some(membrane.client));
    let guest_export: GuestExport = rpc_system.bootstrap(Side::Client);
    (rpc_system, guest_export)
}

/// Build the trusted pid0 RPC system with its full graft-capable `Membrane`.
///
/// The membrane provides epoch-scoped typed authority and, when configured, a
/// host-side node identity signer.
///
/// When `signing_key` is `Some`, an [`EpochGuardedIdentity`] hub is injected into
/// every graft so the kernel can request domain-scoped signers without holding
/// the private key. Auth (if needed) is handled by wrapping in `TerminalServer`
/// at the transport layer, not here.
///
/// Process-local graft wrapper. Only the trusted PID0 bootstrap receives a
/// client for this server.
struct KernelRootGraftBuilder {
    inner: RootMembraneBuilder,
    readiness_gate: Arc<authority::KernelReadyGate>,
    intended_seq: u64,
}

impl GraftBuilder for KernelRootGraftBuilder {
    fn build(
        &self,
        guard: &EpochGuard,
        builder: system_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), capnp::Error> {
        let live_epoch = guard.receiver.borrow();
        if live_epoch.seq != self.intended_seq {
            tracing::warn!(
                event_code = KernelEventCode::GraftSuperseded as u16,
                intended_seq = self.intended_seq,
                live_seq = live_epoch.seq,
                "PID0 root graft superseded before capability issuance"
            );
            return Err(capnp::Error::failed(format!(
                "PID0 generation {} was superseded by generation {}",
                self.intended_seq, live_epoch.seq
            )));
        }
        let intended_guard = EpochGuard {
            issued_seq: self.intended_seq,
            receiver: guard.receiver.clone(),
        };
        self.inner.build_graft(&intended_guard, builder)?;
        self.readiness_gate.bind_generation(self.intended_seq);
        drop(live_epoch);
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_kernel_membrane_rpc<R, W>(
    reader: R,
    writer: W,
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    epoch_rx: watch::Receiver<Epoch>,
    readiness_gate: Arc<authority::KernelReadyGate>,
    signing_key: Option<Arc<SigningKey>>,
    stream_control: libp2p_stream::Control,
    route_registry: Option<crate::dispatch::RouteRegistry>,
    runtime_client: system_capnp::runtime::Client,
    extras: NamedCapabilities,
    ipfs_client: ipfs::HttpClient,
    http_dial: Vec<String>,
    intended_seq: u64,
    registration_scope: watch::Receiver<()>,
) -> RpcSystem<Side>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut root_builder = RootMembraneBuilder::new(
        network_state,
        swarm_cmd_tx,
        signing_key,
        stream_control,
        http_dial,
        runtime_client,
        ipfs_client,
    )
    .with_registration_scope(registration_scope);
    if !extras.is_empty() {
        root_builder = root_builder.with_extras(extras);
    }
    if let Some(registry) = route_registry {
        root_builder = root_builder.with_route_registry(registry);
    }

    // PID0 receives a process-local root membrane whose graft binds readiness.
    let root_builder = KernelRootGraftBuilder {
        inner: root_builder,
        readiness_gate,
        intended_seq,
    };
    let root_membrane: GuestMembrane =
        capnp_rpc::new_client(MembraneServer::new(epoch_rx, root_builder));

    let rpc_network = VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        Side::Server,
        Default::default(),
    );
    RpcSystem::new(Box::new(rpc_network), Some(root_membrane.client))
}

// IPFS content access is tested in fs_intercept::tests and vfs::tests.

#[cfg(test)]
mod tests {
    use super::*;
    use authority::{Epoch, KernelReadyError};
    use capnp::traits::{Imbue, ImbueMut};
    use ed25519_dalek::Signer;
    use futures::FutureExt;
    use std::cell::Cell;
    use std::rc::Rc;

    struct RuntimeStub;
    impl system_capnp::runtime::Server for RuntimeStub {}

    struct StatefulMembrane {
        grafts: Rc<Cell<u32>>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::membrane::Server for StatefulMembrane {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::membrane::GraftParams,
            mut results: system_capnp::membrane::GraftResults,
        ) -> capnp::capability::Promise<(), capnp::Error> {
            let graft = self.grafts.get() + 1;
            self.grafts.set(graft);
            results.get().set_peer_id(&graft.to_be_bytes());
            capnp::capability::Promise::ok(())
        }
    }

    struct PendingMembrane {
        grafts: Rc<Cell<u32>>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::membrane::Server for PendingMembrane {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::membrane::GraftParams,
            _results: system_capnp::membrane::GraftResults,
        ) -> capnp::capability::Promise<(), capnp::Error> {
            self.grafts.set(self.grafts.get() + 1);
            capnp::capability::Promise::from_future(std::future::pending())
        }
    }

    struct FailingMembrane {
        grafts: Rc<Cell<u32>>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::membrane::Server for FailingMembrane {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::membrane::GraftParams,
            _results: system_capnp::membrane::GraftResults,
        ) -> capnp::capability::Promise<(), capnp::Error> {
            self.grafts.set(self.grafts.get() + 1);
            capnp::capability::Promise::err(capnp::Error::failed(
                "supplied-membrane-failure".into(),
            ))
        }
    }

    fn forward_child_membrane(
        supplied: system_capnp::membrane::Client,
    ) -> system_capnp::membrane::Client {
        let (host_stream, guest_stream) = io::duplex(16 * 1024);
        let (host_reader, host_writer) = io::split(host_stream);
        let (guest_reader, guest_writer) = io::split(guest_stream);
        let (host_rpc, _guest_export) =
            build_child_membrane_rpc(host_reader, host_writer, supplied);
        tokio::task::spawn_local(host_rpc.map(|_| ()));

        let guest_network = VatNetwork::new(
            guest_reader.compat(),
            guest_writer.compat_write(),
            Side::Client,
            Default::default(),
        );
        let mut guest_rpc = RpcSystem::new(Box::new(guest_network), None);
        let membrane = guest_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(guest_rpc.map(|_| ()));
        membrane
    }

    #[derive(Clone)]
    struct CountingGraftBuilder {
        grafts: Rc<Cell<u32>>,
    }

    impl GraftBuilder for CountingGraftBuilder {
        fn build(
            &self,
            _guard: &EpochGuard,
            mut builder: system_capnp::membrane::graft_results::Builder<'_>,
        ) -> Result<(), capnp::Error> {
            self.grafts.set(self.grafts.get() + 1);
            builder.set_peer_id(b"test-peer");
            builder.reborrow().init_extras(0);
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
            builder: system_capnp::membrane::graft_results::Builder<'_>,
        ) -> Result<(), capnp::Error> {
            self.readiness_gate.bind_generation(guard.issued_seq);
            self.inner.build(guard, builder)
        }
    }

    fn test_epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: seq.to_be_bytes().to_vec(),
            root: None,
        }
    }

    struct SplitTestMembranes {
        root: GuestMembrane,
        export: GuestMembrane,
        readiness_gate: Arc<authority::KernelReadyGate>,
        root_grafts: Rc<Cell<u32>>,
        export_grafts: Rc<Cell<u32>>,
    }

    fn split_test_membranes(epoch_rx: watch::Receiver<Epoch>) -> SplitTestMembranes {
        let root_grafts = Rc::new(Cell::new(0));
        let export_grafts = Rc::new(Cell::new(0));
        let readiness_gate = Arc::new(authority::KernelReadyGate::new(epoch_rx.clone()));
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
            root: None,
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
    fn root_builder_emits_typed_platform_authority() {
        let epoch = Epoch {
            seq: 1,
            head: b"pid0".to_vec(),
            root: None,
        };
        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(epoch);
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: epoch_rx,
        };
        let (swarm_tx, _swarm_rx) = mpsc::channel(1);
        let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
        let builder = RootMembraneBuilder::new(
            NetworkState::from_peer_id(vec![1, 2, 3]),
            swarm_tx,
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
                message.init_root::<system_capnp::membrane::graft_results::Builder<'_>>();
            results.imbue_mut(&mut cap_table);
            builder.build(&guard, results).expect("build pid0 graft");
        }
        let mut results = message
            .get_root_as_reader::<system_capnp::membrane::graft_results::Reader<'_>>()
            .expect("read pid0 graft");
        results.imbue(&cap_table);
        assert_eq!(results.get_peer_id().expect("peer ID"), &[1, 2, 3]);
        assert!(results.has_stat());
        assert!(results.has_runtime());
        assert!(results.has_authority());
        assert!(results.has_identity());
        assert!(results.has_ipfs());
        let network = results.get_network().expect("network");
        assert!(network.get_stream().has_listener());
        assert!(network.get_stream().has_dialer());
        assert!(network.get_vat().has_listener());
        assert!(network.get_vat().has_dialer());
        assert!(network.get_http().has_dialer());
        let routing = results.get_routing().expect("routing");
        assert!(routing.has_finder());
        assert!(routing.has_announcer());
        assert_eq!(results.get_extras().expect("extras").len(), 0);
    }

    #[test]
    fn root_builder_keeps_application_extras_separate_from_typed_authority() {
        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(test_epoch(1));
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: epoch_rx,
        };
        let (swarm_tx, _swarm_rx) = mpsc::channel(1);
        let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
        let extra_runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
        let builder = RootMembraneBuilder::new(
            NetworkState::from_peer_id(vec![1, 2, 3]),
            swarm_tx,
            Some(Arc::new(gen_signing_key())),
            libp2p_stream::Behaviour::new().new_control(),
            Vec::new(),
            runtime,
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
        )
        .with_extras(
            NamedCapabilities::try_from_pairs([("application-extra", extra_runtime.client)])
                .expect("dynamic extra"),
        );
        let mut message = capnp::message::Builder::new_default();
        let mut cap_table = Vec::new();
        {
            let mut results =
                message.init_root::<system_capnp::membrane::graft_results::Builder<'_>>();
            results.imbue_mut(&mut cap_table);
            builder
                .build(&guard, results)
                .expect("build external graft");
        }
        let mut results = message
            .get_root_as_reader::<system_capnp::membrane::graft_results::Reader<'_>>()
            .expect("read external graft");
        results.imbue(&cap_table);
        let names: Vec<_> = results
            .get_extras()
            .expect("application extras")
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
        assert_eq!(names, ["application-extra"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stat_snapshot_reports_mutable_counts_and_becomes_stale() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let state = NetworkState::from_peer_id(b"secret-peer-marker".to_vec());
                state.add_listen_addr(vec![1, 2, 3]).await;
                state.set_connected_peer_count(7).await;
                let (epoch_tx, epoch_rx) = watch::channel(test_epoch(1));
                let stat: system_capnp::stat::Client = capnp_rpc::new_client(EpochGuardedStat {
                    network_state: state,
                    guard: EpochGuard {
                        issued_seq: 1,
                        receiver: epoch_rx,
                    },
                });

                let response = stat
                    .snapshot_request()
                    .send()
                    .promise
                    .await
                    .expect("current Stat snapshot");
                let snapshot = response
                    .get()
                    .expect("snapshot results")
                    .get_stat()
                    .expect("NodeStat");
                assert_eq!(snapshot.get_connected_peer_count(), 7);
                let addrs = snapshot.get_listen_addrs().expect("listen addresses");
                assert_eq!(addrs.len(), 1);
                assert_eq!(addrs.get(0).expect("listen address"), &[1, 2, 3]);

                epoch_tx.send_replace(test_epoch(2));
                assert!(stat.snapshot_request().send().promise.await.is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pid0_root_graft_keeps_readiness_bound_to_the_intended_generation() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (epoch_tx, epoch_rx) = watch::channel(test_epoch(1));
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: epoch_rx.clone(),
                };
                let readiness_gate = Arc::new(authority::KernelReadyGate::new(epoch_rx.clone()));
                let (swarm_tx, _swarm_rx) = mpsc::channel(1);
                let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
                let root = KernelRootGraftBuilder {
                    inner: RootMembraneBuilder::new(
                        NetworkState::from_peer_id(vec![1, 2, 3]),
                        swarm_tx,
                        None,
                        libp2p_stream::Behaviour::new().new_control(),
                        Vec::new(),
                        runtime,
                        ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                    ),
                    readiness_gate: readiness_gate.clone(),
                    intended_seq: 1,
                };
                let mut message = capnp::message::Builder::new_default();
                let mut cap_table = Vec::new();
                let mut results =
                    message.init_root::<system_capnp::membrane::graft_results::Builder<'_>>();
                results.imbue_mut(&mut cap_table);
                root.build(&guard, results)
                    .expect("build intended PID0 root graft");

                epoch_tx.send_replace(test_epoch(2));
                assert_eq!(
                    readiness_gate.kernel_ready(),
                    Err(KernelReadyError::StaleGeneration)
                );
                assert!(!readiness_gate.is_ready());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pid0_root_graft_fails_fast_when_the_intended_generation_is_superseded() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_epoch_tx, epoch_rx) = watch::channel(test_epoch(2));
                let guard = EpochGuard {
                    issued_seq: 2,
                    receiver: epoch_rx.clone(),
                };
                let readiness_gate = Arc::new(authority::KernelReadyGate::new(epoch_rx.clone()));
                let (swarm_tx, _swarm_rx) = mpsc::channel(1);
                let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
                let root = KernelRootGraftBuilder {
                    inner: RootMembraneBuilder::new(
                        NetworkState::from_peer_id(vec![1, 2, 3]),
                        swarm_tx,
                        None,
                        libp2p_stream::Behaviour::new().new_control(),
                        Vec::new(),
                        runtime,
                        ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                    ),
                    readiness_gate: readiness_gate.clone(),
                    intended_seq: 1,
                };
                let mut message = capnp::message::Builder::new_default();
                let mut cap_table = Vec::new();
                let error = {
                    let mut results =
                        message.init_root::<system_capnp::membrane::graft_results::Builder<'_>>();
                    results.imbue_mut(&mut cap_table);
                    root.build(&guard, results)
                        .expect_err("superseded PID0 root graft must fail")
                };
                assert!(error.to_string().contains("superseded"));
                assert_eq!(
                    readiness_gate.kernel_ready(),
                    Err(KernelReadyError::NotBound)
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn foreign_export_graft_cannot_retarget_pid0_readiness() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (epoch_tx, epoch_rx) = watch::channel(test_epoch(1));
                let split = split_test_membranes(epoch_rx);

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
                assert!(!split.readiness_gate.is_ready());
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
                let split = split_test_membranes(epoch_rx);

                split.root.graft_request().send().promise.await.unwrap();
                epoch_tx.send_replace(test_epoch(2));
                assert_eq!(
                    split.readiness_gate.kernel_ready(),
                    Err(KernelReadyError::StaleGeneration)
                );
                split.root.graft_request().send().promise.await.unwrap();

                assert!(!split.readiness_gate.is_ready());

                for _ in 0..2 {
                    split
                        .readiness_gate
                        .kernel_ready()
                        .expect("duplicate current E2 commit is idempotent");
                }
                assert!(split.readiness_gate.is_ready());

                epoch_tx.send_replace(test_epoch(3));
                assert_eq!(
                    split.readiness_gate.kernel_ready(),
                    Err(KernelReadyError::StaleGeneration)
                );
                assert!(!split.readiness_gate.is_ready());
                assert_eq!(split.root_grafts.get(), 2);
                assert_eq!(split.export_grafts.get(), 0);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retained_child_membrane_cannot_keep_previous_registration_live() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let epoch = Epoch {
                    seq: 1,
                    head: b"pid0-session".to_vec(),
                    root: None,
                };
                let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(epoch);
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: epoch_rx,
                };
                let (swarm_tx, _swarm_rx) = mpsc::channel(1);
                let registry = crate::dispatch::new_registry();
                let (registration_scope, registration_scope_rx) = watch::channel(());
                let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
                let builder = RootMembraneBuilder::new(
                    NetworkState::from_peer_id(vec![1, 2, 3]),
                    swarm_tx,
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
                        .init_root::<system_capnp::membrane::graft_results::Builder<'_>>();
                    results.imbue_mut(&mut first_cap_table);
                    builder.build(&guard, results).expect("build first graft");
                }
                let mut first_results = first_message
                    .get_root_as_reader::<system_capnp::membrane::graft_results::Reader<'_>>()
                    .expect("read first graft");
                first_results.imbue(&first_cap_table);
                let network = first_results
                    .get_network()
                    .expect("network")
                    .get_http()
                    .get_listener()
                    .expect("HTTP listener");
                let child_membrane =
                    authority::membrane_client(guard.receiver.clone(), b"test-peer");
                let executor: system_capnp::executor::Client = capnp_rpc::new_client(ExecutorStub);
                let mut listen = network.listen_request();
                listen.get().set_executor(executor);
                listen.get().set_prefix("/status");
                listen.get().set_membrane(child_membrane);
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
                .expect("route target preflight");
                assert_eq!(crate::dispatch::live_route_count(&registry), Ok(1));

                // External clients may perform additional grafts during one
                // pid0 generation. Those grafts must not invalidate the
                // generation's live route.
                let mut replacement_message = capnp::message::Builder::new_default();
                let mut replacement_cap_table = Vec::new();
                {
                    let mut results = replacement_message
                        .init_root::<system_capnp::membrane::graft_results::Builder<'_>>(
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
                // owner. The route retains its child Membrane, but that
                // capability cannot prolong route liveness.
                drop(registration_scope);
                assert_eq!(
                    crate::dispatch::live_route_count(&registry),
                    Ok(0),
                    "a retained child Membrane must not keep the old session ready"
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
                    root: None,
                };
                let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(epoch);
                let (swarm_tx, _swarm_rx) = mpsc::channel(1);
                let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RuntimeStub);
                let (host_stream, guest_stream) = io::duplex(16 * 1024);
                let (host_reader, host_writer) = io::split(host_stream);
                let (guest_reader, guest_writer) = io::split(guest_stream);
                let readiness_gate = Arc::new(authority::KernelReadyGate::new(epoch_rx.clone()));

                let (_registration_scope, registration_scope_rx) = watch::channel(());
                let host_rpc = build_kernel_membrane_rpc(
                    host_reader,
                    host_writer,
                    NetworkState::from_peer_id(vec![1, 2, 3]),
                    swarm_tx,
                    epoch_rx,
                    readiness_gate.clone(),
                    Some(Arc::new(gen_signing_key())),
                    libp2p_stream::Behaviour::new().new_control(),
                    None,
                    runtime,
                    NamedCapabilities::default(),
                    ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                    vec!["example.com".into()],
                    1,
                    registration_scope_rx,
                );
                tokio::task::spawn_local(host_rpc.map(|_| ()));

                let guest_network = VatNetwork::new(
                    guest_reader.compat(),
                    guest_writer.compat_write(),
                    Side::Client,
                    Default::default(),
                );
                let mut guest_rpc = RpcSystem::new(Box::new(guest_network), None);
                let membrane: system_capnp::membrane::Client = guest_rpc.bootstrap(Side::Server);
                tokio::task::spawn_local(guest_rpc.map(|_| ()));

                let response = membrane
                    .graft_request()
                    .send()
                    .promise
                    .await
                    .expect("process-local PID0 graft RPC");
                let graft = response.get().expect("pid0 graft results");
                assert_eq!(graft.get_peer_id().expect("peer ID"), &[1, 2, 3]);
                assert!(graft.has_stat());
                assert!(graft.has_runtime());
                assert!(graft.has_authority());
                assert!(graft.has_identity());
                assert!(graft.has_ipfs());
                let network = graft.get_network().expect("network");
                assert!(network.get_stream().has_listener());
                assert!(network.get_vat().has_dialer());
                assert!(network.get_http().has_dialer());
                assert_eq!(graft.get_extras().expect("extras").len(), 0);
                readiness_gate
                    .kernel_ready()
                    .expect("commit current pid0 generation");
                assert!(readiness_gate.is_ready());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_child_rpc_serves_a_narrow_membrane() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host_stream, guest_stream) = io::duplex(16 * 1024);
                let (host_reader, host_writer) = io::split(host_stream);
                let (guest_reader, guest_writer) = io::split(guest_stream);

                let epoch = test_epoch(1);
                let (_epoch_tx, epoch_rx) = watch::channel(epoch);
                let child_membrane = authority::membrane_client(epoch_rx, b"test-peer");
                let (host_rpc, _guest_export) =
                    build_child_membrane_rpc(host_reader, host_writer, child_membrane);
                tokio::task::spawn_local(host_rpc.map(|_| ()));

                let guest_network = VatNetwork::new(
                    guest_reader.compat(),
                    guest_writer.compat_write(),
                    Side::Client,
                    Default::default(),
                );
                let mut guest_rpc = RpcSystem::new(Box::new(guest_network), None);
                let membrane: system_capnp::membrane::Client = guest_rpc.bootstrap(Side::Server);
                tokio::task::spawn_local(guest_rpc.map(|_| ()));

                let response = membrane
                    .graft_request()
                    .send()
                    .promise
                    .await
                    .expect("child Membrane.graft RPC");
                let graft = response.get().expect("child graft results");
                assert_eq!(graft.get_peer_id().expect("required peerId"), b"test-peer");
                assert!(!graft.has_stat());
                assert!(!graft.has_runtime());
                assert!(!graft.has_authority());
                assert!(!graft.has_identity());
                assert!(!graft.has_ipfs());
                assert_eq!(graft.get_extras().expect("child extras").len(), 0);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn child_rpc_forwards_the_stateful_supplied_membrane_without_eager_graft() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let grafts = Rc::new(Cell::new(0));
                let supplied: system_capnp::membrane::Client =
                    capnp_rpc::new_client(StatefulMembrane {
                        grafts: grafts.clone(),
                    });
                let child = forward_child_membrane(supplied);

                assert_eq!(grafts.get(), 0, "child bootstrap must not inspect Membrane");
                for expected in 1_u32..=2 {
                    let response = child
                        .graft_request()
                        .send()
                        .promise
                        .await
                        .expect("forwarded graft");
                    assert_eq!(
                        response
                            .get()
                            .expect("forwarded graft results")
                            .get_peer_id()
                            .expect("stateful peerId"),
                        &expected.to_be_bytes()
                    );
                }
                assert_eq!(
                    grafts.get(),
                    2,
                    "each child graft must reach the supplied Membrane server"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn child_rpc_keeps_a_supplied_membrane_pending_until_the_child_grafts() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let grafts = Rc::new(Cell::new(0));
                let supplied: system_capnp::membrane::Client =
                    capnp_rpc::new_client(PendingMembrane {
                        grafts: grafts.clone(),
                    });
                let child = forward_child_membrane(supplied);

                assert_eq!(grafts.get(), 0, "child bootstrap must not inspect Membrane");
                let result = tokio::time::timeout(
                    std::time::Duration::from_millis(25),
                    child.graft_request().send().promise,
                )
                .await;
                assert!(
                    result.is_err(),
                    "supplied pending graft must remain pending"
                );
                assert_eq!(grafts.get(), 1);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn child_rpc_preserves_a_supplied_membrane_failure() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let grafts = Rc::new(Cell::new(0));
                let supplied: system_capnp::membrane::Client =
                    capnp_rpc::new_client(FailingMembrane {
                        grafts: grafts.clone(),
                    });
                let child = forward_child_membrane(supplied);

                assert_eq!(grafts.get(), 0, "child bootstrap must not inspect Membrane");
                let error = match child.graft_request().send().promise.await {
                    Ok(_) => panic!("supplied failing graft must fail"),
                    Err(error) => error,
                };
                assert!(error.to_string().contains("supplied-membrane-failure"));
                assert_eq!(grafts.get(), 1);
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
                    root: None,
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
