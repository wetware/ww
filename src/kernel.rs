//! Host-side kernel source selection, resolution, and runtime identity.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use authority::EpochGuard;
use cid::Cid;
use ed25519_dalek::SigningKey;
use futures::FutureExt;
use tokio::io::{stderr, stdout, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

use crate::cell::loaders::{HostPathLoader, IpfsLoader};
use crate::cell::{Builder, Loader, Program};
use crate::host::SwarmCommand;
use crate::services::CompileRequest;

/// Source selected for the pid0 component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Source {
    Path(PathBuf),
    Cid(Cid),
    Embedded(&'static str),
}

/// Stable, owned description of a selected source for logs and `/version`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceRecord {
    Path { original: String },
    Cid { original: String },
    Embedded { original: String },
}

impl fmt::Display for SourceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path { original } => write!(f, "file:{original}"),
            Self::Cid { original } => write!(f, "cid:{original}"),
            Self::Embedded { original } => write!(f, "embedded:{original}"),
        }
    }
}

impl SourceRecord {
    /// Bound structured log fields without changing the exact source retained
    /// for `/version` and diagnostics.
    pub fn log_value(&self) -> String {
        let value = self.to_string();
        if value.len() <= 512 {
            value
        } else {
            format!("<kernel source omitted: {} bytes>", value.len())
        }
    }
}

/// Resolution metadata retained for diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Metadata {
    pub size: usize,
    pub source_cid: Option<Cid>,
    pub load_duration: Duration,
}

/// Exact pid0 bytes and their loaded-byte runtime identity.
#[derive(Clone, Debug)]
pub struct Artifact {
    pub bytes: Vec<u8>,
    pub cid: Cid,
    pub source: SourceRecord,
    pub metadata: Metadata,
}

/// Whether PID0 owns an interactive terminal for this invocation.
///
/// `WW_TTY` remains the test and compatibility override for stdin that is not
/// reported as a terminal by the host process.
pub fn is_interactive() -> bool {
    use std::io::IsTerminal;

    std::io::stdin().is_terminal() || std::env::var("WW_TTY").is_ok()
}

impl Artifact {
    pub fn identity(&self) -> Identity {
        Identity {
            cid: self.cid.to_string(),
            source: self.source.to_string(),
            wasm_blake3: blake3::hash(&self.bytes).to_hex().to_string(),
            source_cid: self.metadata.source_cid.as_ref().map(ToString::to_string),
            size: self.metadata.size,
        }
    }
}

/// Late-bound identity published by the admin plane after source resolution.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Identity {
    pub cid: String,
    pub source: String,
    pub wasm_blake3: String,
    pub source_cid: Option<String>,
    pub size: usize,
}

/// Shared identity state keeps `/version` available before Kubo and pid0.
#[derive(Clone, Debug)]
pub struct IdentityState {
    requested_source: String,
    resolved: Arc<OnceLock<Identity>>,
}

impl IdentityState {
    pub fn pending(source: &Source) -> Self {
        Self {
            requested_source: source.record().to_string(),
            resolved: Arc::new(OnceLock::new()),
        }
    }

    pub fn pending_source(&self) -> String {
        format!("<pending: {}>", self.requested_source)
    }

    pub fn get(&self) -> Option<&Identity> {
        self.resolved.get()
    }

    pub fn publish(&self, identity: Identity) -> Result<()> {
        self.resolved
            .set(identity)
            .map_err(|_| anyhow::anyhow!("kernel runtime identity was already published"))
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

impl Source {
    /// Parse the documented bare or explicit source syntax.
    pub fn parse(input: &str) -> Result<Self> {
        if input.is_empty() {
            bail!("kernel source must not be empty");
        }

        if let Some(path) = input.strip_prefix("file:") {
            if path.is_empty() {
                bail!("explicit file: kernel source requires a path");
            }
            return Ok(Self::Path(PathBuf::from(path)));
        }

        if let Some(value) = input.strip_prefix("cid:") {
            if value.is_empty() {
                bail!("explicit cid: kernel source requires a CID");
            }
            return value
                .parse()
                .map(Self::Cid)
                .with_context(|| format!("invalid explicit kernel CID '{value}'"));
        }

        if let Some(name) = input.strip_prefix("embedded:") {
            return match name {
                "main" => Ok(Self::Embedded("main")),
                "" => bail!("explicit embedded: kernel source requires a name"),
                other => bail!("unknown embedded kernel '{other}' (available: main)"),
            };
        }

        match input.parse::<Cid>() {
            Ok(cid) => Ok(Self::Cid(cid)),
            Err(_) => Ok(Self::Path(PathBuf::from(input))),
        }
    }

    pub fn record(&self) -> SourceRecord {
        match self {
            Self::Path(path) => SourceRecord::Path {
                original: path.display().to_string(),
            },
            Self::Cid(cid) => SourceRecord::Cid {
                original: cid.to_string(),
            },
            Self::Embedded(name) => SourceRecord::Embedded {
                original: (*name).to_string(),
            },
        }
    }

    /// Resolve this source exactly once. Explicit sources never fall back.
    pub async fn resolve(
        &self,
        ipfs_client: crate::ipfs::HttpClient,
        embedded_main: &'static [u8],
    ) -> Result<Artifact> {
        let started = Instant::now();
        let (bytes, source_cid) = match self {
            Self::Path(path) => {
                let metadata = tokio::fs::metadata(path)
                    .await
                    .with_context(|| format!("kernel file '{}' does not exist", path.display()))?;
                if !metadata.is_file() {
                    bail!("kernel path '{}' is not a regular file", path.display());
                }
                let loader = HostPathLoader;
                let path = path
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("kernel path is not valid UTF-8"))?;
                (
                    loader
                        .load(path)
                        .await
                        .with_context(|| format!("failed to load kernel file '{path}'"))?,
                    None,
                )
            }
            Self::Cid(source_cid) => {
                let path = format!("/ipfs/{source_cid}");
                let loader = IpfsLoader::new(ipfs_client);
                (
                    loader.load(&path).await.with_context(|| {
                        format!("failed to load requested kernel CID {source_cid} from Kubo")
                    })?,
                    Some(source_cid.to_owned()),
                )
            }
            Self::Embedded("main") => {
                if embedded_main.is_empty() {
                    bail!("embedded kernel 'main' is missing or empty; run `make std`");
                }
                (embedded_main.to_vec(), None)
            }
            Self::Embedded(name) => bail!("unknown embedded kernel '{name}'"),
        };

        if bytes.is_empty() {
            bail!("resolved kernel '{}' is empty", self.record());
        }

        let cid = runtime_cid(&bytes);
        if let Some(source_cid) = source_cid.as_ref() {
            validate_source_cid(source_cid, &cid)?;
        }

        Ok(Artifact {
            metadata: Metadata {
                size: bytes.len(),
                source_cid,
                load_duration: started.elapsed(),
            },
            bytes,
            cid,
            source: self.record(),
        })
    }
}

fn validate_source_cid(source_cid: &Cid, runtime_cid: &Cid) -> Result<()> {
    if source_cid.codec() == 0x55 && source_cid.hash().code() == 0x1e && source_cid != runtime_cid {
        bail!(
            "raw BLAKE3 kernel CID mismatch: requested {source_cid}, loaded bytes identify as {runtime_cid}"
        );
    }
    Ok(())
}

/// CLI wins over environment; absent selectors retain the embedded default.
pub fn select_kernel_source(cli: Option<&str>, env: Option<&str>) -> Result<Source> {
    match cli.or(env) {
        Some(value) => Source::parse(value),
        None => Ok(Source::Embedded("main")),
    }
}

pub fn runtime_cid(bytes: &[u8]) -> Cid {
    let digest = blake3::hash(bytes);
    let mh = cid::multihash::Multihash::<64>::wrap(0x1e, digest.as_bytes())
        .expect("blake3 digest always fits in 64-byte multihash");
    Cid::new_v1(0x55, mh)
}

/// Prepared filesystem root for one kernel generation.
pub struct Root {
    path: String,
    tree: Arc<cell::vfs::CidTree>,
}

impl Root {
    pub fn new(path: String, tree: Arc<cell::vfs::CidTree>) -> Self {
        Self { path, tree }
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
pub struct Bootstrap {
    network_state: rpc::NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    signing_key: Arc<SigningKey>,
    stream_control: libp2p_stream::Control,
    route_registry: Option<crate::dispatcher::server::RouteRegistry>,
    ipfs_client: crate::ipfs::HttpClient,
    http_dial: Vec<String>,
}

impl Bootstrap {
    pub fn new(
        network_state: rpc::NetworkState,
        swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
        signing_key: Arc<SigningKey>,
        stream_control: libp2p_stream::Control,
        ipfs_client: crate::ipfs::HttpClient,
        http_dial: Vec<String>,
    ) -> Self {
        Self {
            network_state,
            swarm_cmd_tx,
            signing_key,
            stream_control,
            route_registry: None,
            ipfs_client,
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
pub struct RuntimeInputs {
    wasm_debug: bool,
    engine: Arc<wasmtime::Engine>,
    compile_tx: mpsc::Sender<CompileRequest>,
    cache_policy: rpc::CachePolicy,
    pinset_cache: Arc<cache::PinsetCache>,
}

impl RuntimeInputs {
    pub fn new(
        wasm_debug: bool,
        engine: Arc<wasmtime::Engine>,
        compile_tx: mpsc::Sender<CompileRequest>,
        cache_policy: rpc::CachePolicy,
        pinset_cache: Arc<cache::PinsetCache>,
    ) -> Self {
        Self {
            wasm_debug,
            engine,
            compile_tx,
            cache_policy,
            pinset_cache,
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

/// Host-owned lifecycle of one kernel Cell within one deployment epoch.
pub struct Generation {
    artifact: Artifact,
    root: Root,
    guard: EpochGuard,
    readiness_gate: Arc<authority::KernelReadyGate>,
    bootstrap: Bootstrap,
    runtime_inputs: RuntimeInputs,
    stdio: Stdio,
    terminate_rx: watch::Receiver<()>,
}

struct GenerationCleanup {
    proc_abort: tokio::task::AbortHandle,
    rpc_abort: tokio::task::AbortHandle,
    readiness_gate: Arc<authority::KernelReadyGate>,
}

impl Drop for GenerationCleanup {
    fn drop(&mut self) {
        self.readiness_gate.clear();
        self.proc_abort.abort();
        self.rpc_abort.abort();
    }
}

impl Generation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        artifact: Artifact,
        root: Root,
        guard: EpochGuard,
        readiness_gate: Arc<authority::KernelReadyGate>,
        bootstrap: Bootstrap,
        runtime_inputs: RuntimeInputs,
        stdio: Stdio,
        terminate_rx: watch::Receiver<()>,
    ) -> Self {
        Self {
            artifact,
            root,
            guard,
            readiness_gate,
            bootstrap,
            runtime_inputs,
            stdio,
            terminate_rx,
        }
    }

    /// Instantiate and run the kernel Cell, its privileged RPC system, and
    /// its generation-scoped registration lifetime.
    pub async fn run(self) -> Result<Outcome> {
        let Self {
            artifact,
            root,
            guard,
            readiness_gate,
            bootstrap,
            runtime_inputs,
            stdio,
            mut terminate_rx,
        } = self;

        guard
            .check()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        crate::config::init_tracing();

        tracing::info!(
            kernel_source = %artifact.source.log_value(),
            kernel_cid = %artifact.cid,
            source_cid = ?artifact.metadata.source_cid,
            bytes = artifact.metadata.size,
            load_ms = artifact.metadata.load_duration.as_millis(),
            embedded = matches!(artifact.source, SourceRecord::Embedded { .. }),
            "Loaded selected kernel artifact"
        );

        let stdin = kernel_stdin(stdio);
        let guest_env = prepare_guest_env(&root.path, &artifact.cid);
        let (mut builder, mut handles) = Builder::kernel(
            Program::Bytes(artifact.bytes),
            stdin,
            stdout(),
            stderr(),
            root.tree,
            readiness_gate.clone(),
        );
        builder = builder
            .with_engine(runtime_inputs.engine.clone())
            .with_wasm_debug(runtime_inputs.wasm_debug)
            .with_env(guest_env)
            .with_cache(cache::CacheMode::Shared(
                runtime_inputs.pinset_cache.clone(),
            ));

        let proc = builder.build().await?;
        let (reader, writer) = handles
            .take_host_split()
            .ok_or_else(|| anyhow::anyhow!("host stream was already consumed"))?;

        let runtime_client = crate::launcher::create_runtime_client_with_pinset(
            runtime_inputs.wasm_debug,
            guard.clone(),
            Some(runtime_inputs.engine),
            Some(runtime_inputs.compile_tx),
            runtime_inputs.cache_policy,
            Some(runtime_inputs.pinset_cache),
        );
        let registration_scope = RegistrationScope::new();
        let rpc_system = rpc::graft::build_kernel_membrane_rpc(
            reader,
            writer,
            bootstrap.network_state,
            bootstrap.swarm_cmd_tx,
            runtime_inputs.wasm_debug,
            guard.receiver.clone(),
            readiness_gate.clone(),
            Some(bootstrap.signing_key),
            bootstrap.stream_control,
            bootstrap.route_registry,
            runtime_client,
            rpc::NamedCapabilities::default(),
            bootstrap.ipfs_client,
            bootstrap.http_dial,
            guard.issued_seq,
            registration_scope.receiver(),
        );

        let mut proc_task = tokio::spawn(async move { proc.run().await });
        let rpc_task = tokio::task::spawn_local(rpc_system.map(|_| ()));
        let _cleanup = GenerationCleanup {
            proc_abort: proc_task.abort_handle(),
            rpc_abort: rpc_task.abort_handle(),
            readiness_gate: readiness_gate.clone(),
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

        readiness_gate.clear();
        drop(registration_scope);
        rpc_task.abort();
        let _ = rpc_task.await;
        tracing::debug!(?outcome, "Kernel generation exited");
        Ok(outcome)
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
            tracing::error!("Kernel Cell error: {error:#}");
            Outcome::Exited(1)
        }
        Err(error) => {
            tracing::error!("Kernel Cell task join error: {error}");
            Outcome::Exited(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CID: &str = "bafkr4if3s6yv23hd3hgfvftj2g2uwdrqazv53p36p5lqyy7n77d5t5p54a";

    #[test]
    fn selector_precedence_is_cli_then_env_then_embedded() {
        assert_eq!(
            select_kernel_source(Some("file:/cli.wasm"), Some("file:/env.wasm")).unwrap(),
            Source::Path(PathBuf::from("/cli.wasm"))
        );
        assert_eq!(
            select_kernel_source(None, Some("file:/env.wasm")).unwrap(),
            Source::Path(PathBuf::from("/env.wasm"))
        );
        assert_eq!(
            select_kernel_source(None, None).unwrap(),
            Source::Embedded("main")
        );
    }

    #[test]
    fn explicit_prefixes_override_cid_path_ambiguity() {
        assert_eq!(
            Source::parse(&format!("file:{TEST_CID}")).unwrap(),
            Source::Path(PathBuf::from(TEST_CID))
        );
        assert!(matches!(
            Source::parse(&format!("cid:{TEST_CID}")).unwrap(),
            Source::Cid(_)
        ));
        assert!(matches!(Source::parse(TEST_CID).unwrap(), Source::Cid(_)));
        assert_eq!(
            Source::parse("not-a-cid.wasm").unwrap(),
            Source::Path(PathBuf::from("not-a-cid.wasm"))
        );
        assert_eq!(
            Source::parse(" file with spaces.wasm ").unwrap(),
            Source::Path(PathBuf::from(" file with spaces.wasm "))
        );
    }

    #[tokio::test]
    async fn local_file_resolution_reports_loaded_byte_identity() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"kernel bytes").unwrap();
        let source = Source::Path(file.path().to_owned());
        let resolved = source
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"unused",
            )
            .await
            .unwrap();
        assert_eq!(resolved.bytes, b"kernel bytes");
        assert_eq!(resolved.cid, runtime_cid(b"kernel bytes"));
        assert_eq!(resolved.metadata.size, 12);
        assert_eq!(resolved.metadata.source_cid, None);
    }

    #[tokio::test]
    async fn missing_and_directory_paths_fail_with_named_errors() {
        let missing = Source::Path(PathBuf::from("/definitely/missing/kernel.wasm"));
        let error = missing
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"unused",
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("does not exist"));

        let directory = tempfile::tempdir().unwrap();
        let error = Source::Path(directory.path().to_owned())
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"unused",
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("not a regular file"));
    }

    #[tokio::test]
    async fn empty_kernel_file_fails_with_named_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let error = Source::Path(file.path().to_owned())
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"unused",
            )
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("resolved kernel"), "{message}");
        assert!(message.contains("is empty"), "{message}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_kernel_file_fails_with_named_error() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unreadable.wasm");
        std::fs::write(&path, b"kernel").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = Source::Path(path.clone())
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"unused",
            )
            .await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = result.expect_err("unreadable kernel file must fail");
        let message = format!("{error:#}");
        assert!(message.contains("failed to load kernel file"), "{message}");
    }

    #[tokio::test]
    async fn explicit_cid_failure_does_not_fall_back_to_embedded() {
        let source = Source::parse(&format!("cid:{TEST_CID}")).unwrap();
        let error = source
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"valid embedded bytes that must not be selected",
            )
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("requested kernel CID"), "{message}");
        assert!(message.contains(TEST_CID), "{message}");
    }

    #[test]
    fn raw_blake3_source_cid_mismatch_fails_closed() {
        let requested = runtime_cid(b"requested content");
        let loaded = runtime_cid(b"different loaded content");
        let error = validate_source_cid(&requested, &loaded).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("CID mismatch"), "{message}");
        assert!(message.contains(&requested.to_string()), "{message}");
        assert!(message.contains(&loaded.to_string()), "{message}");
    }

    #[test]
    fn parser_errors_name_explicit_interpretation() {
        let cid_error = Source::parse("cid:not-a-cid").unwrap_err().to_string();
        assert!(cid_error.contains("explicit kernel CID"), "{cid_error}");

        let file_error = Source::parse("file:").unwrap_err().to_string();
        assert!(file_error.contains("file:"), "{file_error}");
    }

    #[tokio::test]
    async fn embedded_resolution_fails_closed_when_artifact_is_missing() {
        let error = Source::Embedded("main")
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"",
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("missing or empty"));
    }

    #[test]
    fn identity_is_late_bound_once() {
        let source = Source::Embedded("main");
        let state = IdentityState::pending(&source);
        assert_eq!(state.pending_source(), "<pending: embedded:main>");
        assert!(state.get().is_none());

        let resolved = Artifact {
            bytes: b"kernel".to_vec(),
            cid: runtime_cid(b"kernel"),
            source: source.record(),
            metadata: Metadata {
                size: 6,
                source_cid: None,
                load_duration: Duration::ZERO,
            },
        };
        state.publish(resolved.identity()).unwrap();
        assert_eq!(state.get().unwrap().cid, runtime_cid(b"kernel").to_string());
        assert!(state.publish(resolved.identity()).is_err());
    }
}
