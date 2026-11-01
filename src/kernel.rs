//! Host-side prepared kernel generations and runtime identity.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use authority::EpochGuard;
use cid::Cid;
use ed25519_dalek::SigningKey;
use tokio::io::{stderr, stdout, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tracing::Instrument;

use crate::cell::{Builder, Program};
use crate::host::SwarmCommand;
use crate::services::CompileRequest;

mod compilation;
pub use compilation::Compilation;
mod preparation;
#[cfg(test)]
pub(crate) use preparation::test_support;
pub use preparation::PreparedKernel;

/// Whether PID0 owns an interactive terminal for this invocation.
///
/// `WW_TTY` remains the test and compatibility override for stdin that is not
/// reported as a terminal by the host process.
pub fn is_interactive() -> bool {
    use std::io::IsTerminal;

    std::io::stdin().is_terminal() || std::env::var("WW_TTY").is_ok()
}

/// Content identity computed during preparation, before a generation runs.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Identity {
    pub cid: String,
    pub source: String,
    pub wasm_blake3: String,
    pub source_cid: Option<String>,
    pub size: usize,
}

/// Identity of a running generation, distinct from its readiness state.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct RunningIdentity {
    #[serde(flatten)]
    pub content: Identity,
    pub epoch_seq: u64,
    pub effective_root: Cid,
}

#[derive(Debug)]
struct PublishedIdentity {
    identity: RunningIdentity,
    owner: Arc<()>,
}

/// Shared identity state keeps `/version` available before Kubo and PID0.
/// Reads hide any snapshot that no longer matches the authoritative epoch.
#[derive(Clone, Debug)]
pub struct IdentityState {
    epoch_rx: watch::Receiver<authority::Epoch>,
    current: Arc<Mutex<Option<PublishedIdentity>>>,
}

impl IdentityState {
    pub fn pending(epoch_rx: watch::Receiver<authority::Epoch>) -> Self {
        Self {
            epoch_rx,
            current: Arc::new(Mutex::new(None)),
        }
    }

    pub fn pending_source(&self) -> String {
        "<pending: /boot/kernel.wasm>".to_owned()
    }

    pub fn get(&self) -> Option<RunningIdentity> {
        // Keep the epoch read fence until the coherent identity is copied.
        let epoch = self.epoch_rx.borrow();
        let current = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        current.as_ref().and_then(|published| {
            let identity = &published.identity;
            (epoch.seq == identity.epoch_seq && epoch.root == Some(identity.effective_root))
                .then(|| identity.clone())
        })
    }

    /// Confirm the epoch/root approved by Deployment's source admission.
    /// A later revocation can race admitted execution; epoch-bound authority
    /// still rejects that execution's capabilities and readiness commit.
    fn publish(
        &self,
        content: Identity,
        issued_seq: u64,
        effective_root: Cid,
        terminate_rx: &watch::Receiver<()>,
    ) -> Option<IdentityGuard> {
        let epoch = self.epoch_rx.borrow();
        if epoch.seq != issued_seq
            || epoch.root != Some(effective_root)
            || terminate_rx.has_changed().unwrap_or(true)
        {
            return None;
        }
        let owner = Arc::new(());
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *current = Some(PublishedIdentity {
            identity: RunningIdentity {
                content,
                epoch_seq: issued_seq,
                effective_root,
            },
            owner: Arc::clone(&owner),
        });
        Some(IdentityGuard {
            current: Arc::clone(&self.current),
            owner,
        })
    }

    #[cfg(test)]
    pub(crate) fn publish_for_test(
        &self,
        content: Identity,
        issued_seq: u64,
        effective_root: Cid,
    ) -> impl Drop {
        let (_terminate_tx, terminate_rx) = watch::channel(());
        self.publish(content, issued_seq, effective_root, &terminate_rx)
            .expect("test identity must match its authoritative epoch")
    }
}

/// The generation owns this guard from admission through exit or cancellation.
/// Pointer identity also distinguishes two generations with identical content.
struct IdentityGuard {
    current: Arc<Mutex<Option<PublishedIdentity>>>,
    owner: Arc<()>,
}

impl Drop for IdentityGuard {
    fn drop(&mut self) {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if current
            .as_ref()
            .is_some_and(|published| Arc::ptr_eq(&published.owner, &self.owner))
        {
            *current = None;
        }
    }
}

impl serde::Serialize for IdentityState {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(&self.get(), serializer)
    }
}

pub fn runtime_cid(bytes: &[u8]) -> Cid {
    cell::routing_key::derive(bytes)
}

/// Prepared filesystem root for one kernel generation.
#[derive(Clone)]
pub struct Root {
    cid: Cid,
    path: String,
    tree: Arc<cell::vfs::CidTree>,
}

impl Root {
    pub fn new(cid: Cid, tree: Arc<cell::vfs::CidTree>) -> Self {
        Self {
            cid,
            path: format!("/ipfs/{cid}"),
            tree,
        }
    }
}

/// Kernel stdio policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stdio {
    /// Bridge the host terminal to the kernel Cell.
    Host,
    /// Give the kernel Cell an immediately closed stdin stream.
    Closed,
}

/// Privileged Membrane bootstrap dependencies for one kernel generation.
#[derive(Clone)]
pub struct Bootstrap {
    network_state: rpc::NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    signing_key: Arc<SigningKey>,
    stream_control: libp2p_stream::Control,
    route_registry: Option<crate::dispatcher::server::RouteRegistry>,
    http_dial: Vec<String>,
}

impl Bootstrap {
    pub fn new(
        network_state: rpc::NetworkState,
        swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
        signing_key: Arc<SigningKey>,
        stream_control: libp2p_stream::Control,
        http_dial: Vec<String>,
    ) -> Self {
        Self {
            network_state,
            swarm_cmd_tx,
            signing_key,
            stream_control,
            route_registry: None,
            http_dial,
        }
    }

    pub fn with_route_registry(
        mut self,
        registry: crate::dispatcher::server::RouteRegistry,
    ) -> Self {
        self.route_registry = Some(registry);
        self
    }
}

/// Runtime construction inputs shared by the kernel and its child Executors.
#[derive(Clone)]
pub struct RuntimeInputs {
    wasm_debug: bool,
    runtime_engine: cell::engine::RuntimeEngine,
    compile_tx: mpsc::Sender<CompileRequest>,
    cache_policy: rpc::CachePolicy,
    pinset_cache: Arc<cache::PinsetCache>,
    filesystem_resources: Arc<cache::HostResources>,
}

impl RuntimeInputs {
    pub fn new(
        wasm_debug: bool,
        runtime_engine: cell::engine::RuntimeEngine,
        compile_tx: mpsc::Sender<CompileRequest>,
        cache_policy: rpc::CachePolicy,
        pinset_cache: Arc<cache::PinsetCache>,
        filesystem_resources: Arc<cache::HostResources>,
    ) -> Self {
        Self {
            wasm_debug,
            runtime_engine,
            compile_tx,
            cache_policy,
            pinset_cache,
            filesystem_resources,
        }
    }
}

/// Keeps kernel HTTP registrations live for exactly one generation.
#[must_use = "dropping the scope owner invalidates this kernel generation's registrations"]
pub struct RegistrationScope {
    sender: watch::Sender<()>,
}

impl RegistrationScope {
    fn new() -> Self {
        let (sender, _receiver) = watch::channel(());
        Self { sender }
    }

    fn receiver(&self) -> watch::Receiver<()> {
        self.sender.subscribe()
    }
}

/// Outcome of one kernel generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Exited(i32),
    Terminated,
}

/// Construction-to-launch handshake with the deployment's source observer.
/// Deployment drains authoritative updates before acknowledging admission.
pub struct LaunchAdmission {
    pub constructed: tokio::sync::oneshot::Sender<()>,
    pub admitted: tokio::sync::oneshot::Receiver<()>,
}

/// Host-owned lifecycle of one kernel Cell within one deployment epoch.
pub struct Generation {
    prepared: PreparedKernel,
    root: Root,
    guard: EpochGuard,
    identity_state: IdentityState,
    readiness_gate: Arc<authority::KernelReadyGate>,
    bootstrap: Bootstrap,
    runtime_inputs: RuntimeInputs,
    stdio: Stdio,
    terminate_rx: watch::Receiver<()>,
    filesystem_ownership: cell::FilesystemOwnership,
    admission: LaunchAdmission,
}

struct GenerationCleanup {
    proc_abort: tokio::task::AbortHandle,
    rpc: rpc::managed_rpc::ManagedRpc,
    readiness_gate: Arc<authority::KernelReadyGate>,
    identity: Option<IdentityGuard>,
}

impl Drop for GenerationCleanup {
    fn drop(&mut self) {
        self.identity.take();
        self.readiness_gate.clear();
        self.proc_abort.abort();
        self.rpc.shutdown();
    }
}

impl Generation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        prepared: PreparedKernel,
        root: Root,
        guard: EpochGuard,
        identity_state: IdentityState,
        readiness_gate: Arc<authority::KernelReadyGate>,
        bootstrap: Bootstrap,
        runtime_inputs: RuntimeInputs,
        stdio: Stdio,
        terminate_rx: watch::Receiver<()>,
        admission: LaunchAdmission,
    ) -> Result<Self, cache::AdmissionError> {
        let filesystem_resources = cache::CellResources::new(
            Arc::clone(&runtime_inputs.filesystem_resources),
            cache::CellLimits::default(),
        );
        let filesystem_ownership = cell::FilesystemOwnership::try_new(filesystem_resources)?;
        Ok(Self {
            prepared,
            root,
            guard,
            identity_state,
            readiness_gate,
            bootstrap,
            runtime_inputs,
            stdio,
            terminate_rx,
            filesystem_ownership,
            admission,
        })
    }

    /// Repeatable proof that this generation's filesystem work and private
    /// cleanup have completed. Available before `run` is first polled.
    pub fn filesystem_completion(&self) -> cache::FsCompletion {
        self.filesystem_ownership.completion()
    }

    /// Instantiate and run the kernel Cell, its privileged RPC system, and
    /// its generation-scoped registration lifetime.
    pub async fn run(self) -> Result<Outcome> {
        let Self {
            prepared,
            root,
            guard,
            identity_state,
            readiness_gate,
            bootstrap,
            runtime_inputs,
            stdio,
            mut terminate_rx,
            filesystem_ownership,
            admission,
        } = self;

        anyhow::ensure!(
            identity_state.epoch_rx.same_channel(&guard.receiver),
            "kernel identity and generation must share the authoritative epoch channel"
        );
        if !generation_is_current(&guard, root.cid, &terminate_rx) {
            return Ok(Outcome::Terminated);
        }
        // The Host retains its guard across generations. A standalone Kernel
        // invocation retains the first-initialization guard until this run ends.
        let _telemetry_guard = crate::config::init_tracing();

        anyhow::ensure!(
            prepared.effective_root() == root.cid,
            "prepared kernel belongs to a different effective root"
        );
        let identity = prepared.identity();
        tracing::info!(
            kernel_source = %identity.source,
            kernel_cid = %identity.cid,
            source_cid = %prepared.source_file(),
            bytes = identity.size,
            "Loaded selected kernel artifact"
        );
        let guest_env = prepare_guest_env(&root.path, &prepared.runtime_cid());
        let stdin = kernel_stdin(stdio);
        let (mut builder, mut handles) = Builder::kernel(
            Program::ValidatedKernel(prepared.validated()),
            stdin,
            stdout(),
            stderr(),
            root.tree,
            readiness_gate.clone(),
            filesystem_ownership,
        );
        builder = builder
            .with_runtime_engine(runtime_inputs.runtime_engine.clone())
            .with_wasm_debug(runtime_inputs.wasm_debug)
            .with_env(guest_env)
            .with_cache(cache::CacheMode::Shared(
                runtime_inputs.pinset_cache.clone(),
            ));

        let filesystem_completion = builder.filesystem_completion();
        let filesystem_finalizer = builder
            .take_filesystem_finalizer()
            .expect("generation filesystem finalizer was already registered");
        tokio::task::spawn_local(
            rpc::local_tasks::track(async move {
                let _ = filesystem_finalizer.run().await;
            })
            .in_current_span(),
        );
        let construction = tokio::select! {
            biased;
            _ = terminate_rx.changed() => return Ok(Outcome::Terminated),
            _ = generation_revoked(&guard, root.cid) => return Ok(Outcome::Terminated),
            constructed = builder.build() => constructed,
        };
        let proc = match construction {
            Ok(proc) => proc,
            Err(error) => {
                filesystem_completion.wait().await.map_err(|cleanup_error| {
                    anyhow::anyhow!(
                        "kernel Cell construction failed ({error:#}) and filesystem cleanup failed: {cleanup_error}"
                    )
                })?;
                return Err(error);
            }
        };
        let (reader, writer) = handles
            .take_host_split()
            .ok_or_else(|| anyhow::anyhow!("host stream was already consumed"))?;

        // The executor cannot observe Deployment's queued source messages.
        // Ask the authoritative owner to drain those messages after async
        // construction, then recheck the published epoch before launch.
        if admission.constructed.send(()).is_err() {
            return Ok(Outcome::Terminated);
        }
        tokio::select! {
            biased;
            _ = terminate_rx.changed() => return Ok(Outcome::Terminated),
            _ = generation_revoked(&guard, root.cid) => return Ok(Outcome::Terminated),
            admitted = admission.admitted => {
                if admitted.is_err() {
                    return Ok(Outcome::Terminated);
                }
            }
        }

        // Construction can await filesystem work. Recheck epoch, root, and
        // termination after it completes and before executable guest work or
        // a privileged Membrane is admitted. No await separates publication
        // from spawning the admitted process.
        let Some(identity_guard) =
            identity_state.publish(identity, guard.issued_seq, root.cid, &terminate_rx)
        else {
            drop(proc);
            filesystem_completion.wait().await.map_err(|error| {
                anyhow::anyhow!("canceled kernel construction filesystem cleanup failed: {error}")
            })?;
            return Ok(Outcome::Terminated);
        };

        let runtime_client = crate::launcher::create_runtime_client_with_pinset(
            runtime_inputs.wasm_debug,
            guard.clone(),
            runtime_inputs.runtime_engine,
            Some(runtime_inputs.compile_tx),
            runtime_inputs.cache_policy,
            Some(runtime_inputs.pinset_cache),
            runtime_inputs.filesystem_resources,
        );
        let registration_scope = RegistrationScope::new();
        let rpc_system = rpc::graft::build_kernel_membrane_rpc(
            reader,
            writer,
            bootstrap.network_state,
            bootstrap.swarm_cmd_tx,
            guard.receiver.clone(),
            readiness_gate.clone(),
            Some(bootstrap.signing_key),
            bootstrap.stream_control,
            bootstrap.route_registry,
            runtime_client,
            rpc::NamedCapabilities::default(),
            bootstrap.http_dial,
            guard.issued_seq,
            registration_scope.receiver(),
        );

        let mut proc_task = tokio::spawn(async move { proc.run().await }.in_current_span());
        let mut cleanup = GenerationCleanup {
            proc_abort: proc_task.abort_handle(),
            rpc: rpc_system,
            readiness_gate: readiness_gate.clone(),
            identity: Some(identity_guard),
        };
        let outcome = tokio::select! {
            joined = &mut proc_task => map_join_outcome(joined),
            changed = terminate_rx.changed() => {
                if changed.is_ok() {
                    proc_task.abort();
                    let _ = (&mut proc_task).await;
                    Outcome::Terminated
                } else {
                    map_join_outcome(proc_task.await)
                }
            }
        };

        cleanup.identity.take();
        readiness_gate.clear();
        drop(registration_scope);
        let (filesystem_result, _rpc_result) = tokio::join!(
            filesystem_completion.wait(),
            cleanup.rpc.shutdown_and_join(),
        );
        filesystem_result.map_err(|error| {
            anyhow::anyhow!("kernel generation filesystem cleanup failed: {error}")
        })?;
        tracing::debug!(?outcome, "Kernel generation exited");
        Ok(outcome)
    }
}

fn generation_is_current(
    guard: &EpochGuard,
    root: Cid,
    terminate_rx: &watch::Receiver<()>,
) -> bool {
    let epoch = guard.receiver.borrow();
    epoch.seq == guard.issued_seq
        && epoch.root == Some(root)
        && !terminate_rx.has_changed().unwrap_or(true)
}

async fn generation_revoked(guard: &EpochGuard, root: Cid) {
    let mut receiver = guard.receiver.clone();
    loop {
        {
            let epoch = receiver.borrow_and_update();
            if epoch.seq != guard.issued_seq || epoch.root != Some(root) {
                return;
            }
        }
        if receiver.changed().await.is_err() {
            // A fixed guard has no sender. Closing a watch channel alone does
            // not change its last authoritative epoch; termination is separate.
            std::future::pending::<()>().await;
        }
    }
}

fn kernel_stdin(policy: Stdio) -> Box<dyn tokio::io::AsyncRead + Send + Sync + Unpin + 'static> {
    match policy {
        Stdio::Closed => {
            let (reader, writer) = tokio::io::duplex(1);
            drop(writer);
            Box::new(reader)
        }
        Stdio::Host => {
            let (reader, mut writer) = tokio::io::duplex(4096);
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);

            // This thread preserves the current terminal behavior. It can hold
            // StdinLock across generation replacement; a process-wide stdin
            // broker remains a separate workstream.
            std::thread::spawn(move || {
                use std::io::Read;
                let stdin = std::io::stdin();
                let mut handle = stdin.lock();
                let mut buf = [0_u8; 4096];
                loop {
                    match handle.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.blocking_send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });

            tokio::spawn(async move {
                while let Some(data) = rx.recv().await {
                    if writer.write_all(&data).await.is_err() {
                        break;
                    }
                }
            });
            Box::new(reader)
        }
    }
}

fn prepare_guest_env(root: &str, artifact_cid: &Cid) -> Vec<String> {
    let mut env = vec!["PATH=/bin".to_string(), format!("WW_ROOT={root}")];
    if is_interactive() {
        env.push("WW_TTY=1".to_string());
    }
    env.push(format!("WW_CELL_CID={artifact_cid}"));
    env
}

fn map_join_outcome(joined: Result<Result<()>, tokio::task::JoinError>) -> Outcome {
    match joined {
        Ok(Ok(())) => Outcome::Exited(0),
        Ok(Err(error)) => {
            tracing::error!(
                event_code = rpc::observability::EventCode::CellFailed as u16,
                error = error.as_ref() as &dyn std::error::Error,
                "Kernel Cell error"
            );
            Outcome::Exited(1)
        }
        Err(error) => {
            tracing::error!(
                event_code = rpc::observability::EventCode::CellFailed as u16,
                error = &error as &dyn std::error::Error,
                "Kernel Cell task join error"
            );
            Outcome::Exited(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const TEST_CID: &str = "bafkr4if3s6yv23hd3hgfvftj2g2uwdrqazv53p36p5lqyy7n77d5t5p54a";

    struct KernelBootstrap {
        _dropped: tokio::sync::oneshot::Sender<()>,
    }
    impl authority::system_capnp::membrane::Server for KernelBootstrap {}

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn generation_owner_loss_cancels_host_process_and_disconnects_rpc() {
        use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty::VatNetwork};
        use rpc::managed_rpc::{ManagedRpc, ManagedRpcSystem};
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        tokio::task::LocalSet::new()
            .run_until(async {
                let root = runtime_cid(b"root");
                let (_epoch_tx, epoch_rx) = watch::channel(authority::Epoch {
                    seq: 1,
                    head: vec![],
                    root: Some(root),
                });
                let identity_state = IdentityState::pending(epoch_rx.clone());
                let (_terminate_tx, terminate_rx) = watch::channel(());
                let identity = identity_state
                    .publish(test_identity(), 1, root, &terminate_rx)
                    .unwrap();
                let ready = Arc::new(authority::KernelReadyGate::new(epoch_rx));
                ready.bind_generation(1);
                ready.kernel_ready().unwrap();
                let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
                let bootstrap: authority::system_capnp::membrane::Client =
                    capnp_rpc::new_client(KernelBootstrap {
                        _dropped: dropped_tx,
                    });
                let (stream, _peer) = tokio::io::duplex(8192);
                let (reader, writer) = tokio::io::split(stream);
                let mut rpc = ManagedRpcSystem::new(
                    Box::new(VatNetwork::new(
                        reader.compat(),
                        writer.compat_write(),
                        Side::Server,
                        Default::default(),
                    )),
                    Some(bootstrap.client),
                );
                let retained: authority::system_capnp::membrane::Client =
                    rpc.bootstrap(Side::Client);
                let proc = tokio::spawn(std::future::pending::<()>());
                let owner = GenerationCleanup {
                    proc_abort: proc.abort_handle(),
                    rpc: ManagedRpc::spawn(rpc),
                    readiness_gate: ready.clone(),
                    identity: Some(identity),
                };
                drop(owner);
                assert!(identity_state.get().is_none());
                assert!(!ready.is_ready());
                assert!(proc.await.unwrap_err().is_cancelled());
                assert!(
                    tokio::time::timeout(std::time::Duration::from_secs(2), dropped_rx)
                        .await
                        .unwrap()
                        .is_err()
                );
                assert!(retained.graft_request().send().promise.await.is_err());
            })
            .await;
    }

    fn test_identity() -> Identity {
        Identity {
            cid: runtime_cid(b"kernel").to_string(),
            source: "/ipfs/root/boot/kernel.wasm".to_owned(),
            wasm_blake3: blake3::hash(b"kernel").to_hex().to_string(),
            source_cid: Some(TEST_CID.to_owned()),
            size: 6,
        }
    }

    fn test_epoch(seq: u64, root: Option<Cid>) -> authority::Epoch {
        authority::Epoch {
            seq,
            root,
            head: Vec::new(),
        }
    }

    struct TestGeneration {
        generation: Generation,
        identity: IdentityState,
        epoch_tx: watch::Sender<authority::Epoch>,
        terminate_tx: watch::Sender<()>,
        constructed_rx: tokio::sync::oneshot::Receiver<()>,
        admit_tx: tokio::sync::oneshot::Sender<()>,
    }

    async fn test_generation(staging_dir: PathBuf) -> TestGeneration {
        let root_cid = test_support::ROOT.parse().unwrap();
        let (epoch_tx, epoch_rx) = watch::channel(test_epoch(1, Some(root_cid)));
        let identity_state = IdentityState::pending(epoch_rx.clone());
        let readiness_gate = Arc::new(authority::KernelReadyGate::new(epoch_rx.clone()));
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".into());
        let resources = crate::services::test_filesystem_resources();
        let tree = Arc::new(cell::vfs::CidTree::new(
            root_cid,
            client.clone(),
            staging_dir,
            resources.clone(),
            crate::services::test_filesystem_scope(),
        ));
        let pinset_cache = Arc::new(
            cache::PinsetCache::new(
                Arc::new(crate::deployment::RetainedPinner::new(client)),
                1024,
                resources.clone(),
            )
            .await
            .unwrap(),
        );
        let runtime_engine = test_support::runtime();
        let (compile_tx, _compile_rx) = mpsc::channel(1);
        let (swarm_cmd_tx, _swarm_cmd_rx) = mpsc::channel(1);
        let (terminate_tx, terminate_rx) = watch::channel(());
        let (constructed_tx, constructed_rx) = tokio::sync::oneshot::channel();
        let (admit_tx, admit_rx) = tokio::sync::oneshot::channel();
        let generation = Generation::new(
            test_support::prepared(test_support::ROOT),
            Root::new(root_cid, tree),
            EpochGuard {
                issued_seq: 1,
                receiver: epoch_rx,
            },
            identity_state.clone(),
            readiness_gate,
            Bootstrap::new(
                rpc::NetworkState::new(),
                swarm_cmd_tx,
                Arc::new(SigningKey::from_bytes(&[7; 32])),
                libp2p_stream::Behaviour::new().new_control(),
                Vec::new(),
            ),
            RuntimeInputs::new(
                false,
                runtime_engine,
                compile_tx,
                rpc::CachePolicy::Shared,
                pinset_cache,
                resources,
            ),
            Stdio::Closed,
            terminate_rx,
            LaunchAdmission {
                constructed: constructed_tx,
                admitted: admit_rx,
            },
        )
        .unwrap();
        TestGeneration {
            generation,
            identity: identity_state,
            epoch_tx,
            terminate_tx,
            constructed_rx,
            admit_tx,
        }
    }

    #[tokio::test]
    async fn generation_rejects_identity_from_an_unrelated_epoch_channel() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let staging = tempfile::tempdir().unwrap();
                let TestGeneration {
                    mut generation,
                    identity,
                    epoch_tx: _epoch_tx,
                    terminate_tx: _terminate_tx,
                    constructed_rx: _constructed_rx,
                    admit_tx,
                } = test_generation(staging.path().to_owned()).await;
                let (_unrelated_tx, unrelated_rx) =
                    watch::channel(generation.guard.receiver.borrow().clone());
                let unrelated_identity = IdentityState::pending(unrelated_rx);
                generation.identity_state = unrelated_identity.clone();
                let completion = generation.filesystem_completion();
                admit_tx.send(()).unwrap();

                let error = generation.run().await.unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("share the authoritative epoch channel"),
                    "{error:#}"
                );
                completion.wait().await.unwrap();
                assert!(identity.get().is_none());
                assert!(unrelated_identity.get().is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn failed_async_generation_construction_never_publishes_identity() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let staging_file = tempfile::NamedTempFile::new().unwrap();
                let TestGeneration {
                    generation,
                    identity,
                    epoch_tx: _epoch_tx,
                    terminate_tx: _terminate_tx,
                    ..
                } = test_generation(staging_file.path().to_owned()).await;
                let completion = generation.filesystem_completion();
                let error = generation.run().await.unwrap_err();
                assert!(
                    format!("{error:#}").contains("failed to preopen CidTree staging dir"),
                    "{error:#}"
                );
                completion.wait().await.unwrap();
                assert!(identity.get().is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn generation_process_keeps_its_cell_span_across_spawn() {
        use tracing::Instrument;
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .json()
            .with_ansi(false)
            .with_writer(move || Writer(writer.clone()))
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        tokio::task::LocalSet::new()
            .run_until(async {
                let first_dir = tempfile::tempdir().unwrap();
                let second_dir = tempfile::tempdir().unwrap();
                let first = test_generation(first_dir.path().to_owned()).await;
                let second = test_generation(second_dir.path().to_owned()).await;
                let first_task = tokio::task::spawn_local(
                    first
                        .generation
                        .run()
                        .instrument(tracing::info_span!("cell", cell_name = "kernel-a")),
                );
                let second_task = tokio::task::spawn_local(
                    second
                        .generation
                        .run()
                        .instrument(tracing::info_span!("cell", cell_name = "kernel-b")),
                );
                first.constructed_rx.await.unwrap();
                second.constructed_rx.await.unwrap();
                tracing::info!("unrelated-kernel-probe");
                first.admit_tx.send(()).unwrap();
                second.admit_tx.send(()).unwrap();
                assert_eq!(first_task.await.unwrap().unwrap(), Outcome::Exited(0));
                assert_eq!(second_task.await.unwrap().unwrap(), Outcome::Exited(0));
            })
            .await;
        let events: Vec<serde_json::Value> = String::from_utf8(output.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let mut owners = Vec::new();
        for event in events {
            if event["fields"]["message"] == "unrelated-kernel-probe" {
                assert!(event.get("span").is_none(), "{event}");
            }
            if event["fields"]["message"] == "fuel.initial" {
                assert_eq!(event["span"]["name"], "cell", "{event}");
                owners.push(event["span"]["cell_name"].as_str().unwrap().to_owned());
            }
        }
        owners.sort();
        assert_eq!(owners, ["kernel-a", "kernel-b"]);
    }

    #[tokio::test]
    async fn immediate_generation_exit_clears_identity_after_enqueue() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let staging = tempfile::tempdir().unwrap();
                let TestGeneration {
                    generation,
                    identity,
                    epoch_tx: _epoch_tx,
                    terminate_tx: _terminate_tx,
                    constructed_rx,
                    admit_tx,
                } = test_generation(staging.path().to_owned()).await;
                let completion = generation.filesystem_completion();
                let worker = tokio::task::spawn_local(generation.run());
                constructed_rx.await.unwrap();
                assert!(identity.get().is_none());
                admit_tx.send(()).unwrap();
                assert_eq!(worker.await.unwrap().unwrap(), Outcome::Exited(0));
                completion.wait().await.unwrap();
                assert!(identity.get().is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn generation_rechecks_epoch_and_root_after_async_construction() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for replacement in [
                    test_epoch(2, None),
                    test_epoch(1, Some(runtime_cid(b"different root"))),
                ] {
                    let staging = tempfile::tempdir().unwrap();
                    let TestGeneration {
                        generation,
                        identity,
                        epoch_tx,
                        terminate_tx: _terminate_tx,
                        constructed_rx,
                        admit_tx,
                    } = test_generation(staging.path().to_owned()).await;
                    let completion = generation.filesystem_completion();
                    let ready = generation.readiness_gate.clone();
                    let worker = tokio::task::spawn_local(generation.run());
                    constructed_rx.await.unwrap();
                    assert!(identity.get().is_none());
                    epoch_tx.send_replace(replacement);
                    // Approval and revocation are both ready. Revocation must win.
                    admit_tx.send(()).unwrap();
                    assert_eq!(worker.await.unwrap().unwrap(), Outcome::Terminated);
                    completion.wait().await.unwrap();
                    assert!(identity.get().is_none());
                    assert!(!ready.is_ready());
                }
            })
            .await;
    }

    #[tokio::test]
    async fn generation_abort_while_waiting_for_admission_never_publishes_identity() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let staging = tempfile::tempdir().unwrap();
                let TestGeneration {
                    generation,
                    identity,
                    epoch_tx: _epoch_tx,
                    terminate_tx: _terminate_tx,
                    constructed_rx,
                    admit_tx: _admit_tx,
                } = test_generation(staging.path().to_owned()).await;
                let completion = generation.filesystem_completion();
                let worker = tokio::task::spawn_local(generation.run());
                constructed_rx.await.unwrap();
                assert!(identity.get().is_none());
                worker.abort();
                assert!(worker.await.unwrap_err().is_cancelled());
                completion.wait().await.unwrap();
                assert!(identity.get().is_none());
            })
            .await;
    }

    #[test]
    fn identity_admission_checks_epoch_root_and_termination() {
        let root = runtime_cid(b"root");
        let (epoch_tx, epoch_rx) = watch::channel(test_epoch(1, None));
        let state = IdentityState::pending(epoch_rx);
        let (terminate_tx, terminate_rx) = watch::channel(());
        assert_eq!(state.pending_source(), "<pending: /boot/kernel.wasm>");
        assert!(state.get().is_none());
        assert!(state
            .publish(test_identity(), 1, root, &terminate_rx)
            .is_none());
        epoch_tx.send_replace(test_epoch(1, Some(root)));
        assert!(state
            .publish(test_identity(), 0, root, &terminate_rx)
            .is_none());
        assert!(state
            .publish(
                test_identity(),
                1,
                runtime_cid(b"wrong root"),
                &terminate_rx
            )
            .is_none());
        terminate_tx.send_replace(());
        assert!(state
            .publish(test_identity(), 1, root, &terminate_rx)
            .is_none());
        drop(terminate_tx);
        assert!(state
            .publish(test_identity(), 1, root, &terminate_rx)
            .is_none());
        assert!(state.get().is_none());
    }

    #[test]
    fn identity_hides_unrooted_and_mismatched_epochs_before_cleanup() {
        let root = runtime_cid(b"root");
        let (epoch_tx, epoch_rx) = watch::channel(test_epoch(1, Some(root)));
        let state = IdentityState::pending(epoch_rx);
        let (_terminate_tx, terminate_rx) = watch::channel(());
        let _identity = state
            .publish(test_identity(), 1, root, &terminate_rx)
            .unwrap();
        let running = state.get().unwrap();
        assert_eq!(running.content.cid, runtime_cid(b"kernel").to_string());
        assert_eq!(running.content.source_cid.as_deref(), Some(TEST_CID));
        assert_ne!(
            running.content.source_cid.as_deref(),
            Some(running.content.cid.as_str())
        );
        assert_eq!(running.epoch_seq, 1);
        assert_eq!(running.effective_root, root);

        epoch_tx.send_replace(test_epoch(1, Some(runtime_cid(b"different root"))));
        assert!(state.get().is_none());
        epoch_tx.send_replace(test_epoch(2, None));
        assert!(state.get().is_none());
        epoch_tx.send_replace(test_epoch(2, Some(root)));
        assert!(state.get().is_none());
        assert!(state
            .publish(test_identity(), 1, root, &terminate_rx)
            .is_none());
    }

    #[test]
    fn same_kernel_bytes_receive_new_epoch_and_old_cleanup_cannot_clear_them() {
        let root = runtime_cid(b"root");
        let (epoch_tx, epoch_rx) = watch::channel(test_epoch(1, Some(root)));
        let state = IdentityState::pending(epoch_rx);
        let (_terminate_tx, terminate_rx) = watch::channel(());
        let old = state
            .publish(test_identity(), 1, root, &terminate_rx)
            .unwrap();
        epoch_tx.send_replace(test_epoch(2, Some(root)));
        assert!(state.get().is_none());
        let current = state
            .publish(test_identity(), 2, root, &terminate_rx)
            .unwrap();
        drop(old);
        let running = state.get().unwrap();
        assert_eq!(running.epoch_seq, 2);
        assert_eq!(running.content, test_identity());
        drop(current);
        assert!(state.get().is_none());
    }

    #[test]
    fn identity_cleanup_uses_generation_owner_even_when_epoch_and_bytes_match() {
        let root = runtime_cid(b"root");
        let (_epoch_tx, epoch_rx) = watch::channel(test_epoch(1, Some(root)));
        let state = IdentityState::pending(epoch_rx);
        let (_terminate_tx, terminate_rx) = watch::channel(());
        let old = state
            .publish(test_identity(), 1, root, &terminate_rx)
            .unwrap();
        let current = state
            .publish(test_identity(), 1, root, &terminate_rx)
            .unwrap();
        drop(old);
        assert!(state.get().is_some());
        drop(current);
        assert!(state.get().is_none());
    }

    #[tokio::test]
    async fn identity_clears_when_enqueued_execution_exits_immediately() {
        let root = runtime_cid(b"root");
        let (_epoch_tx, epoch_rx) = watch::channel(test_epoch(1, Some(root)));
        let state = IdentityState::pending(epoch_rx);
        let (_terminate_tx, terminate_rx) = watch::channel(());
        let worker_state = state.clone();
        tokio::spawn(async move {
            let _identity = worker_state
                .publish(test_identity(), 1, root, &terminate_rx)
                .unwrap();
        })
        .await
        .unwrap();
        // The launcher has no publication to perform after the worker exits.
        assert!(state.get().is_none());
    }

    #[tokio::test]
    async fn identity_clears_after_panic_and_abort() {
        let root = runtime_cid(b"root");
        let (_epoch_tx, epoch_rx) = watch::channel(test_epoch(1, Some(root)));
        let state = IdentityState::pending(epoch_rx);
        let (_terminate_tx, terminate_rx) = watch::channel(());
        let worker_state = state.clone();
        let worker_terminate_rx = terminate_rx.clone();
        let panicked = tokio::spawn(async move {
            let _identity = worker_state
                .publish(test_identity(), 1, root, &worker_terminate_rx)
                .unwrap();
            panic!("injected generation panic");
        })
        .await
        .unwrap_err();
        assert!(panicked.is_panic());
        assert!(state.get().is_none());

        let (published_tx, published_rx) = tokio::sync::oneshot::channel();
        let worker_state = state.clone();
        let worker = tokio::spawn(async move {
            let _identity = worker_state
                .publish(test_identity(), 1, root, &terminate_rx)
                .unwrap();
            published_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        published_rx.await.unwrap();
        assert!(state.get().is_some());
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert!(state.get().is_none());
    }

    #[tokio::test]
    async fn generation_revocation_observes_updates_that_precede_its_first_poll() {
        let root = runtime_cid(b"root");
        let (epoch_tx, epoch_rx) = watch::channel(test_epoch(1, Some(root)));
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: epoch_rx,
        };
        let (_terminate_tx, terminate_rx) = watch::channel(());
        assert!(generation_is_current(&guard, root, &terminate_rx));
        let revoked = generation_revoked(&guard, root);
        epoch_tx.send_replace(test_epoch(2, None));
        revoked.await;
        assert!(!generation_is_current(&guard, root, &terminate_rx));
    }
}
