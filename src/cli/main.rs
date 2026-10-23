use anyhow::{bail, Context, Result};
use std::io::Write as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use authority::Epoch;
use clap::{Parser, Subcommand};
use ed25519_dalek::VerifyingKey;
use std::path::{Path, PathBuf};
use tokio::sync::watch;

use libp2p::Multiaddr;

mod daemon_cmd;
mod doctor_cmd;
mod image_cmd;
mod installation;
mod ns_cmd;
mod p3_toolchain;

use ww::cell::image;
use ww::host;
use ww::ipfs;

// Embedded installation seeds. Runtime PID0 selection reads the effective FHS.
// build.rs sets cfg flags (has_wasm_*) when each file exists and is non-empty.
// Debug builds without canonical assets need an explicit installation payload.
#[cfg(has_wasm_std_kernel_bin_main_wasm)]
const EMBEDDED_KERNEL: &[u8] = include_bytes!("../../std/kernel/bin/main.wasm");
#[cfg(not(has_wasm_std_kernel_bin_main_wasm))]
const EMBEDDED_KERNEL: &[u8] = b"";

#[cfg(has_wasm_std_status_bin_status_wasm)]
const EMBEDDED_STATUS: &[u8] = include_bytes!("../../std/status/bin/status.wasm");
#[cfg(not(has_wasm_std_status_bin_status_wasm))]
const EMBEDDED_STATUS: &[u8] = b"";

/// Return the configured local admin listener, or `None` for the explicit
/// opt-out sentinel. Keep this normalization separate from socket parsing so
/// CLI and environment input share one tested disable path.
fn admin_listen_addr(value: String) -> Option<String> {
    (!value.trim().eq_ignore_ascii_case("off")).then_some(value)
}

fn service_exit_error(exit: Option<ww::services::ServiceExit>) -> anyhow::Error {
    match exit {
        Some(exit) => anyhow::anyhow!(
            "supervised service '{}' exited: {}",
            exit.name,
            exit.error
                .as_deref()
                .unwrap_or("service stopped unexpectedly")
        ),
        None => anyhow::anyhow!("service supervision channel closed"),
    }
}

#[derive(Parser)]
#[command(name = "ww")]
#[command(about = "Agentic OS for autonomous programs that coordinate across trust boundaries.")]
#[command(version = "0.1.0")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Statically validate composed images without executing guest code.
    Image {
        #[command(subcommand)]
        action: image_cmd::ImageAction,
    },
    /// Initialize a new typed cell guest project.
    ///
    /// Scaffolds a complete Rust project for a Wetware guest with
    /// Cap'n Proto schema and build script.
    Init {
        /// Project name (e.g., 'oracle', 'greeter')
        #[arg(value_name = "NAME")]
        name: String,
    },

    /// Build a guest project, placing artifacts in bin/.
    ///
    /// Compiles a Rust project targeting native wasm32-wasip3 and copies the
    /// artifact into the project's image tree at bin/<directory-name>.wasm.
    ///
    /// Expects Cargo.toml at the root of <path>.
    Build {
        /// Path to the wetware environment (default: current directory)
        #[arg(default_value = ".", value_name = "PATH")]
        path: PathBuf,
    },

    /// Run a wetware environment.
    ///
    /// Mount sources use source[:/guest/target], applied in argument order.
    /// Local files require an explicit target; root sources must be directories.
    /// Local sources require ./, /, or ~/; no implicit current-directory mount.
    ///
    /// Examples:
    ///   ww run .
    ///   ww run ./images/app ./config:/etc/myapp
    ///   ww run ./kernel.wasm:/boot/kernel.wasm
    ///   ww run /ipfs/<directory-cid>
    Run {
        /// Ordered source[:/guest/target] mounts above the installed FHS and Stem.
        #[arg(value_name = "MOUNT")]
        mounts: Vec<String>,

        /// Installed FHS directory, frozen below Stem, namespaces, and CLI mounts.
        /// Defaults to $HOME/.ww/fhs. Must be an absolute path or ~/path.
        #[arg(long, env = "WW_FHS_ROOT", value_name = "PATH")]
        fhs_root: Option<PathBuf>,

        /// Host-only roots whose `etc/ns` directories configure namespace layers.
        #[arg(long, value_name = "PATH", hide = true)]
        namespace_root: Vec<PathBuf>,

        /// libp2p listen multiaddr. Repeatable; comma-separated via WW_LISTEN.
        /// Defaults to TCP and QUIC on both IPv4 and IPv6 at port 2025.
        /// Every requested address must bind successfully — bind failures are
        /// hard errors. Pass an explicit subset to opt out of e.g. IPv6 or QUIC.
        #[arg(
            long = "listen",
            value_name = "MULTIADDR",
            env = "WW_LISTEN",
            value_delimiter = ','
        )]
        listen: Vec<Multiaddr>,

        /// Enable WASM debug info for guest processes
        #[arg(long)]
        wasm_debug: bool,

        /// Rejected legacy selector; use a positional /boot/kernel.wasm mount.
        #[arg(long = "kernel", hide = true, value_name = "PATH|CID")]
        obsolete_kernel: Option<String>,

        /// Path to an Ed25519 identity file (host-side only; not a guest mount).
        /// Works well with direnv: `export WW_IDENTITY=~/.ww/identity` in .envrc.
        #[arg(long, env = "WW_IDENTITY", value_name = "PATH")]
        identity: Option<String>,

        /// Allow ephemeral identity (insecure). By default, `ww run` requires
        /// a persistent identity file so auth-dependent shell/network flows can
        /// rely on stable signing keys across restarts.
        #[arg(long)]
        insecure_ephemeral: bool,

        /// Atom contract address (hex, 0x-prefixed). Enables authoritative
        /// deployment following and authority revocation on head changes.
        #[arg(long, conflicts_with = "ipns_stem")]
        stem: Option<String>,

        /// IPNS name to follow as the authoritative deployment Stem.
        #[arg(long, value_name = "IPNS_NAME", conflicts_with = "stem")]
        ipns_stem: Option<String>,

        /// HTTP Routing V1 Gateway listener used for raw IPNS records.
        /// Kubo 0.33 requires Gateway.ExposeRoutingAPI=true.
        #[arg(
            long,
            default_value = "http://localhost:8080",
            env = "IPFS_ROUTING_API"
        )]
        ipns_routing_url: String,

        /// HTTP JSON-RPC URL for finalized-depth Atom polling.
        #[arg(long, default_value = "http://127.0.0.1:8545")]
        rpc_url: String,

        /// WebSocket JSON-RPC URL for advisory pre-finality Atom events.
        #[arg(long, default_value = "ws://127.0.0.1:8545")]
        ws_url: String,

        /// Chain depth used when reading authoritative Atom.head() state.
        #[arg(long, default_value = "6")]
        confirmation_depth: u64,

        /// Number of executor worker threads for cell scheduling.
        /// Each worker runs its own single-threaded tokio runtime.
        /// 0 = auto-detect (one per CPU core). NOTE: auto-detect reads the
        /// node's core count, NOT the cgroup CPU quota, so under a k8s CPU
        /// limit it over-subscribes during the compile-heavy boot window
        /// (measured self-contention). Pin this to match the CPU budget on
        /// constrained hosts; WW_EXECUTOR_THREADS sets it from the environment
        /// (parity with WW_COMPILE_WORKERS) so the deploy manifest can carry it
        /// next to the CPU limit.
        #[arg(long, default_value = "0", env = "WW_EXECUTOR_THREADS")]
        executor_threads: usize,

        /// Enable the WAGI HTTP server on the given address.
        /// Example: --http-listen 127.0.0.1:8080
        #[arg(long, value_name = "ADDR")]
        http_listen: Option<String>,

        /// Allow cells to make outbound HTTP requests to the given host.
        /// Repeatable. Without this flag, no http-client capability is granted.
        /// Supports exact hosts, subdomain globs (*.example.com), or '*' for all.
        #[arg(long, value_name = "HOST")]
        http_dial: Vec<String>,

        /// Runtime cache policy for `Runtime.load()`.
        /// "shared" (default): same WASM bytes → same Executor server.
        /// "isolated": always create a fresh Executor server.
        #[arg(long, default_value = "shared", env = "WW_RUNTIME_CACHE_POLICY")]
        runtime_cache_policy: String,

        /// Local HTTP admin endpoint. Serves GET /healthz, GET /readyz,
        /// GET /version, GET /metrics, GET /host/id, and GET /host/addrs.
        /// Defaults to localhost only; pass `--with-http-admin off` to
        /// disable it.
        #[arg(
            long,
            value_name = "ADDR",
            default_value = "127.0.0.1:2026",
            env = "WW_HTTP_ADMIN"
        )]
        with_http_admin: String,

        /// IPFS HTTP API endpoint
        #[arg(long, default_value = "http://localhost:5001", env = "IPFS_API")]
        ipfs_url: String,
    },

    /// Probe a running node's localhost admin endpoint.
    ///
    /// Designed for distroless container exec probes, where curl and wget are
    /// intentionally unavailable.
    Healthcheck {
        /// Admin listener address.
        #[arg(
            long,
            value_name = "ADDR",
            default_value = "127.0.0.1:2026",
            env = "WW_HTTP_ADMIN"
        )]
        admin: String,

        /// Check serving readiness (`/readyz`) instead of liveness (`/healthz`).
        #[arg(long)]
        ready: bool,

        /// Also require `/version` to report this exact source revision.
        #[arg(long, value_name = "SHA")]
        expect_git_sha: Option<String>,

        /// Require an enabled Wasmtime cache with at least one successful hit
        /// or store in this process.
        #[arg(long)]
        require_cache_enabled: bool,

        /// Require an enabled Wasmtime cache to have loaded an entry. This
        /// proves a cache entry survived from an earlier process and is meant
        /// for post-restart monitoring, not a first cold boot.
        #[arg(long)]
        require_cache_warm: bool,

        /// Per-request timeout in seconds.
        #[arg(long, default_value_t = 2)]
        timeout_secs: u64,
    },

    /// Generate a new Ed25519 identity secret.
    ///
    /// Prints a base58btc-encoded secret key to stdout.  Metadata (Peer ID)
    /// is printed to stderr so stdout stays pipeable:
    ///
    ///     ww keygen > ~/.ww/identity
    ///     ww keygen --output ~/.ww/identity   # equivalent
    Keygen {
        /// Write the secret to a file instead of stdout.
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
    },

    /// Manage the wetware background daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Publish a built image tree to IPFS.
    ///
    /// Imports the tree as a directory and reports its root CID.
    /// Uses the legacy directory importer, separate from `run` snapshots.
    ///
    /// Requires at least one bin/*.wasm file (run 'ww build' first).
    Push {
        /// Path to the wetware environment (default: current directory)
        #[arg(default_value = ".", value_name = "PATH")]
        path: PathBuf,

        /// IPFS HTTP API endpoint
        #[arg(long, default_value = "http://localhost:5001")]
        ipfs_url: String,
    },

    /// Effectful operations that mutate state beyond the current directory.
    Perform {
        #[command(subcommand)]
        action: PerformAction,
    },

    /// Check the development environment for required and optional tools.
    ///
    /// Verifies the pinned native wasm32-wasip3 toolchain and Cargo.
    /// Optionally checks for Kubo (IPFS) and Ollama (LLM).
    ///
    /// Exit code 0 if all required checks pass; 1 otherwise.
    /// Optional checks never cause a non-zero exit.
    Doctor,

    /// Manage wetware namespaces.
    ///
    /// Namespaces map names (like `ww`) to IPFS directory trees that are
    /// mounted as FHS layers at boot. The standard library ships as the
    /// `ww` namespace.
    Ns {
        #[command(subcommand)]
        action: NsAction,
    },

    /// OCI container image operations via IPFS.
    Oci {
        #[command(subcommand)]
        action: OciAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Register wetware as a user-level background service.
    ///
    /// Generates a key if missing, creates a platform service file
    /// (launchd on macOS, systemd on Linux) from flags/defaults,
    /// and prints the activation command.
    Install {
        /// Path to an Ed25519 identity file. Defaults to ~/.ww/identity;
        /// generated automatically if the file does not exist.
        #[arg(long, value_name = "PATH")]
        identity: Option<PathBuf>,

        /// libp2p listen multiaddr. Repeatable; comma-separated via WW_LISTEN.
        /// Overrides the default TCP+QUIC v4/v6 set on port 2025.
        #[arg(
            long = "listen",
            value_name = "MULTIADDR",
            env = "WW_LISTEN",
            value_delimiter = ','
        )]
        listen: Vec<Multiaddr>,

        /// Mounts to run: source[:/guest/target]. Local paths require ./, /, or ~/.
        #[arg(long, value_name = "PATH")]
        images: Vec<String>,

        /// HTTP Routing V1 Gateway listener for host-owned IPNS publication.
        #[arg(
            long,
            default_value = "http://localhost:8080",
            env = "IPFS_ROUTING_API"
        )]
        ipns_routing_url: String,
    },

    /// Remove the platform service file.
    ///
    /// Removes the launchd plist (macOS) or systemd unit (Linux) and
    /// prints the deactivation command. Does not touch ~/.ww/identity.
    Uninstall,
}

#[derive(Subcommand)]
enum PerformAction {
    /// Bootstrap the ~/.ww user layer and daemon.
    ///
    /// Idempotent: re-running skips completed steps, retries failed ones.
    ///
    /// Steps:
    ///   1. Create ~/.ww directory structure
    ///   2. Generate Ed25519 identity at ~/.ww/identity (if missing)
    ///   3. Register background daemon (launchd/systemd)
    ///   4. Print summary with next steps
    Install {
        /// Canonical FHS directory containing boot/kernel.wasm and bin/status.wasm.
        #[arg(long)]
        payload: Option<PathBuf>,
        /// Final executable path (defaults to ~/.ww/bin/ww).
        #[arg(long)]
        executable: Option<PathBuf>,
        /// Validate payload and destination ownership without writing files.
        #[arg(long)]
        check: bool,
    },

    /// Remove the wetware daemon and optionally ~/.ww.
    ///
    /// Steps:
    ///   1. Stop and remove background daemon
    ///   2. Optionally remove ~/.ww (prompts for confirmation)
    Uninstall,

    /// Refresh WASM images and the daemon to match this binary.
    ///
    /// Safe to run repeatedly. Does not touch identity or directory structure.
    ///
    /// Steps:
    ///   1. Sync WASM images (CID compare, overwrite if changed)
    ///   2. Republish standard library (if images changed and Kubo running)
    ///   3. Regenerate daemon service file
    ///   4. Restart daemon
    Update,

    /// Disabled until verified Host upgrades are implemented. Use the installer.
    Upgrade {
        /// IPFS HTTP API endpoint.
        #[arg(long, default_value = "http://localhost:5001", env = "IPFS_API")]
        ipfs_url: String,
    },
}

#[derive(Subcommand)]
enum OciAction {
    /// Pull the wetware container image from IPFS and load it into Docker/podman.
    ///
    /// Resolves the OCI tar from /ipns/releases.wetware.run/oci/image.tar
    /// and pipes it to `docker load` or `podman load`.
    ///
    /// Examples:
    ///   ww oci import
    ///   ww oci import --cid QmHash...
    ///   ww oci import --stdout | podman load
    Import {
        /// Use a specific CID instead of resolving IPNS.
        #[arg(long)]
        cid: Option<String>,

        /// Write the image tar to stdout instead of loading automatically.
        #[arg(long)]
        stdout: bool,

        /// IPFS HTTP API endpoint.
        #[arg(long, default_value = "http://localhost:5001", env = "IPFS_API")]
        ipfs_url: String,
    },
}

#[derive(Subcommand)]
enum NsAction {
    /// List configured namespaces.
    List,

    /// Add or update a namespace.
    ///
    /// Writes a config file to ~/.ww/etc/ns/<name>.
    Add {
        /// Namespace name (e.g., 'ww', 'myorg')
        #[arg(value_name = "NAME")]
        name: String,

        /// IPNS key for live resolution
        #[arg(long, value_name = "KEY")]
        ipns: Option<String>,

        /// Bootstrap IPFS path (e.g., /ipfs/bafyrei...)
        #[arg(long, value_name = "PATH")]
        bootstrap: Option<String>,
    },

    /// Remove a namespace.
    Remove {
        /// Namespace name to remove
        #[arg(value_name = "NAME")]
        name: String,
    },

    /// Resolve a namespace to its current IPFS CID.
    Resolve {
        /// Namespace name to resolve
        #[arg(value_name = "NAME")]
        name: String,
    },
}

/// Strip the `/p2p/<peer-id>` suffix from a multiaddr string, if present.
fn strip_p2p_suffix(addr: &str) -> &str {
    if let Some(idx) = addr.find("/p2p/") {
        &addr[..idx]
    } else {
        addr
    }
}

/// Parse Kubo bootstrap info into a `KuboBootstrapInfo` suitable for seeding
/// the in-process Kademlia client.
///
/// Prefers a loopback TCP address (the typical case when Kubo runs locally).
/// Falls back to any parseable TCP multiaddr.  Returns `None` if no suitable
/// address can be found.
fn parse_kubo_bootstrap(info: &ipfs::KuboInfo) -> Option<host::KuboBootstrapInfo> {
    let peer_id: libp2p::PeerId = info.peer_id.parse().ok()?;

    // Try loopback TCP first (Kubo on same machine), then any TCP addr.
    let addr = info
        .swarm_addrs
        .iter()
        .filter_map(|s| strip_p2p_suffix(s).parse::<Multiaddr>().ok())
        .find(|a| {
            let s = a.to_string();
            s.contains("/ip4/127.0.0.1/tcp/") || s.contains("/ip4/127.0.0.1/udp/")
        })
        .or_else(|| {
            info.swarm_addrs
                .iter()
                .filter_map(|s| strip_p2p_suffix(s).parse::<Multiaddr>().ok())
                .find(|a| a.to_string().contains("/tcp/"))
        })?;

    tracing::info!(
        kubo_peer = %peer_id,
        %addr,
        "Bootstrapping Kad client against Kubo (Amino DHT)"
    );

    Some(host::KuboBootstrapInfo { peer_id, addr })
}

/// Environment override for the kubo-readiness wait deadline, in seconds.
/// `0` means wait indefinitely (the production posture).
const KUBO_WAIT_MAX_SECS_ENV: &str = "WW_KUBO_WAIT_MAX_SECS";

/// Default kubo-readiness deadline when [`KUBO_WAIT_MAX_SECS_ENV`] is unset.
/// Generous enough to cover a kubo sidecar coming up alongside `ww`, short
/// enough that a dev running `ww run` without kubo gets a clear error rather
/// than an indefinite hang. Deploys set the env to `0` for unbounded waiting.
const KUBO_WAIT_DEFAULT_SECS: u64 = 120;

/// Environment override for how long one retryable Kubo boot call may keep
/// retrying. `0` means retry indefinitely.
const KUBO_BOOT_RETRY_MAX_SECS_ENV: &str = "WW_KUBO_BOOT_RETRY_MAX_SECS";

/// Development default for the retry budget. Production may opt into
/// indefinite recovery independently of the initial Kubo-readiness wait.
const KUBO_BOOT_RETRY_MAX_SECS_DEFAULT: u64 = 120;

/// Environment override for a single Kubo-dependent boot operation. A timeout
/// here never applies to guest content transfer or ordinary DHT operations.
const KUBO_BOOT_OPERATION_TIMEOUT_SECS_ENV: &str = "WW_KUBO_BOOT_OPERATION_TIMEOUT_SECS";

/// Long enough for normal cold IPNS/DHT work, short enough to turn a Kubo
/// no-progress state into an observable retry instead of a permanently
/// liveness-green, readiness-closed process.
const KUBO_BOOT_OPERATION_TIMEOUT_DEFAULT_SECS: u64 = 90;

fn kubo_env_u64(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("{name} must be an unsigned integer, got {value:?}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow::anyhow!("failed to read {name}: {error}")),
    }
}

fn kubo_wait_max_secs() -> Result<u64> {
    kubo_env_u64(KUBO_WAIT_MAX_SECS_ENV, KUBO_WAIT_DEFAULT_SECS)
}

fn kubo_boot_retry_max_secs() -> Result<u64> {
    kubo_env_u64(
        KUBO_BOOT_RETRY_MAX_SECS_ENV,
        KUBO_BOOT_RETRY_MAX_SECS_DEFAULT,
    )
}

fn kubo_boot_operation_timeout_secs() -> Result<u64> {
    let secs = kubo_env_u64(
        KUBO_BOOT_OPERATION_TIMEOUT_SECS_ENV,
        KUBO_BOOT_OPERATION_TIMEOUT_DEFAULT_SECS,
    )?;
    if secs == 0 {
        bail!(
            "{KUBO_BOOT_OPERATION_TIMEOUT_SECS_ENV}=0 is unsafe for boot-critical Kubo calls; use a positive number"
        );
    }
    Ok(secs)
}

/// Core of [`wait_for_kubo_ready`] with an explicit deadline (`max_secs`; `0` =
/// unbounded), split out so tests can drive it without touching process env.
async fn wait_for_kubo_ready_with(client: &ipfs::HttpClient, max_secs: u64) -> Result<()> {
    use std::time::{Duration, Instant};

    let deadline = (max_secs > 0).then(|| Instant::now() + Duration::from_secs(max_secs));

    // Fast path: usually kubo is already up.
    if client.kubo_info().await.is_ok() {
        return Ok(());
    }

    let mut backoff = Duration::from_millis(500);
    let backoff_cap = Duration::from_secs(15);
    let started = Instant::now();
    let mut attempt: u64 = 0;

    loop {
        attempt += 1;
        match client.kubo_info().await {
            Ok(_) => {
                tracing::info!(
                    attempt,
                    waited_secs = started.elapsed().as_secs(),
                    "kubo reachable; proceeding with boot"
                );
                return Ok(());
            }
            Err(e) => {
                let waited = started.elapsed();
                // Escalate to warn after the first few misses so a genuinely
                // stuck dependency is visible in logs, while a normal
                // startup race stays quiet.
                if attempt <= 3 {
                    tracing::debug!(attempt, error = %e, "kubo not ready; waiting");
                } else {
                    tracing::warn!(
                        attempt,
                        waited_secs = waited.as_secs(),
                        error = %e,
                        "kubo still not ready; retrying (staying alive rather than crash-looping)"
                    );
                }

                if let Some(deadline) = deadline {
                    if Instant::now() + backoff >= deadline {
                        anyhow::bail!(
                            "kubo not reachable after {}s at {}; set {}=0 to wait indefinitely",
                            waited.as_secs(),
                            client.base_url(),
                            KUBO_WAIT_MAX_SECS_ENV
                        );
                    }
                }

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(backoff_cap);
            }
        }
    }
}

/// Parse a hex-encoded contract address (with or without 0x prefix) into 20 bytes.
fn parse_contract_address(s: &str) -> Result<[u8; 20]> {
    let hex_str = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(hex_str).context("Invalid hex in --stem address")?;
    if bytes.len() != 20 {
        bail!(
            "Contract address must be 20 bytes, got {} bytes",
            bytes.len()
        );
    }
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&bytes);
    Ok(addr)
}

struct AdvisoryTask(tokio::task::JoinHandle<()>);

impl Drop for AdvisoryTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn atom_advisory_candidates(
    http_url: String,
    ws_url: String,
    contract_address: [u8; 20],
) -> (watch::Receiver<Option<Vec<u8>>>, AdvisoryTask) {
    let (candidate_tx, candidate_rx) = watch::channel(None);
    let task = tokio::spawn(async move {
        let start_block = match atom::current_block_number(&http_url).await {
            Ok(block) => block,
            Err(error) => {
                tracing::debug!(%error, "Atom advisory setup failed; speculation disabled");
                return;
            }
        };
        let indexer = std::sync::Arc::new(atom::AtomIndexer::new(atom::IndexerConfig {
            ws_url,
            http_url,
            contract_address,
            start_block,
            getlogs_max_range: 2_000,
            reconnection: atom::ReconnectionConfig::default(),
        }));
        let mut events = indexer.subscribe();
        let relay = async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        candidate_tx.send_replace(Some(event.cid));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "Atom advisory events lagged; using latest event");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        };
        tokio::select! {
            result = std::sync::Arc::clone(&indexer).run() => {
                if let Err(error) = result {
                    tracing::debug!(%error, "Atom advisory indexer stopped; speculation disabled");
                }
            }
            _ = relay => {}
        }
    });
    (candidate_rx, AdvisoryTask(task))
}

impl Commands {
    async fn run(self) -> Result<()> {
        match self {
            Commands::Image { action } => action.run().await,
            Commands::Init { name } => Self::init(name).await,
            Commands::Build { path } => Self::build(path).await,
            Commands::Run {
                mounts: mount_args,
                mut namespace_root,
                fhs_root,
                listen,
                wasm_debug,
                obsolete_kernel,
                identity,
                insecure_ephemeral,
                stem,
                ipns_stem,
                ipns_routing_url,
                rpc_url,
                ws_url,
                confirmation_depth,
                executor_threads,
                http_listen,
                http_dial,
                runtime_cache_policy,
                with_http_admin,
                ipfs_url,
            } => {
                anyhow::ensure!(
                    obsolete_kernel.is_none() && std::env::var_os("WW_KERNEL").is_none(),
                    "--kernel and WW_KERNEL are no longer supported. Mount the kernel in the effective FHS: \
                     ww run ./kernel.wasm:/boot/kernel.wasm or \
                     ww run /ipfs/<CID>:/boot/kernel.wasm."
                );
                let (installed_baseline, mounts) = Self::startup_mounts(
                    &mount_args,
                    fhs_root,
                    &mut namespace_root,
                    stem.is_some() || ipns_stem.is_some(),
                )?;
                // Identity is passed separately — NOT as a mount.
                // The host reads it to create the signing key for the Membrane.
                // It must never enter the merged FHS tree (which is preopened
                // to guests and published to IPFS).
                let identity_path = identity.map(PathBuf::from);
                let listen = if listen.is_empty() {
                    daemon_cmd::default_listen()
                        .iter()
                        .map(|s| s.parse())
                        .collect::<Result<Vec<_>, _>>()
                        .context("parse default listen multiaddrs")?
                } else {
                    listen
                };
                Self::run_with_mounts(
                    mounts,
                    installed_baseline,
                    namespace_root,
                    identity_path,
                    insecure_ephemeral,
                    listen,
                    wasm_debug,
                    stem,
                    ipns_stem,
                    ipns_routing_url,
                    rpc_url,
                    ws_url,
                    confirmation_depth,
                    executor_threads,
                    http_listen,
                    http_dial,
                    runtime_cache_policy,
                    admin_listen_addr(with_http_admin),
                    ipfs_url,
                )
                .await
            }
            Commands::Healthcheck {
                admin,
                ready,
                expect_git_sha,
                require_cache_enabled,
                require_cache_warm,
                timeout_secs,
            } => {
                Self::healthcheck(
                    admin,
                    ready,
                    expect_git_sha,
                    require_cache_enabled,
                    require_cache_warm,
                    timeout_secs,
                )
                .await
            }
            Commands::Daemon { action } => match action {
                DaemonAction::Install {
                    identity,
                    listen,
                    images,
                    ipns_routing_url,
                } => Self::daemon_install(identity, listen, images, ipns_routing_url, false)
                    .await
                    .map(|_| ()),
                DaemonAction::Uninstall => Self::daemon_uninstall().await,
            },
            Commands::Push { path, ipfs_url } => Self::push(path, ipfs_url).await,
            Commands::Keygen { output } => Self::keygen(output).await,
            Commands::Perform { action } => match action {
                PerformAction::Install {
                    payload,
                    executable,
                    check,
                } => installation::install(payload, executable, check)
                    .await
                    .map(|_| ()),
                PerformAction::Uninstall => Self::perform_uninstall().await,
                PerformAction::Update => Self::perform_update().await,
                PerformAction::Upgrade { ipfs_url } => Self::perform_upgrade(ipfs_url).await,
            },
            Commands::Doctor => Self::doctor().await,
            Commands::Oci { action } => match action {
                OciAction::Import {
                    cid,
                    stdout,
                    ipfs_url,
                } => Self::oci_import(cid, stdout, ipfs_url).await,
            },
            Commands::Ns { action } => match action {
                NsAction::List => Self::ns_list().await,
                NsAction::Add {
                    name,
                    ipns,
                    bootstrap,
                } => Self::ns_add(name, ipns, bootstrap).await,
                NsAction::Remove { name } => Self::ns_remove(name).await,
                NsAction::Resolve { name } => Self::ns_resolve(name).await,
            },
        }
    }

    async fn admin_get(admin: &str, path: &str, timeout_secs: u64) -> Result<(u16, String)> {
        if admin.trim().eq_ignore_ascii_case("off") {
            bail!("admin endpoint is disabled");
        }
        let request = async {
            let mut stream = tokio::net::TcpStream::connect(admin)
                .await
                .with_context(|| format!("connect to admin endpoint at {admin}"))?;
            let request =
                format!("GET {path} HTTP/1.1\r\nHost: {admin}\r\nConnection: close\r\n\r\n");
            stream
                .write_all(request.as_bytes())
                .await
                .context("write admin request")?;
            let mut response = Vec::new();
            stream
                .read_to_end(&mut response)
                .await
                .context("read admin response")?;
            let response = String::from_utf8(response).context("admin response was not UTF-8")?;
            let (head, body) = response
                .split_once("\r\n\r\n")
                .context("admin response was not valid HTTP")?;
            let status = head
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .context("admin response did not include a status")?
                .parse::<u16>()
                .context("admin response status was not numeric")?;
            Ok((status, body.to_string()))
        };

        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), request)
            .await
            .with_context(|| format!("admin request to {path} timed out"))?
    }

    fn validate_version(
        body: &str,
        expected_git_sha: Option<&str>,
        require_cache_enabled: bool,
        require_cache_warm: bool,
    ) -> Result<()> {
        let version: serde_json::Value =
            serde_json::from_str(body).context("parse /version response")?;
        if let Some(expected) = expected_git_sha {
            let actual = version["git_sha"]
                .as_str()
                .context("/version omitted git_sha")?;
            if actual != expected {
                bail!("source revision mismatch: expected {expected}, found {actual}");
            }
        }
        if require_cache_enabled || require_cache_warm {
            let state = version["wasmtime_cache_state"]
                .as_str()
                .context("/version omitted wasmtime_cache_state")?;
            if state != "enabled" {
                bail!("Wasmtime cache is not enabled: state={state}");
            }
            let hits = version["wasmtime_cache_hits_total"]
                .as_u64()
                .context("/version omitted wasmtime_cache_hits_total")?;
            let stores = version["wasmtime_cache_stores_total"]
                .as_u64()
                .context("/version omitted wasmtime_cache_stores_total")?;
            if require_cache_enabled && hits == 0 && stores == 0 {
                bail!("Wasmtime cache is enabled but has recorded no hits or stores");
            }
            if require_cache_warm && hits == 0 {
                bail!(
                    "Wasmtime cache has no recorded load; require-cache-warm needs a prior-process cache hit"
                );
            }
        }
        Ok(())
    }

    async fn healthcheck(
        admin: String,
        ready: bool,
        expect_git_sha: Option<String>,
        require_cache_enabled: bool,
        require_cache_warm: bool,
        timeout_secs: u64,
    ) -> Result<()> {
        let path = if ready { "/readyz" } else { "/healthz" };
        let (status, body) = Self::admin_get(&admin, path, timeout_secs).await?;
        if status != 200 {
            bail!("{path} returned HTTP {status}: {}", body.trim());
        }

        if expect_git_sha.is_some() || require_cache_enabled || require_cache_warm {
            let (status, body) = Self::admin_get(&admin, "/version", timeout_secs).await?;
            if status != 200 {
                bail!("/version returned HTTP {status}: {}", body.trim());
            }
            Self::validate_version(
                &body,
                expect_git_sha.as_deref(),
                require_cache_enabled,
                require_cache_warm,
            )?;
        }

        println!("ok");
        Ok(())
    }

    /// Initialize a new typed cell guest project.
    async fn init(name: String) -> Result<()> {
        let target_dir = PathBuf::from(&name);

        if target_dir.exists() {
            bail!("Directory already exists: {}", target_dir.display());
        }

        // Create directory structure
        std::fs::create_dir_all(target_dir.join("src"))?;

        // foo.capnp — skeleton interface
        let iface_name = to_pascal_case(&name);
        let file_id: u64 = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            name.hash(&mut h);
            h.finish() | (1u64 << 63)
        };
        let capnp_content = format!(
            r#"# {name} capability interface.

@0x{file_id:016x};

interface {iface_name} {{
  hello @0 (name :Text) -> (greeting :Text);
  # Replace with your methods.
}}
"#,
        );
        std::fs::write(target_dir.join(format!("{name}.capnp")), capnp_content)?;

        // Cargo.toml
        let cargo_toml = format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[workspace]  # standalone — not part of the host workspace

[dependencies]
capnp     = "0.25.3"
capnp-rpc = "0.25.0"
log       = "0.4"
system    = {{ path = "../../std/system" }}

[lib]
crate-type = ["cdylib"]

[build-dependencies]
capnpc    = "0.25.3"

[patch.crates-io]
capnp-rpc = {{ git = "https://github.com/wetware/capnproto-rust", rev = "b3befb30fa1cb17b3b49f278d1a6f02b2ef2860a" }}
"#
        );
        std::fs::write(target_dir.join("Cargo.toml"), cargo_toml)?;

        // build.rs — compiles the application schema; system provides platform bindings.
        let build_rs = format!(
            r#"fn main() {{
    capnpc::CompilerCommand::new()
        .file("{name}.capnp")
        .run()
        .expect("failed to compile {name}.capnp");
    println!("cargo:rerun-if-changed={name}.capnp");
}}
"#,
        );
        std::fs::write(target_dir.join("build.rs"), build_rs)?;

        // src/lib.rs — guest entry point
        let lib_rs = format!(
            r#"use std::rc::Rc;

use capnp::capability::Promise;
use system::{{system_capnp, Guest}};

#[allow(dead_code)]
mod {name}_capnp {{
    include!(concat!(env!("OUT_DIR"), "/{name}_capnp.rs"));
}}

type Membrane = system_capnp::membrane::Client;

// ---------------------------------------------------------------------------
// {iface_name} implementation
// ---------------------------------------------------------------------------

struct {iface_name}Impl;

#[allow(refining_impl_trait)]
impl {name}_capnp::{snake_name}::Server for {iface_name}Impl {{
    fn hello(
        self: Rc<Self>,
        params: {name}_capnp::{snake_name}::HelloParams,
        mut results: {name}_capnp::{snake_name}::HelloResults,
    ) -> Promise<(), capnp::Error> {{
        let name = capnp_rpc::pry!(capnp_rpc::pry!(params.get()).get_name())
            .to_str()
            .unwrap_or("world");
        results
            .get()
            .set_greeting(&format!("Hello, {{name}}!"));
        Promise::ok(())
    }}
}}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct {iface_name}Guest;

impl Guest for {iface_name}Guest {{
    async fn run() -> Result<(), ()> {{
        // Export the service capability through the guest RPC session.
        let impl_ = {iface_name}Impl;
        let client: {name}_capnp::{snake_name}::Client = capnp_rpc::new_client(impl_);
        log::info!("{name}: cell mode");
        let result = system::serve(client.client, |_membrane: Membrane| async move {{
            std::future::pending().await
        }})
        .await;
        result.map_err(|error| {{
            log::error!("{name} RPC failed: {{error}}");
        }})
    }}
}}

system::export!({iface_name}Guest);
"#,
            snake_name = name.replace('-', "_"),
        );
        std::fs::write(target_dir.join("src/lib.rs"), lib_rs)?;

        println!("Initialized cell project: {name}/");
        println!("  {name}.capnp            — capability interface (edit this)");
        println!("  Cargo.toml              — project configuration");
        println!("  build.rs                — schema compilation");
        println!("  src/lib.rs              — guest entry point");
        println!();
        println!("\u{2697}\u{fe0f}  Next steps:");
        println!("  1. Edit {name}.capnp with your interface methods");
        println!("  2. Implement the server in src/lib.rs");
        println!("  3. ww build {name}");
        Ok(())
    }

    async fn build(path: PathBuf) -> Result<()> {
        // Resolve '.' and relative spellings before deriving the image filename.
        let path = path
            .canonicalize()
            .with_context(|| format!("Failed to resolve project directory: {}", path.display()))?;
        let cargo_toml = path.join("Cargo.toml");

        if !cargo_toml.exists() {
            bail!(
                "Cargo.toml not found at: {}\n\
                 \n\
                 Please run 'ww init' first to scaffold a new environment.",
                cargo_toml.display()
            );
        }

        println!("Building WASM artifact for: {}", path.display());

        let toolchain = p3_toolchain::Toolchain::discover_and_validate()
            .context("Native WASI P3 toolchain is unavailable. Run 'ww doctor' for details")?;
        let target_root = path.join("target");

        let output = std::process::Command::new("cargo")
            .arg(format!("+{}", p3_toolchain::NIGHTLY))
            .args(["build", "-Z", "build-std=std,panic_abort"])
            .args(["--target", "wasm32-wasip3", "--release"])
            .arg("--message-format=json-render-diagnostics")
            .arg("--manifest-path")
            .arg(&cargo_toml)
            .env("CARGO_TARGET_WASM32_WASIP3_LINKER", &toolchain.component_ld)
            .env("CARGO_ENCODED_RUSTFLAGS", toolchain.encoded_rustflags())
            .env("CARGO_TARGET_DIR", &target_root)
            .output()
            .context("Failed to execute cargo build")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            bail!("cargo build failed:\n{}", stderr);
        }

        let src_wasm = built_wasm_artifact(&output.stdout, &cargo_toml)?;
        toolchain.validate_component(&src_wasm)?;

        // Copy WASM to bin/<name>.wasm
        let bin_dir = path.join("bin");
        std::fs::create_dir_all(&bin_dir).context("Failed to create bin directory")?;

        let project_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("Project directory must have a UTF-8 name")?;
        let dst_wasm = bin_dir.join(format!("{project_name}.wasm"));
        std::fs::copy(&src_wasm, &dst_wasm).context(format!(
            "Failed to copy {} to {}",
            src_wasm.display(),
            dst_wasm.display()
        ))?;

        println!("  bin/{project_name}.wasm");

        println!("Build complete: {}", path.display());
        Ok(())
    }

    /// Resolve the node's Ed25519 signing key.
    ///
    /// If an explicit `identity_path` is provided, load the key from it.
    /// Otherwise load `~/.ww/identity` by default.
    ///
    /// Ephemeral identity is only allowed when `allow_ephemeral` is true.
    ///
    /// The identity file is intentionally kept OUT of the merged FHS tree
    /// so that guests cannot read the private key via WASI filesystem access.
    /// Guests access signing only through the Signer capability in the Membrane.
    ///
    /// Returns `(signing_key, verifying_key, source_description)`.
    fn resolve_identity(
        identity_path: Option<&std::path::Path>,
        allow_ephemeral: bool,
    ) -> Result<(ed25519_dalek::SigningKey, VerifyingKey, &'static str)> {
        let default_path = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".ww/identity"));

        let resolved_path = identity_path
            .map(std::path::Path::to_path_buf)
            .or(default_path);

        if let Some(path) = resolved_path {
            if path.exists() {
                let path_str = path.to_str().context("identity path is non-UTF-8")?;
                let sk = ww::keys::load(path_str)?;
                let vk = sk.verifying_key();
                return Ok((sk, vk, "file"));
            }

            if allow_ephemeral {
                tracing::warn!(
                    path = %path.display(),
                    "Identity file not found; using insecure ephemeral key (--insecure-ephemeral)"
                );
                let sk = ww::keys::generate()?;
                let vk = sk.verifying_key();
                return Ok((sk, vk, "ephemeral"));
            }

            bail!(
                "Identity file not found: {}\n\
                 `ww run` requires a persistent identity by default.\n\
                 Create one with: ww keygen > ~/.ww/identity\n\
                 Or bypass (insecure, ephemeral) with: --insecure-ephemeral",
                path.display()
            );
        }

        if allow_ephemeral {
            tracing::warn!(
                "No HOME directory detected; using insecure ephemeral key (--insecure-ephemeral)"
            );
            let sk = ww::keys::generate()?;
            let vk = sk.verifying_key();
            Ok((sk, vk, "ephemeral"))
        } else {
            bail!(
                "No identity path provided and HOME is unset.\n\
                 Pass --identity <path> or use --insecure-ephemeral (insecure)."
            )
        }
    }

    /// Shared invocation-time source selection for run and image check.
    fn startup_mounts(
        mount_args: &[String],
        fhs_root: Option<PathBuf>,
        namespace_roots: &mut Vec<PathBuf>,
        authoritative_source: bool,
    ) -> Result<(Option<ww::cell::mount::Mount>, Vec<ww::cell::mount::Mount>)> {
        let home = dirs::home_dir();
        let baseline = Self::installed_baseline_mount(fhs_root, home.as_deref())?;
        let mut mounts = ww::cell::mount::parse_args(mount_args)?;
        ww::cell::mount::freeze_local_paths(
            &mut mounts,
            &std::env::current_dir().context("read invocation CWD")?,
            home.as_deref(),
        )?;
        if mounts.is_empty() && namespace_roots.is_empty() && !authoritative_source {
            if let Some(home) = home {
                namespace_roots.push(home.join(".ww"));
            }
        }
        Ok((baseline, mounts))
    }

    /// Discover the publishable baseline without importing private installation state.
    fn installed_baseline_mount(
        configured: Option<PathBuf>,
        home: Option<&Path>,
    ) -> Result<Option<ww::cell::mount::Mount>> {
        let explicit = configured.is_some();
        let Some(mut path) = configured.or_else(|| home.map(|home| home.join(".ww/fhs"))) else {
            return Ok(None);
        };
        if let Ok(relative) = path.strip_prefix("~") {
            path = home
                .context("HOME is required for a ~/ FHS root")?
                .join(relative);
        }
        anyhow::ensure!(
            path.is_absolute(),
            "--fhs-root / WW_FHS_ROOT must be an absolute FHS directory: {}",
            path.display()
        );
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("inspect installed FHS {}; install a canonical FHS or set --fhs-root to an existing directory", path.display())),
        };
        anyhow::ensure!(
            metadata.is_dir(),
            "installed FHS must be a directory without symlink traversal: {}",
            path.display()
        );
        if let Some(home) = home {
            let root = path
                .canonicalize()
                .context("resolve installed FHS for private-state check")?;
            let home = home
                .canonicalize()
                .context("resolve HOME for private-state check")?;
            let private = home.join(".ww");
            let private = match private.canonicalize() {
                Ok(private) => private,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => private,
                Err(error) => return Err(error).context("resolve private installation directory"),
            };
            anyhow::ensure!(
                !private.starts_with(&root),
                "installed FHS must not contain private installation state: {}",
                path.display()
            );
        }
        Ok(Some(ww::cell::mount::Mount {
            source: path
                .to_str()
                .context("installed FHS path is not UTF-8")?
                .to_owned(),
            target: image::GuestTarget::root(),
        }))
    }

    /// Require an installed baseline, explicit image, or authoritative source before Host services.
    fn startup_namespaces(
        mounts: &[ww::cell::mount::Mount],
        namespace_roots: &[PathBuf],
        authoritative_source: bool,
        installed_baseline: bool,
    ) -> Result<Vec<ww::ns::NamespaceConfig>> {
        image::validate_mounts_virtual(mounts)?;
        // Metadata preflight keeps invalid local input actionable during a
        // backend outage. Capture still performs authoritative no-follow reads.
        for mount in mounts
            .iter()
            .filter(|mount| ww::cell::mount::is_local_source(&mount.source))
        {
            let metadata = std::fs::symlink_metadata(&mount.source)
                .with_context(|| format!("inspect local mount source {:?}", mount.source))?;
            anyhow::ensure!(
                metadata.is_dir() || metadata.is_file(),
                "local mount source must be a regular file or directory (symlinks are not allowed): {:?}",
                mount.source
            );
            anyhow::ensure!(
                !mount.is_root() || metadata.is_dir(),
                "root mount requires a directory; give file {:?} an explicit guest target",
                mount.source
            );
        }
        let local_roots = mounts
            .iter()
            .filter(|mount| mount.is_root() && ww::cell::mount::is_local_source(&mount.source))
            .map(|mount| Path::new(&mount.source));
        let scan_roots: Vec<&Path> = namespace_roots
            .iter()
            .map(PathBuf::as_path)
            .chain(local_roots)
            .collect();
        let configs = ww::ns::scan_namespace_configs(&scan_roots)
            .context("Failed to scan namespace configs")?;
        let mut namespace_images = 0;
        for config in &configs {
            if let Some(source) = config.ipfs_path()? {
                // Validate configured image syntax before waiting on Kubo.
                ww::cell::mount::parse_args(&[source])?;
                namespace_images += 1;
            }
        }
        anyhow::ensure!(
            mounts.len() + namespace_images + usize::from(authoritative_source) + usize::from(installed_baseline)
                <= ww::cell::mount::MAX_MOUNT_OPERATIONS,
            "mount operation limit exceeded (including installed baseline, namespace images, and authoritative head)"
        );
        if mounts.is_empty()
            && namespace_images == 0
            && !authoritative_source
            && !installed_baseline
        {
            bail!(
                "No installed FHS, explicit image source, or authoritative Stem configured. \
                 Install the canonical FHS, set --fhs-root, or pass a complete explicit source \
                 (for example, `ww run .`), a namespace root, --stem, or --ipns-stem."
            );
        }
        Ok(configs)
    }

    /// Generate a new Ed25519 identity secret.
    async fn keygen(output: Option<PathBuf>) -> Result<()> {
        let sk = ww::keys::generate()?;
        let encoded = ww::keys::encode(&sk);

        let kp = ww::keys::to_libp2p(&sk)?;
        let peer_id = kp.public().to_peer_id();

        if let Some(path) = output {
            ww::keys::save(&sk, &path)?;
            eprintln!("Secret written to: {}", path.display());
        } else {
            println!("{encoded}");
        }

        eprintln!("Peer ID:        {peer_id}");
        Ok(())
    }

    /// Register wetware as a user-level background service.
    ///
    /// When `quiet` is true, suppresses status output (used by `perform install`
    /// which prints its own summary).
    async fn daemon_install(
        identity: Option<PathBuf>,
        listen: Vec<Multiaddr>,
        images: Vec<String>,
        ipns_routing_url: String,
        quiet: bool,
    ) -> Result<bool> {
        daemon_cmd::daemon_install(identity, listen, images, ipns_routing_url, quiet).await
    }

    /// Remove the platform service file.
    async fn daemon_uninstall() -> Result<()> {
        daemon_cmd::daemon_uninstall().await
    }

    /// Run a wetware environment from parsed mounts.
    #[allow(clippy::too_many_arguments)]
    async fn run_with_mounts(
        mounts: Vec<ww::cell::mount::Mount>,
        installed_baseline: Option<ww::cell::mount::Mount>,
        namespace_roots: Vec<PathBuf>,
        identity: Option<PathBuf>,
        insecure_ephemeral: bool,
        listen: Vec<Multiaddr>,
        wasm_debug: bool,
        stem: Option<String>,
        ipns_stem: Option<String>,
        ipns_routing_url: String,
        rpc_url: String,
        ws_url: String,
        confirmation_depth: u64,
        executor_threads: usize,
        http_listen: Option<String>,
        http_dial: Vec<String>,
        runtime_cache_policy: String,
        with_http_admin: Option<String>,
        ipfs_url: String,
    ) -> Result<()> {
        let stem_contract = stem.as_deref().map(parse_contract_address).transpose()?;
        let ipns_stem_name = ipns_stem
            .as_deref()
            .map(ww::ipns::parse_name)
            .transpose()
            .context("parse --ipns-stem")?;
        let authoritative_source = stem_contract.is_some() || ipns_stem_name.is_some();
        let ns_configs = Self::startup_namespaces(
            &mounts,
            &namespace_roots,
            authoritative_source,
            installed_baseline.is_some(),
        )?;

        let (filesystem_owner, filesystem_resources) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::default(),
        )
        .context("failed to start Host filesystem lifecycle")?;
        let mut supervisor = ww::services::Host::new(
            std::sync::Arc::clone(&filesystem_resources),
            filesystem_owner,
        );
        let mut executor_pool_slot = None;
        let mut deployment_slot = None;
        let mut deployment_lifecycle_slot = None;
        let mut retained_startup_roots = Vec::new();

        let outcome: Result<i32> = async {
        let lifecycle = filesystem_resources
            .try_filesystem_scope()
            .context("failed to reserve deployment filesystem root")?;
        let deployment_scope = lifecycle.0.clone();
        deployment_lifecycle_slot = Some(lifecycle);

        ww::config::init_tracing_to_stderr(false);

        let ipfs_client = ipfs::HttpClient::new(ipfs_url);

        // Resolve the host identity and start the local admin plane before
        // waiting on Kubo or resolving mounts. This keeps liveness and boot
        // phase diagnostics available during dependency outages.
        tracing::debug!("resolving identity...");
        let (sk, _verifying_key, identity_source) =
            Self::resolve_identity(identity.as_deref(), insecure_ephemeral)?;
        tracing::info!(source = identity_source, "Node identity resolved");
        let keypair = ww::keys::to_libp2p(&sk)?;
        let peer_id = keypair.public().to_peer_id();
        let network_state = ww::rpc::NetworkState::from_peer_id(peer_id.to_bytes());
        let runtime_status = ww::metrics::RuntimeStatus::starting();
        let (epoch_tx, epoch_rx) = watch::channel(Epoch::zero());
        let kernel_ready_gate =
            std::sync::Arc::new(authority::KernelReadyGate::new(epoch_rx.clone()));
        // Allocate the WAGI route map before the admin plane so registrations
        // and HTTP dispatch share one map.
        let route_registry = http_listen
            .as_ref()
            .map(|_| ww::dispatcher::server::new_registry());
        let version_info = ww::metrics::VersionInfo {
            git_sha: env!("WW_BUILD_GIT_SHA").to_string(),
            // The OCI runtime digest is not intrinsically visible inside a
            // container. Deployments may inject the authoritative value;
            // deploy verification still checks pod.status.imageID directly.
            oci_image_id: std::env::var("WW_OCI_IMAGE_ID")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            kernel_identity: ww::kernel::IdentityState::pending(epoch_rx.clone()),
        };
        let kernel_identity = version_info.kernel_identity.clone();

        let fuel_registry = ww::metrics::new_fuel_registry();
        let rpc_metrics = ww::metrics::new_rpc_metrics();
        let cache_metrics = ww::metrics::new_cache_metrics();
        let stream_metrics = ww::metrics::new_stream_metrics();
        let wasmtime_cache_metrics = ww::cell::engine::wasmtime_cache_metrics();

        // Bind before spawning so an unavailable control-plane port is a
        // synchronous startup failure.
        if let Some(ref addr) = with_http_admin {
            let listen_addr: std::net::SocketAddr = addr
                .parse()
                .context("invalid --with-http-admin address (expected host:port)")?;
            let listener = std::net::TcpListener::bind(listen_addr)
                .with_context(|| format!("failed to bind --with-http-admin at {listen_addr}"))?;
            listener
                .set_nonblocking(true)
                .context("failed to configure --with-http-admin listener")?;
            supervisor.try_spawn(
                "admin",
                ww::metrics::AdminService {
                    listener,
                    peer_id: peer_id.to_string(),
                    network_state: network_state.clone(),
                    version_info,
                    runtime_status: runtime_status.clone(),
                    kernel_ready_gate: kernel_ready_gate.clone(),
                    route_registry: route_registry.clone(),
                    fuel_registry: fuel_registry.clone(),
                    rpc_metrics: rpc_metrics.clone(),
                    cache_metrics: cache_metrics.clone(),
                    stream_metrics: stream_metrics.clone(),
                    wasmtime_cache_metrics: wasmtime_cache_metrics.clone(),
                },
            )?;
        }

        // Every IPFS-backed boot operation below depends on Kubo. Wait before
        // namespace resolution, pinning, or mount assembly so a transient
        // outage remains observable through the already-running admin plane
        // instead of becoming a CrashLoopBackOff.
        runtime_status.set_phase("waiting-for-kubo");
        let kubo_wait_max_secs = kubo_wait_max_secs()?;
        let kubo_boot_retry_max_secs = kubo_boot_retry_max_secs()?;
        let kubo_operation_timeout_secs = kubo_boot_operation_timeout_secs()?;
        let retry_status = runtime_status.clone();
        let boot_ipfs_client = ipfs::BootClient::new(
            ipfs_client.clone(),
            kubo_boot_retry_max_secs,
            kubo_operation_timeout_secs,
        )
        .with_retry_observer(std::sync::Arc::new(move |operation, attempt| {
            retry_status.mark_degraded(format!("Kubo boot operation {operation} is retrying"));
            tracing::debug!(operation, attempt, "Kubo boot retry status updated");
        }));
        tracing::info!(
            retry_max_secs = kubo_boot_retry_max_secs,
            operation_timeout_secs = kubo_operation_timeout_secs,
            "Kubo boot retry policy configured"
        );
        tokio::select! {
            result = wait_for_kubo_ready_with(&ipfs_client, kubo_wait_max_secs) => result?,
            service_exit = supervisor.next_service_exit() => {
                return Err(service_exit_error(service_exit));
            }
        }
        runtime_status.set_phase("resolving-configuration");

        let has_baseline = installed_baseline.is_some();
        let mut all_mounts: Vec<ww::cell::mount::Mount> = installed_baseline.into_iter().collect();
        // The installed `ww` namespace uses the host identity as its default
        // IPNS signer. Publish before namespace resolution so a newly migrated
        // name exists before Kubo resolves it. The republisher then becomes
        // the only writer for this state directory in the running process.
        let mut runtime_ns_configs = ns_configs.clone();
        if let Some(config) = ns_configs.iter().find(|config| config.name == "ww") {
            let configured_name = if config.ipns.is_empty() {
                None
            } else {
                // Namespace IPNS values may also be DNSLink names. Only a
                // PeerID-form value can identify the host-owned publisher.
                ww::ipns::parse_name(&config.ipns).ok()
            };
            if configured_name == Some(peer_id) {
                if let Some(path) = config.bootstrap_ipfs_path()? {
                    let desired: cid::Cid = path
                        .strip_prefix("/ipfs/")
                        .expect("validated bootstrap path has /ipfs prefix")
                        .parse()
                        .context("parse default ww namespace bootstrap CID")?;
                    let state_dir = dirs::home_dir()
                        .context("default IPNS publication requires a HOME directory")?
                        .join(".ww");
                    let routing = ww::ipns::RoutingClient::new(ipns_routing_url.clone())?;
                    let routing_ready = match routing.probe().await {
                        Ok(()) => true,
                        Err(error) if error.kind() == ww::ipns::RoutingErrorKind::Unsupported => {
                            return Err(anyhow::Error::new(error).context(
                                "default IPNS publication requires Kubo HTTP Routing V1",
                            ));
                        }
                        Err(error) if error.kind() == ww::ipns::RoutingErrorKind::Temporary => {
                            tracing::warn!(
                                "Initial Routing V1 probe failed; default IPNS publication will retry: {error}"
                            );
                            false
                        }
                        Err(error) => {
                            return Err(anyhow::Error::new(error)
                                .context("default IPNS Routing V1 probe failed"));
                        }
                    };
                    let mut publisher =
                        ww::ipns::Publisher::open(keypair.clone(), routing, &state_dir)?;
                    let initial_publish_succeeded = if routing_ready {
                        match publisher.publish(desired).await {
                            Ok(()) => true,
                            Err(error) if ww::ipns::is_temporary_routing_failure(&error) => {
                                tracing::warn!(
                                    "Initial host IPNS publication failed; republisher will retry: {error:#}"
                                );
                                false
                            }
                            Err(error) => {
                                return Err(error).context("initial host IPNS publication failed");
                            }
                        }
                    } else {
                        false
                    };
                    if !initial_publish_succeeded {
                        if let Some(runtime_config) = runtime_ns_configs
                            .iter_mut()
                            .find(|runtime_config| runtime_config.name == config.name)
                        {
                            // Until the signed record is visible, select the
                            // immutable bootstrap instead of treating a 404 as
                            // a terminal namespace-resolution error.
                            runtime_config.ipns.clear();
                        }
                    }
                    supervisor.try_spawn(
                        "ipns-republisher",
                        ww::ipns::Republisher::new(publisher, desired, initial_publish_succeeded),
                    )?;
                }
            }
        }

        // Namespace layers sit between stem (on-chain base) and user mounts.
        if !runtime_ns_configs.is_empty() {
            let resolved = tokio::select! {
                resolved = ww::ns::resolve_namespaces(
                    &runtime_ns_configs,
                    &boot_ipfs_client,
                    &runtime_status,
                ) => resolved.context("Failed to resolve namespace configs")?,
                service_exit = supervisor.next_service_exit() => {
                    return Err(service_exit_error(service_exit));
                }
            };
            for (name, ipfs_path) in &resolved {
                tracing::info!(ns = %name, path = %ipfs_path, "Mounting namespace");
                all_mounts.push(ww::cell::mount::Mount {
                    source: ipfs_path.clone(),
                    target: image::GuestTarget::root(),
                });
            }
        }

        // Append user-specified mounts after namespace layers.
        // User mounts are highest priority — they override everything.
        all_mounts.extend(mounts);

        // Freeze the complete ordered batch once. Deployment replays these
        // retained identities without reopening paths or resolving names again.
        runtime_status.set_phase("resolving-mounts");
        let mount_transaction = filesystem_resources.try_foreground_transaction(
            filesystem_resources.try_logical_io()?,
            &[&deployment_scope],
        )?;
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        let mut mount_resolution = Box::pin(image::resolve_frozen_mounts_with_cancel(
            &all_mounts,
            &boot_ipfs_client,
            &mount_transaction,
            authoritative_source,
            &mut cancel_rx,
        ));
        let resolved_mounts = tokio::select! {
            result = &mut mount_resolution => result?,
            service_exit = supervisor.next_service_exit() => {
                let _ = cancel_tx.send(true);
                if let Err(cleanup_error) = mount_resolution.await {
                    tracing::error!("mount resolution cancelled after service exit: {cleanup_error:#}");
                }
                return Err(service_exit_error(service_exit));
            }
        };
        drop(mount_resolution);
        drop(mount_transaction);
        let mut frozen_mounts = resolved_mounts.mounts;
        let installed_baseline = has_baseline.then(|| frozen_mounts.remove(0));
        let retained_source_roots = resolved_mounts.retained_roots;
        retained_startup_roots.clone_from(&retained_source_roots);
        let staging_setup = filesystem_resources.try_cleanup_transaction(
            filesystem_resources.try_logical_io()?,
            &[&deployment_scope],
        )?;
        let staging_cleanup = staging_setup.try_cleanup_phase()?;
        let staging_scope = deployment_scope.clone();
        let staging_dir = staging_setup
            .run_blocking(cache::FilesystemOperation::ProcessSetup, tempfile::TempDir::new)
            .await?
            .install(|directory| {
                let directory = directory.context("failed to create deployment staging dir")?;
                let path = directory.path().to_owned();
                staging_scope.own_cleanup_from_phase(
                    staging_cleanup,
                    cache::FilesystemOperation::ProcessCleanup,
                    move || directory.close(),
                )?;
                Ok::<_, anyhow::Error>(path)
            })?;
        drop(staging_setup);

        // Host-wide IPFS pin/content cache. CidTree uses this to materialize
        // CID-backed file content on demand. 128 MiB budget for pinned entries.
        let pinset_cache = std::sync::Arc::new(
            cache::PinsetCache::new(
                std::sync::Arc::new(ww::deployment::RetainedPinner::new(ipfs_client.clone())),
                128 * 1024 * 1024,
                std::sync::Arc::clone(&filesystem_resources),
            )
            .await
            .context("failed to create PinsetCache")?,
        );

        // Attempt to fetch Kubo's identity so we can bootstrap the in-process
        // Kad client against the local node (Amino DHT /ipfs/kad/1.0.0).
        // Non-fatal: if Kubo is unreachable we still start, just without Kad.
        tracing::debug!("fetching kubo info...");
        let kubo_bootstrap = match ipfs_client.kubo_info().await {
            Ok(info) => parse_kubo_bootstrap(&info),
            Err(e) => {
                tracing::warn!("Could not fetch Kubo identity (Kad DHT will not bootstrap): {e}");
                None
            }
        };

        // Fetch a random sample of Kubo's connected peers to seed the Kad
        // routing table.  Capped at K_VALUE (20) — the Kademlia replication
        // factor and the minimum for a single query to converge.  The
        // automatic bootstrap walk (triggered when entries < K) will
        // discover additional peers if needed.
        tracing::debug!("fetching kubo swarm peers...");
        let kubo_peers: Vec<(libp2p::PeerId, Multiaddr)> = match ipfs_client.swarm_peers().await {
            Ok(raw) => {
                use rand::seq::SliceRandom;
                let mut parsed: Vec<_> = raw
                    .into_iter()
                    .filter_map(|(peer_str, addr_str)| {
                        let peer_id: libp2p::PeerId = peer_str.parse().ok()?;
                        let addr: Multiaddr = addr_str.parse().ok()?;
                        Some((peer_id, addr))
                    })
                    .collect();
                const MAX_KUBO_PEERS: usize = 3; // Seed a few; Kad bootstrap walk finds the rest
                if parsed.len() > MAX_KUBO_PEERS {
                    let mut rng = rand::rng();
                    parsed.shuffle(&mut rng);
                    parsed.truncate(MAX_KUBO_PEERS);
                }
                parsed
            }
            Err(e) => {
                tracing::warn!("Could not fetch Kubo swarm peers: {e}");
                Vec::new()
            }
        };

        tracing::debug!("kubo peers fetched");

        // ---- Thread-per-subsystem runtime (Pingora-inspired) ----
        //
        // Each subsystem gets its own OS thread + single-threaded tokio
        // runtime.  The Host supervisor coordinates shutdown.
        let (swarm_cmd_tx, swarm_cmd_rx) = tokio::sync::mpsc::channel(64);
        let (swarm_ready_tx, swarm_ready_rx) = tokio::sync::oneshot::channel();

        // Swarm thread: libp2p event loop.
        // The libp2p Host is constructed inside the swarm thread so that
        // TCP listeners register with the correct tokio reactor.
        supervisor.try_spawn(
            "swarm",
            ww::services::SwarmService {
                params: ww::services::SwarmServiceParams {
                    listen: listen.clone(),
                    keypair,
                    kubo_bootstrap,
                    kubo_peers,
                },
                cmd_rx: swarm_cmd_rx,
                ready_tx: swarm_ready_tx,
                network_state: network_state.clone(),
            },
        )?;

        // Wait for the swarm thread to construct the host and send back
        // the stream control + network state.
        tracing::debug!("waiting for swarm ready...");
        let swarm_ready = tokio::select! {
            biased;
            ready = swarm_ready_rx => {
                ready
                    .context("swarm thread exited before reporting readiness")?
                    .context("swarm service failed to start")?
            }
            service_exit = supervisor.next_service_exit() => {
                return Err(service_exit_error(service_exit));
            }
        };
        tracing::debug!("swarm ready");
        let network_state = swarm_ready.network_state;
        let stream_control = swarm_ready.stream_control;
        // Executor pool: M:N cell scheduling across N worker threads.
        executor_pool_slot = Some(
            ww::services::ExecutorPool::try_new(
                executor_threads,
                supervisor.shutdown_rx(),
                std::sync::Arc::clone(&filesystem_resources),
            )
            .context("failed to start executor pool")?,
        );
        let executor_pool = executor_pool_slot
            .as_ref()
            .expect("executor pool was just installed");

        // Compilation service: offload component compilation from executor workers.
        let (compile_tx, compile_rx) = tokio::sync::mpsc::channel(64);
        supervisor.try_spawn(
            "compiler",
            ww::services::CompilationService {
                request_rx: compile_rx,
            },
        )?;

        // WAGI HTTP server thread (only when --http-listen is provided).
        if let (Some(addr), Some(registry)) = (http_listen.as_ref(), route_registry.as_ref()) {
            let listen_addr: std::net::SocketAddr = addr
                .parse()
                .context("invalid --http-listen address (expected host:port)")?;
            let listener = std::net::TcpListener::bind(listen_addr)
                .with_context(|| format!("failed to bind --http-listen at {listen_addr}"))?;
            listener
                .set_nonblocking(true)
                .context("failed to configure --http-listen listener")?;
            supervisor.try_spawn(
                "wagi-http",
                ww::services::WagiService {
                    listener,
                    registry: registry.clone(),
                },
            )?;
        }

        runtime_status.set_phase("preparing-deployment");
        let (candidate_rx, _advisory_task) = match stem_contract {
            Some(contract) => {
                let (receiver, task) = atom_advisory_candidates(rpc_url.clone(), ws_url, contract);
                (Some(receiver), Some(task))
            }
            None => (None, None),
        };
        let stem_source: Option<Box<dyn ww::stem::Source>> = if let Some(contract) = stem_contract {
            Some(Box::new(ww::stem::atom::Source::new(
                ww::stem::atom::Config::new(rpc_url.clone(), contract, confirmation_depth),
            )?))
        } else if let Some(name) = ipns_stem_name {
            let routing = ww::ipns::RoutingClient::new(ipns_routing_url.clone())?;
            match routing.probe().await {
                Ok(()) => {}
                Err(error) if error.kind() == ww::ipns::RoutingErrorKind::Unsupported => {
                    return Err(anyhow::Error::new(error)
                        .context("--ipns-stem requires Kubo HTTP Routing V1"));
                }
                Err(error) if error.kind() == ww::ipns::RoutingErrorKind::Temporary => tracing::warn!(
                    "Initial IPNS Stem Routing V1 probe failed; Source.current() will retry: {error}"
                ),
                Err(error) => {
                    return Err(anyhow::Error::new(error)
                        .context("--ipns-stem Routing V1 probe failed"));
                }
            }
            let state_dir = dirs::home_dir()
                .context("--ipns-stem requires a HOME directory for durable watermark state")?
                .join(".ww");
            Some(Box::new(ww::stem::ipns::Source::new(
                ww::stem::ipns::Config::new(name, ipns_routing_url.clone(), state_dir),
            )?))
        } else {
            None
        };
        let deployment_config = ww::deployment::Config {
            source: stem_source,
            candidates: candidate_rx,
            installed_baseline,
            runtime_engine: executor_pool.runtime_engine(),
            frozen_mounts,
            retained_source_roots,
            ipfs_client: ipfs_client.clone(),
            staging_dir,
            filesystem_host: std::sync::Arc::clone(&filesystem_resources),
            filesystem_scope: deployment_scope.clone(),
        };
        deployment_slot = Some(tokio::select! {
            result = ww::deployment::Deployment::bootstrap(
                deployment_config,
                epoch_tx,
                epoch_rx.clone(),
            ) => result?,
            service_exit = supervisor.next_service_exit() => {
                return Err(service_exit_error(service_exit));
            }
        });
        let deployment = deployment_slot
            .as_mut()
            .expect("deployment was just installed");
        let cid_tree = deployment.cid_tree();
        let rooted_epoch = deployment.current_epoch();
        let root_cid = rooted_epoch
            .root
            .as_ref()
            .context("deployment bootstrap returned an unrooted epoch")?;
        let image_path = format!("/ipfs/{root_cid}");
        tracing::debug!(seq = rooted_epoch.seq, root = %image_path, "deployment root resolved");

        let listen_summary = listen
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(",");
        tracing::info!(
            mounts = all_mounts.len(),
            root = %image_path,
            listen = %listen_summary,
            http = http_listen.as_deref().unwrap_or("disabled"),
            admin = with_http_admin.as_deref().unwrap_or("disabled"),
            "Booting environment"
        );

        let cache_policy = match runtime_cache_policy.as_str() {
            "shared" => ww::rpc::CachePolicy::Shared,
            "isolated" => ww::rpc::CachePolicy::Isolated,
            other => anyhow::bail!(
                "invalid --runtime-cache-policy '{}' (expected 'shared' or 'isolated')",
                other
            ),
        };

        let signing_key = std::sync::Arc::new(sk);
        let interactive = ww::kernel::is_interactive();
        let mut kernel_bootstrap = ww::kernel::Bootstrap::new(
            network_state.clone(),
            swarm_cmd_tx.clone(),
            signing_key,
            stream_control,
            http_dial,
        );
        if let Some(ref registry) = route_registry {
            kernel_bootstrap = kernel_bootstrap.with_route_registry(registry.clone());
        }
        let kernel_launcher = ww::deployment::KernelLauncher::new(
            executor_pool,
            kernel_identity,
            kernel_ready_gate,
            kernel_bootstrap,
            ww::kernel::RuntimeInputs::new(
                wasm_debug,
                executor_pool.runtime_engine(),
                compile_tx,
                cache_policy,
                pinset_cache,
                std::sync::Arc::clone(&filesystem_resources),
            ),
            ww::kernel::Stdio::Host,
        );

        let exit_code = 'generations: loop {
            runtime_status.set_phase("starting-kernel");
            let outcome = tokio::select! {
                outcome = deployment.run_generation(&kernel_launcher) => outcome?,
                service_exit = supervisor.next_service_exit() => {
                    tracing::error!(error = %service_exit_error(service_exit), "runtime supervision failed");
                    break 'generations 1;
                }
            };
            match outcome {
                ww::deployment::Outcome::Authoritative { epoch, result } => {
                    match result {
                        Ok(ww::kernel::Outcome::Exited(0)) => break 'generations 0,
                        Ok(ww::kernel::Outcome::Exited(code)) => {
                            tracing::error!(code, epoch, "Kernel generation exited with failure");
                        }
                        Ok(ww::kernel::Outcome::Terminated) => {
                            tracing::error!(
                                epoch,
                                "Kernel terminated without a deployment replacement"
                            );
                        }
                        Err(error) => tracing::error!(epoch, "Kernel error: {error}"),
                    }
                    let event_code = if epoch == 0 {
                        ww::rpc::graft::KernelEventCode::InitialInitFailed
                    } else {
                        ww::rpc::graft::KernelEventCode::EpochRestartInitFailed
                    };
                    tracing::error!(
                        event_code = event_code as u16,
                        epoch,
                        "Kernel generation failed to initialize"
                    );
                    break 'generations 1;
                }
                ww::deployment::Outcome::Replaced {
                    old_epoch,
                    new_epoch,
                    teardown_elapsed,
                } => {
                    tracing::info!(
                        event_code = ww::rpc::graft::KernelEventCode::GraftSuperseded as u16,
                        intended_seq = old_epoch,
                        live_seq = new_epoch,
                        "Kernel result superseded by newer generation"
                    );
                    if interactive {
                        tracing::info!(
                            event_code =
                                ww::rpc::graft::KernelEventCode::InteractiveReplaced as u16,
                            epoch = old_epoch,
                            "Interactive PID0 generation was replaced; restart ww run"
                        );
                        break 'generations 0;
                    }
                    tracing::info!(
                        event_code = ww::rpc::graft::KernelEventCode::GenerationReplaced as u16,
                        old_epoch,
                        new_epoch,
                        teardown_ms = teardown_elapsed.as_millis() as u64,
                        "Kernel generation replaced"
                    );
                }
                ww::deployment::Outcome::TeardownTimedOut { epoch, timeout } => {
                    tracing::error!(
                        event_code = ww::rpc::graft::KernelEventCode::TeardownTimeout as u16,
                        epoch,
                        teardown_ms = timeout.as_millis() as u64,
                        "Kernel generation teardown timed out"
                    );
                    break 'generations 1;
                }
            }
        };
        tracing::info!(code = exit_code, "Kernel exited");

        // Hold the CidTree alive until after guest exits; the outer epilogue
        // closes admission and drains workers/filesystem ownership in order.
        drop(cid_tree);
        Ok(exit_code)
        }
        .await;

        supervisor.close_filesystem_admission();
        if let Some(mut deployment) = deployment_slot.take() {
            deployment.shutdown().await;
        }
        if let Some(executor_pool) = executor_pool_slot.take() {
            executor_pool.shutdown_and_join().await;
        }
        let deployment_cleanup =
            if let Some((scope, owner, finalizer, completion)) = deployment_lifecycle_slot.take() {
                drop(scope);
                drop(owner);
                let finalization = finalizer.run().await;
                let proof = completion.wait().await;
                Some((finalization, proof))
            } else {
                None
            };
        let filesystem_shutdown = supervisor.shutdown_filesystem().await;
        supervisor.shutdown_services().await;

        let mut shutdown_errors = Vec::new();
        if let Some((finalization, proof)) = deployment_cleanup {
            if let Err(error) = finalization {
                shutdown_errors.push(format!(
                    "deployment filesystem finalization failed: {error}"
                ));
            }
            if let Err(error) = proof {
                shutdown_errors.push(format!("deployment filesystem completion failed: {error}"));
            }
        }
        if let Err(error) = filesystem_shutdown {
            shutdown_errors.push(format!("Host filesystem shutdown failed: {error}"));
        }
        let outcome = outcome.map_err(|error| {
            if retained_startup_roots.is_empty() {
                error
            } else {
                error.context(format!(
                    "frozen source pins remain retained after startup/deployment failure: {:?}",
                    retained_startup_roots
                ))
            }
        });
        match outcome {
            Ok(exit_code) if shutdown_errors.is_empty() => std::process::exit(exit_code),
            Ok(_) => Err(anyhow::anyhow!(shutdown_errors.join("; "))),
            Err(error) if shutdown_errors.is_empty() => Err(error),
            Err(error) => Err(error.context(format!(
                "ordered shutdown also failed: {}",
                shutdown_errors.join("; ")
            ))),
        }
    }

    /// Publish a wetware environment to IPFS
    async fn push(path: PathBuf, ipfs_url: String) -> Result<()> {
        // An image can contain several named components. Publication does not
        // choose an entrypoint or rewrite the input tree.
        let bin_dir = path.join("bin");
        let mut has_component = false;
        if bin_dir.is_dir() {
            for entry in
                std::fs::read_dir(&bin_dir).context("Failed to read image bin directory")?
            {
                let entry = entry?.path();
                if entry.is_file() && entry.extension().is_some_and(|ext| ext == "wasm") {
                    has_component = true;
                    break;
                }
            }
        }
        if !has_component {
            bail!(
                "No bin/*.wasm file found in: {}\n\
                 \n\
                 Please run 'ww build' first to compile your guest program.",
                path.display()
            );
        }

        println!("Publishing to IPFS...");

        // Add the environment directory to IPFS
        let ipfs_client = ipfs::HttpClient::new(ipfs_url);
        let cid = ipfs_client
            .add_dir(&path)
            .await
            .context("Failed to publish environment to IPFS")?;

        println!("Published to IPFS!");
        println!("CID: {}", cid);
        println!("IPFS path: /ipfs/{}", cid);
        println!("\nTo mount this image on a node:");
        println!("  ww run /ipfs/{}", cid);

        Ok(())
    }

    fn restart_user_daemon(home: &std::path::Path) -> Option<bool> {
        let plist_path = home.join("Library/LaunchAgents/io.wetware.ww.plist");
        let systemd_path = home.join(".config/systemd/user/ww.service");

        if cfg!(target_os = "macos") && plist_path.exists() {
            let _ = std::process::Command::new("launchctl")
                .args(["unload", &plist_path.display().to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let ok = std::process::Command::new("launchctl")
                .args(["load", &plist_path.display().to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            Some(ok)
        } else if cfg!(target_os = "linux") && systemd_path.exists() {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            let ok = std::process::Command::new("systemctl")
                .args(["--user", "restart", "ww"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            Some(ok)
        } else {
            None
        }
    }

    fn remove_user_daemon(home: &std::path::Path) -> Result<bool> {
        if cfg!(target_os = "macos") {
            let plist_path = home.join("Library/LaunchAgents/io.wetware.ww.plist");
            if plist_path.exists() {
                let _ = std::process::Command::new("launchctl")
                    .args(["unload", &plist_path.display().to_string()])
                    .status();
                std::fs::remove_file(&plist_path)
                    .with_context(|| format!("remove {}", plist_path.display()))?;
                return Ok(true);
            }
            return Ok(false);
        }

        if cfg!(target_os = "linux") {
            let unit_path = home.join(".config/systemd/user/ww.service");
            if unit_path.exists() {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "stop", "ww"])
                    .status();
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "disable", "ww"])
                    .status();
                std::fs::remove_file(&unit_path)
                    .with_context(|| format!("remove {}", unit_path.display()))?;
                return Ok(true);
            }
            return Ok(false);
        }

        bail!("unsupported platform; only macOS and Linux are supported")
    }

    /// Revalidate installed payload and refresh managed files through the installer.
    #[allow(clippy::unused_async)]
    async fn perform_update() -> Result<()> {
        if !installation::install(None, None, false).await? {
            println!("Daemon restart (nothing changed)");
            return Ok(());
        }
        let home = dirs::home_dir().context("cannot determine HOME")?;
        match Self::restart_user_daemon(&home) {
            Some(true) => Ok(()),
            _ => bail!(
                "installation updated; start the daemon with the activation command printed above"
            ),
        }
    }

    /// Remove the wetware daemon and optionally ~/.ww.
    #[allow(clippy::unused_async)]
    async fn perform_uninstall() -> Result<()> {
        let home = dirs::home_dir().context("Cannot determine home directory")?;
        let ww_dir = home.join(".ww");

        // Step 1: Stop and remove daemon.
        match Self::remove_user_daemon(&home)? {
            true => println!("  Daemon ...................... REMOVED"),
            false => println!("  Daemon ...................... NOT FOUND (already removed)"),
        }

        // Step 2: Optionally remove ~/.ww.
        let mut ww_dir_removed = false;
        if ww_dir.exists() {
            print!("  Remove ~/.ww? This deletes your identity and all data. [y/N] ");
            std::io::stdout().flush().ok();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if input.trim().eq_ignore_ascii_case("y") {
                std::fs::remove_dir_all(&ww_dir)
                    .with_context(|| format!("remove {}", ww_dir.display()))?;
                println!("  ~/.ww ....................... REMOVED");
                ww_dir_removed = true;
            } else {
                println!("  ~/.ww ....................... KEPT");
            }
        }

        println!();
        println!("Wetware uninstalled.");

        // Only mention the binary if it lives outside ~/.ww (or ~/.ww was kept).
        if let Ok(exe) = std::env::current_exe() {
            let exe = exe.canonicalize().unwrap_or(exe);
            let inside_ww_dir = exe.starts_with(&ww_dir);
            if !inside_ww_dir || !ww_dir_removed {
                println!(
                    "  Binary at {} not removed (delete manually if desired).",
                    exe.display()
                );
            }
        }

        Ok(())
    }

    /// Pull a container image from IPFS and load it into Docker/podman.
    async fn oci_import(cid: Option<String>, to_stdout: bool, ipfs_url: String) -> Result<()> {
        let ipfs_client = ipfs::HttpClient::new(ipfs_url);

        // Resolve the image tar path.
        let ipfs_path = if let Some(ref cid) = cid {
            format!("/ipfs/{}/oci/image.tar", cid.trim_start_matches("/ipfs/"))
        } else {
            // Resolve IPNS to get latest release.
            eprintln!("Resolving /ipns/releases.wetware.run ...");
            let resolved = ipfs_client
                .name_resolve("/ipns/releases.wetware.run")
                .await
                .context(
                    "IPNS resolution failed.\n\
                     \n\
                     Make sure Kubo is running (`ipfs daemon`) and connected to the DHT.\n\
                     Alternatively, specify a CID directly: ww oci import --cid <CID>",
                )?;
            format!("{}/oci/image.tar", resolved.trim_end_matches('/'))
        };

        eprintln!("Fetching {ipfs_path} ...");
        let tar_bytes = ipfs_client
            .cat(&ipfs_path)
            .await
            .context("Failed to fetch OCI image tar from IPFS")?;

        if to_stdout {
            // Write raw tar to stdout for manual piping.
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            out.write_all(&tar_bytes)
                .context("Failed to write image tar to stdout")?;
            out.flush()?;
            return Ok(());
        }

        // Detect container runtime.
        let runtime = Self::detect_container_runtime()?;
        eprintln!("Loading image via {runtime} ...");

        let mut child = std::process::Command::new(&runtime)
            .arg("load")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to start `{runtime} load`"))?;

        {
            use std::io::Write;
            let stdin = child.stdin.as_mut().context("Failed to open stdin pipe")?;
            stdin
                .write_all(&tar_bytes)
                .context("Failed to write image data to container runtime")?;
        }

        let status = child
            .wait()
            .context("Failed to wait on container runtime")?;
        if !status.success() {
            bail!("`{runtime} load` exited with status {status}");
        }

        println!("Loaded wetware/ww:latest from IPFS");
        Ok(())
    }

    /// Detect whether Docker or podman is available.
    fn detect_container_runtime() -> Result<String> {
        for runtime in &["docker", "podman"] {
            if std::process::Command::new(runtime)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return Ok(runtime.to_string());
            }
        }
        bail!(
            "Neither docker nor podman found in PATH.\n\
             \n\
             Install one of:\n\
             - Docker: https://docs.docker.com/get-docker/\n\
             - Podman: https://podman.io/getting-started/installation"
        )
    }

    /// Refuse before network access or filesystem changes.
    #[allow(clippy::unused_async)]
    async fn perform_upgrade(_ipfs_url: String) -> Result<()> {
        bail!("Native self-upgrade is disabled until verified Host upgrades are available. Use the current installer to install the complete Host and FHS package.")
    }

    /// List configured namespaces from ~/.ww/etc/ns/.
    #[allow(clippy::unused_async)]
    async fn ns_list() -> Result<()> {
        ns_cmd::ns_list().await
    }

    /// Add or update a namespace config.
    #[allow(clippy::unused_async)]
    async fn ns_add(name: String, ipns: Option<String>, bootstrap: Option<String>) -> Result<()> {
        ns_cmd::ns_add(name, ipns, bootstrap).await
    }

    /// Remove a namespace config.
    #[allow(clippy::unused_async)]
    async fn ns_remove(name: String) -> Result<()> {
        ns_cmd::ns_remove(name).await
    }

    /// Resolve a namespace to its current IPFS CID.
    async fn ns_resolve(name: String) -> Result<()> {
        ns_cmd::ns_resolve(name).await
    }

    /// Check the development environment.
    #[allow(clippy::unused_async)]
    async fn doctor() -> Result<()> {
        doctor_cmd::doctor().await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    cli.command.run().await
}

/// Convert a kebab-case or snake_case name to PascalCase.
/// "price-oracle" → "PriceOracle", "foo_bar" → "FooBar", "foo" → "Foo"
fn to_pascal_case(s: &str) -> String {
    s.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Select this project's artifact from this Cargo invocation, including cached
/// compiler-artifact messages. Never select stale files by directory iteration.
fn built_wasm_artifact(stdout: &[u8], manifest: &Path) -> Result<PathBuf> {
    let mut artifacts = std::collections::BTreeSet::new();
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: serde_json::Value =
            serde_json::from_slice(line).context("Invalid Cargo JSON output")?;
        if message["reason"] != "compiler-artifact"
            || message["manifest_path"].as_str().map(Path::new) != Some(manifest)
        {
            continue;
        }
        if let Some(filenames) = message["filenames"].as_array() {
            for filename in filenames.iter().filter_map(|value| value.as_str()) {
                let path = PathBuf::from(filename);
                if path.extension().is_some_and(|ext| ext == "wasm") {
                    artifacts.insert(path);
                }
            }
        }
    }
    if artifacts.len() != 1 {
        bail!(
            "Expected exactly one WASM artifact for {}, found {}. Configure a single guest target.",
            manifest.display(),
            artifacts.len()
        );
    }
    Ok(artifacts.into_iter().next().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_selects_only_the_current_projects_reported_wasm() {
        let messages = concat!(
            "{\"reason\":\"compiler-artifact\",\"manifest_path\":\"/dependency/Cargo.toml\",\"filenames\":[\"/target/dependency.wasm\"]}\n",
            "{\"reason\":\"compiler-artifact\",\"manifest_path\":\"/project/Cargo.toml\",\"filenames\":[\"/target/guest.wasm\",\"/target/guest.d\"],\"fresh\":true}\n",
            "{\"reason\":\"build-finished\",\"success\":true}\n",
        );
        assert_eq!(
            built_wasm_artifact(messages.as_bytes(), Path::new("/project/Cargo.toml")).unwrap(),
            PathBuf::from("/target/guest.wasm")
        );
    }

    #[test]
    fn build_rejects_missing_and_ambiguous_wasm_artifacts() {
        let manifest = Path::new("/project/Cargo.toml");
        assert!(built_wasm_artifact(b"", manifest).is_err());
        let ambiguous = br#"{"reason":"compiler-artifact","manifest_path":"/project/Cargo.toml","filenames":["/target/a.wasm","/target/b.wasm"]}"#;
        assert!(built_wasm_artifact(ambiguous, manifest).is_err());
        assert!(built_wasm_artifact(b"not JSON", manifest).is_err());
    }

    #[tokio::test]
    async fn wait_for_kubo_ready_times_out_against_unreachable_node() {
        // Point at a closed port; a finite deadline must surface an error
        // (dev fail-fast) rather than hanging, and the message must name the
        // env escape hatch.
        let client = ipfs::HttpClient::new("http://127.0.0.1:1".to_string());
        let start = std::time::Instant::now();
        let err = wait_for_kubo_ready_with(&client, 1)
            .await
            .expect_err("unreachable kubo with a 1s deadline must error");
        // Bounded: the deadline is 1s and backoff is capped, so this returns
        // quickly rather than looping forever.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "wait should honor the deadline, took {:?}",
            start.elapsed()
        );
        let msg = err.to_string();
        assert!(
            msg.contains(KUBO_WAIT_MAX_SECS_ENV),
            "error should point at the unbounded-wait env override: {msg}"
        );
    }

    #[test]
    fn test_resolve_identity_missing_path_errors_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("nonexistent");
        let err = Commands::resolve_identity(Some(missing.as_path()), false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("requires a persistent identity by default"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("ww keygen > ~/.ww/identity"),
            "missing keygen remediation in error: {msg}"
        );
        assert!(
            msg.contains("--insecure-ephemeral"),
            "missing explicit insecure bypass hint in error: {msg}"
        );
    }

    #[test]
    fn test_resolve_identity_ephemeral_when_missing_and_allowed() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("nonexistent");
        let (_, _, source) = Commands::resolve_identity(Some(missing.as_path()), true).unwrap();
        assert_eq!(source, "ephemeral");
    }

    #[test]
    fn test_resolve_identity_loads_existing() {
        let dir = tempfile::TempDir::new().unwrap();
        let id_path = dir.path().join("identity");
        // Write a known key.
        let sk = ww::keys::generate().unwrap();
        let encoded = ww::keys::encode(&sk);
        std::fs::write(&id_path, &encoded).unwrap();

        let (loaded_sk, _, source) =
            Commands::resolve_identity(Some(id_path.as_path()), false).unwrap();
        assert_eq!(source, "file");
        assert_eq!(ww::keys::encode(&loaded_sk), encoded);
    }

    #[test]
    fn test_resolve_identity_existing_file_wins_over_ephemeral_flag() {
        let dir = tempfile::TempDir::new().unwrap();
        let id_path = dir.path().join("identity");
        let sk = ww::keys::generate().unwrap();
        let encoded = ww::keys::encode(&sk);
        std::fs::write(&id_path, &encoded).unwrap();

        let (loaded_sk, _, source) =
            Commands::resolve_identity(Some(id_path.as_path()), true).unwrap();
        assert_eq!(source, "file");
        assert_eq!(ww::keys::encode(&loaded_sk), encoded);
    }

    #[test]
    fn run_has_no_implicit_current_directory_mount() {
        let cli = Cli::try_parse_from(["ww", "run"]).unwrap();
        let Commands::Run { mounts, .. } = cli.command else {
            panic!("expected run command");
        };
        assert!(mounts.is_empty());
    }

    #[test]
    fn startup_without_installed_or_explicit_sources_fails() {
        let error = Commands::startup_namespaces(&[], &[], false, false).unwrap_err();
        assert!(error.to_string().contains("No installed FHS"), "{error:#}");
    }

    #[test]
    fn startup_requires_an_actual_namespace_image() {
        let root = tempfile::tempdir().unwrap();
        let roots = vec![root.path().to_path_buf()];
        assert!(Commands::startup_namespaces(&[], &roots, false, false).is_err());
        std::fs::create_dir_all(root.path().join("etc/ns")).unwrap();
        std::fs::write(root.path().join("etc/ns/empty"), "# no image\n").unwrap();
        assert!(Commands::startup_namespaces(&[], &roots, false, false).is_err());
        std::fs::write(
            root.path().join("etc/ns/ww"),
            "bootstrap=bafkreigh2akiscaildcw453gm7nmiv5df6b5u3rtmw3klhyxqqzkjureme\n",
        )
        .unwrap();
        assert!(Commands::startup_namespaces(&[], &roots, false, false).is_ok());
    }

    #[test]
    fn startup_accepts_stem_or_explicit_targeted_source() {
        assert!(Commands::startup_namespaces(&[], &[], true, false).is_ok());
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("config");
        std::fs::write(&file, b"configuration").unwrap();
        let mounts =
            ww::cell::mount::parse_args(&[format!("{}:/etc/myapp", file.display())]).unwrap();
        assert!(Commands::startup_namespaces(&mounts, &[], false, false).is_ok());
    }

    #[tokio::test]
    async fn invalid_local_source_fails_before_identity_or_kubo() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        for (source, expected) in [
            (root.path().join("missing"), "inspect local mount source"),
            (file, "root mount requires a directory"),
        ] {
            let cli = Cli::try_parse_from([
                "ww",
                "run",
                source.to_str().unwrap(),
                "--ipfs-url",
                "http://127.0.0.1:1",
            ])
            .unwrap();
            let error = cli.command.run().await.unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn startup_rejects_local_symlink() {
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(root.path(), &link).unwrap();
        let mounts =
            ww::cell::mount::parse_args(&[format!("{}:/etc/app", link.display())]).unwrap();
        let error = Commands::startup_namespaces(&mounts, &[], false, false).unwrap_err();
        assert!(
            error.to_string().contains("symlinks are not allowed"),
            "{error:#}"
        );
    }

    #[test]
    fn startup_counts_authoritative_head_against_operation_bound() {
        let root = tempfile::tempdir().unwrap();
        let mut mounts =
            ww::cell::mount::parse_args(&vec![format!("{}:/etc/app", root.path().display()); 128])
                .unwrap();
        assert!(Commands::startup_namespaces(&mounts, &[], false, false).is_ok());
        assert!(Commands::startup_namespaces(&mounts, &[], true, false).is_err());
        mounts.pop();
        assert!(Commands::startup_namespaces(&mounts, &[], true, false).is_ok());
        assert!(Commands::startup_namespaces(&mounts, &[], true, true).is_err());
        mounts.pop();
        assert!(Commands::startup_namespaces(&mounts, &[], true, true).is_ok());
    }

    #[test]
    fn baseline_discovery_uses_only_the_installed_fhs() {
        let home = tempfile::tempdir().unwrap();
        assert!(Commands::installed_baseline_mount(None, Some(home.path()))
            .unwrap()
            .is_none());
        let fhs = home.path().join(".ww/fhs");
        std::fs::create_dir_all(&fhs).unwrap();
        std::fs::write(home.path().join(".ww/identity"), b"private").unwrap();
        let baseline = Commands::installed_baseline_mount(None, Some(home.path()))
            .unwrap()
            .unwrap();
        assert_eq!(baseline.source, fhs.to_str().unwrap());
        assert!(baseline.is_root());
        assert!(Commands::startup_namespaces(&[], &[], false, true).is_ok());
        assert!(
            Commands::installed_baseline_mount(Some(fhs.join("missing")), Some(home.path()))
                .is_err()
        );
        assert!(Commands::installed_baseline_mount(
            Some(PathBuf::from("relative")),
            Some(home.path())
        )
        .is_err());
    }

    #[test]
    fn baseline_rejects_private_ancestors_after_path_normalization() {
        let home = tempfile::tempdir().unwrap();
        let fhs = home.path().join(".ww/fhs");
        std::fs::create_dir_all(&fhs).unwrap();
        let error = Commands::installed_baseline_mount(Some(fhs.join("..")), Some(home.path()))
            .unwrap_err();
        assert!(error.to_string().contains("private installation state"));
    }

    #[cfg(unix)]
    #[test]
    fn baseline_rejects_a_symlink_root() {
        let home = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".ww")).unwrap();
        std::os::unix::fs::symlink(source.path(), home.path().join(".ww/fhs")).unwrap();
        assert!(Commands::installed_baseline_mount(None, Some(home.path())).is_err());
    }

    #[test]
    fn targeted_directory_does_not_supply_host_namespaces() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc/ns")).unwrap();
        std::fs::write(root.path().join("etc/ns/invalid"), "bootstrap=not-a-cid\n").unwrap();
        let mounts =
            ww::cell::mount::parse_args(&[format!("{}:/etc/app", root.path().display())]).unwrap();
        assert!(Commands::startup_namespaces(&mounts, &[], false, false)
            .unwrap()
            .is_empty());
        let mounts = ww::cell::mount::parse_args(&[root.path().display().to_string()]).unwrap();
        assert!(Commands::startup_namespaces(&mounts, &[], false, false).is_err());
    }

    fn parse_run_admin_addr(args: &[&str]) -> String {
        match Cli::try_parse_from(args)
            .expect("parse run command")
            .command
        {
            Commands::Run {
                with_http_admin, ..
            } => with_http_admin,
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn run_admin_defaults_to_localhost() {
        assert_eq!(parse_run_admin_addr(&["ww", "run"]), "127.0.0.1:2026");
    }

    #[test]
    fn run_rejects_removed_epoch_drain_option() {
        for value in ["0", "1"] {
            let error = Cli::try_parse_from(["ww", "run", "--epoch-drain-secs", value])
                .err()
                .expect("removed drain option must fail during parsing");
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
            assert!(error.to_string().contains("--epoch-drain-secs"));
        }
    }

    #[test]
    fn atom_and_ipns_stems_are_mutually_exclusive() {
        let error = Cli::try_parse_from([
            "ww",
            "run",
            "--stem",
            "0x0000000000000000000000000000000000000000",
            "--ipns-stem",
            "12D3KooWGuR5BdSqp23UeoeesuwYwW3ebQ9rZ8aVwfWEDU8kvCYJ",
        ])
        .err()
        .expect("Atom and IPNS Stem options must conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn run_admin_off_disables_case_insensitively() {
        for value in ["off", "OFF", " Off "] {
            let configured = parse_run_admin_addr(&["ww", "run", "--with-http-admin", value]);
            assert_eq!(admin_listen_addr(configured), None, "{value}");
        }
    }

    #[test]
    fn run_cli_accepts_explicit_fhs_root_without_changing_mount_priority() {
        let command = Cli::try_parse_from([
            "ww",
            "run",
            "./override:/boot/kernel.wasm",
            "--fhs-root",
            "/opt/wetware/fhs",
        ])
        .unwrap()
        .command;
        let Commands::Run {
            mounts, fhs_root, ..
        } = command
        else {
            panic!("expected run");
        };
        assert_eq!(mounts, ["./override:/boot/kernel.wasm"]);
        assert_eq!(fhs_root, Some(PathBuf::from("/opt/wetware/fhs")));
    }

    #[test]
    fn healthcheck_cli_supports_readiness_and_revision_checks() {
        let command = Cli::try_parse_from([
            "ww",
            "healthcheck",
            "--admin",
            "127.0.0.1:3030",
            "--ready",
            "--expect-git-sha",
            "0123456789abcdef",
            "--require-cache-enabled",
            "--require-cache-warm",
            "--timeout-secs",
            "7",
        ])
        .expect("parse healthcheck command")
        .command;

        match command {
            Commands::Healthcheck {
                admin,
                ready,
                expect_git_sha,
                require_cache_enabled,
                require_cache_warm,
                timeout_secs,
            } => {
                assert_eq!(admin, "127.0.0.1:3030");
                assert!(ready);
                assert_eq!(expect_git_sha.as_deref(), Some("0123456789abcdef"));
                assert!(require_cache_enabled);
                assert!(require_cache_warm);
                assert_eq!(timeout_secs, 7);
            }
            _ => panic!("expected healthcheck command"),
        }
    }

    #[tokio::test]
    async fn admin_get_parses_status_and_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let address = listener.local_addr().expect("test listener address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = [0u8; 512];
            let count = socket.read(&mut request).await.expect("read request");
            assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /healthz "));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n")
                .await
                .expect("write response");
        });

        let (status, body) = Commands::admin_get(&address.to_string(), "/healthz", 2)
            .await
            .expect("admin GET");
        assert_eq!(status, 200);
        assert_eq!(body, "ok\n");
        server.await.expect("test server");
    }

    #[test]
    fn healthcheck_version_validation_rejects_cache_fallback() {
        Commands::validate_version(
            r#"{"git_sha":"abc","wasmtime_cache_state":"enabled","wasmtime_cache_hits_total":0,"wasmtime_cache_stores_total":1}"#,
            Some("abc"),
            true,
            false,
        )
        .expect("enabled cache and matching revision");

        let error = Commands::validate_version(
            r#"{"git_sha":"abc","wasmtime_cache_state":"fallback","wasmtime_cache_hits_total":0,"wasmtime_cache_stores_total":0}"#,
            Some("abc"),
            true,
            false,
        )
        .expect_err("fallback cache must fail");
        assert!(format!("{error:#}").contains("state=fallback"));

        let error = Commands::validate_version(
            r#"{"git_sha":"abc","wasmtime_cache_state":"enabled","wasmtime_cache_hits_total":0,"wasmtime_cache_stores_total":0}"#,
            Some("abc"),
            true,
            false,
        )
        .expect_err("idle cache must fail after runtime readiness");
        assert!(format!("{error:#}").contains("no hits or stores"));

        Commands::validate_version(
            r#"{"git_sha":"abc","wasmtime_cache_state":"enabled","wasmtime_cache_hits_total":1,"wasmtime_cache_stores_total":0}"#,
            Some("abc"),
            false,
            true,
        )
        .expect("a cache hit proves a warm cache");

        let error = Commands::validate_version(
            r#"{"git_sha":"abc","wasmtime_cache_state":"enabled","wasmtime_cache_hits_total":0,"wasmtime_cache_stores_total":1}"#,
            Some("abc"),
            false,
            true,
        )
        .expect_err("a first-process store does not prove cross-process persistence");
        assert!(format!("{error:#}").contains("require-cache-warm"));

        let error = Commands::validate_version(
            r#"{"git_sha":"abc","wasmtime_cache_state":"disabled","wasmtime_cache_hits_total":1,"wasmtime_cache_stores_total":0}"#,
            Some("abc"),
            false,
            true,
        )
        .expect_err("a warm-cache check still requires an enabled cache");
        assert!(format!("{error:#}").contains("state=disabled"));
    }

    // ── Daemon service-file writer tests ──────────────────────────────
    //
    // These guard the `--http-listen` flag emission, which is the
    // single point of failure for the engagement starter kit demo:
    // if either the launchd plist or the systemd unit drops the flag,
    // the daemon comes up with no WAGI HTTP server and `curl /status`
    // gets connection refused. The `tests/status_cell_e2e.rs` integration
    // path bypasses these writers entirely (it spawns the cell via
    // Runtime/Executor in-process), so a regression in either writer
    // would slip past silently without these unit tests.

    fn config_with_http_listen(addr: &str) -> daemon_cmd::DaemonServiceConfig {
        daemon_cmd::DaemonServiceConfig {
            listen: vec!["/ip4/0.0.0.0/tcp/2025".to_string()],
            identity: PathBuf::from("/tmp/identity"),
            namespace_roots: vec![PathBuf::from("/tmp/host-state")],
            images: Vec::new(),
            http_listen: Some(addr.to_string()),
            ipfs_url: "http://localhost:5001".to_string(),
            ipns_routing_url: "http://localhost:8080".to_string(),
        }
    }

    fn config_no_http_listen() -> daemon_cmd::DaemonServiceConfig {
        daemon_cmd::DaemonServiceConfig {
            listen: vec!["/ip4/0.0.0.0/tcp/2025".to_string()],
            identity: PathBuf::from("/tmp/identity"),
            namespace_roots: Vec::new(),
            images: Vec::new(),
            http_listen: None,
            ipfs_url: "http://localhost:5001".to_string(),
            ipns_routing_url: "http://localhost:8080".to_string(),
        }
    }

    #[test]
    fn test_launchd_plist_emits_http_listen_when_set() {
        let home = tempfile::TempDir::new().unwrap();
        let config = config_with_http_listen("127.0.0.1:2080");
        let ww_bin = std::path::Path::new("/usr/local/bin/ww");

        let changed =
            daemon_cmd::write_launchd_plist(ww_bin, &config, home.path(), "/tmp/identity", true)
                .expect("write plist should succeed");
        assert!(changed, "a missing plist should count as changed");

        let changed =
            daemon_cmd::write_launchd_plist(ww_bin, &config, home.path(), "/tmp/identity", true)
                .expect("rewrite identical plist should succeed");
        assert!(!changed, "an identical plist should not count as changed");

        let plist =
            std::fs::read_to_string(home.path().join("Library/LaunchAgents/io.wetware.ww.plist"))
                .expect("plist should exist");

        assert!(
            plist.contains("<string>--http-listen</string>"),
            "plist should emit --http-listen flag, got:\n{plist}"
        );
        assert!(
            plist.contains("<string>127.0.0.1:2080</string>"),
            "plist should emit the configured listen address, got:\n{plist}"
        );
        assert!(
            plist.contains("<string>--namespace-root</string>")
                && plist.contains("<string>/tmp/host-state</string>"),
            "plist should read namespace configuration through a host-only root, got:\n{plist}"
        );
        assert!(
            plist.contains("<string>--ipns-routing-url</string>")
                && plist.contains("<string>http://localhost:8080</string>"),
            "plist should preserve the Routing V1 listener, got:\n{plist}"
        );
    }

    #[test]
    fn test_launchd_plist_omits_http_listen_when_none() {
        let home = tempfile::TempDir::new().unwrap();
        let config = config_no_http_listen();
        let ww_bin = std::path::Path::new("/usr/local/bin/ww");

        daemon_cmd::write_launchd_plist(ww_bin, &config, home.path(), "/tmp/identity", true)
            .expect("write plist should succeed");

        let plist =
            std::fs::read_to_string(home.path().join("Library/LaunchAgents/io.wetware.ww.plist"))
                .expect("plist should exist");

        assert!(
            !plist.contains("--http-listen"),
            "plist should NOT emit --http-listen when config.http_listen is None, got:\n{plist}"
        );
    }

    #[test]
    fn test_systemd_unit_emits_http_listen_when_set() {
        let home = tempfile::TempDir::new().unwrap();
        let config = config_with_http_listen("127.0.0.1:2080");
        let ww_bin = std::path::Path::new("/usr/local/bin/ww");

        let changed =
            daemon_cmd::write_systemd_unit(ww_bin, &config, home.path(), "/tmp/identity", true)
                .expect("write unit should succeed");
        assert!(changed, "a missing unit should count as changed");

        let changed =
            daemon_cmd::write_systemd_unit(ww_bin, &config, home.path(), "/tmp/identity", true)
                .expect("rewrite identical unit should succeed");
        assert!(!changed, "an identical unit should not count as changed");

        let unit = std::fs::read_to_string(home.path().join(".config/systemd/user/ww.service"))
            .expect("unit should exist");

        assert!(
            unit.contains("--http-listen 127.0.0.1:2080"),
            "systemd unit should emit --http-listen flag with addr, got:\n{unit}"
        );
        assert!(
            unit.contains("--namespace-root /tmp/host-state"),
            "systemd unit should read namespace configuration through a host-only root, got:\n{unit}"
        );
        assert!(
            unit.contains("--ipns-routing-url http://localhost:8080"),
            "systemd unit should preserve the Routing V1 listener, got:\n{unit}"
        );
    }

    #[test]
    fn test_systemd_unit_omits_http_listen_when_none() {
        let home = tempfile::TempDir::new().unwrap();
        let config = config_no_http_listen();
        let ww_bin = std::path::Path::new("/usr/local/bin/ww");

        daemon_cmd::write_systemd_unit(ww_bin, &config, home.path(), "/tmp/identity", true)
            .expect("write unit should succeed");

        let unit = std::fs::read_to_string(home.path().join(".config/systemd/user/ww.service"))
            .expect("unit should exist");

        assert!(
            !unit.contains("--http-listen"),
            "systemd unit should NOT emit --http-listen when config.http_listen is None, got:\n{unit}"
        );
    }

    #[test]
    fn default_fhs_refuses_unowned_legacy_status() {
        let home = tempfile::tempdir().unwrap();
        let ww_dir = home.path().join(".ww");
        let path = ww_dir.join("std/status/bin/status.wasm");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"custom legacy status").unwrap();
        assert!(daemon_cmd::refresh_default_fhs(&ww_dir).is_err());
        assert!(!ww_dir.join("fhs").exists());
        assert_eq!(std::fs::read(path).unwrap(), b"custom legacy status");
    }
}
