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
mod ns_cmd;

use ww::cell::image;
#[cfg(test)]
use ww::cell::loaders::EmbeddedLoader;
use ww::host;
use ww::ipfs;

// Embedded WASM blobs — compiled into the binary so standard cells work
// without requiring `make std` on the user's machine.
// build.rs sets cfg flags (has_wasm_*) when each file exists and is non-empty.
// In debug/test builds without `make std`, these are empty slices and the
// EmbeddedLoader gracefully falls through to the next loader in the chain.
#[cfg(has_wasm_std_kernel_bin_main_wasm)]
const EMBEDDED_KERNEL: &[u8] = include_bytes!("../../std/kernel/bin/main.wasm");
#[cfg(not(has_wasm_std_kernel_bin_main_wasm))]
const EMBEDDED_KERNEL: &[u8] = b"";

#[cfg(has_wasm_std_status_bin_status_wasm)]
const EMBEDDED_STATUS: &[u8] = include_bytes!("../../std/status/bin/status.wasm");
#[cfg(not(has_wasm_std_status_bin_status_wasm))]
const EMBEDDED_STATUS: &[u8] = b"";

/// Build the standard embedded loader with all bundled WASM images.
#[cfg(test)]
fn embedded_loader() -> EmbeddedLoader {
    let mut loader = EmbeddedLoader::new();
    // Important: do NOT register empty placeholders. If an empty blob is
    // inserted, EmbeddedLoader "wins" path resolution and masks downstream
    // loaders (HostPath/IPFS), causing zero-byte WASM loads.
    if !EMBEDDED_KERNEL.is_empty() {
        loader = loader.insert("bin/main.wasm", EMBEDDED_KERNEL);
    }
    if !EMBEDDED_STATUS.is_empty() {
        loader = loader.insert("bin/status.wasm", EMBEDDED_STATUS);
    }
    loader
}

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
enum Commands {
    /// Initialize a new typed cell guest project.
    ///
    /// Scaffolds a complete Rust project for a Wetware guest with
    /// Cap'n Proto schema, build script, and FHS boot layout.
    Init {
        /// Project name (e.g., 'oracle', 'greeter')
        #[arg(value_name = "NAME")]
        name: String,
    },

    /// Build a guest project, placing artifacts in bin/.
    ///
    /// Compiles a Rust project targeting wasm32-wasip2 and copies the
    /// artifact into the project's FHS root at boot/main.wasm.
    ///
    /// Expects Cargo.toml at the root of <path>.
    Build {
        /// Path to the wetware environment (default: current directory)
        #[arg(default_value = ".", value_name = "PATH")]
        path: PathBuf,
    },

    /// Run a wetware environment.
    ///
    /// Every positional argument is a mount source mounted at `/` (image layer).
    /// Targeted mounts (`source:/guest/path`) are rejected in backend virtual mode.
    ///
    /// Examples:
    ///   ww run .                                    # dev mode
    ///   ww run images/app
    ///   ww run /ipfs/QmHash /ipns/k51qzi5uqu5...
    Run {
        /// Mount source(s) at `/` (image layers).
        #[arg(default_value = ".", value_name = "MOUNT")]
        mounts: Vec<String>,

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

        /// Select pid0 by local path, CID, or explicit file:/cid:/embedded: prefix.
        /// CLI overrides WW_KERNEL; absent both, the embedded kernel is used.
        #[arg(long, value_name = "PATH|CID")]
        kernel: Option<String>,

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
        #[arg(long)]
        stem: Option<String>,

        /// HTTP JSON-RPC URL for finalized-depth Atom polling.
        #[arg(long, default_value = "http://127.0.0.1:8545")]
        rpc_url: String,

        /// Deprecated compatibility option. Atom following uses HTTP polling.
        #[arg(long, default_value = "ws://127.0.0.1:8545")]
        ws_url: String,

        /// Chain depth used when reading authoritative Atom.head() state.
        #[arg(long, default_value = "6")]
        confirmation_depth: u64,

        /// Deprecated and inert. Epoch authority now changes immediately.
        #[arg(long, default_value = "1")]
        epoch_drain_secs: u64,

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

    /// Snapshot a project and push it to IPFS.
    ///
    /// Adds the entire FHS tree to IPFS as a directory and returns the
    /// resulting CID. Optionally updates the on-chain Atom contract.
    ///
    /// Expects boot/main.wasm to exist (run 'ww build' first).
    Push {
        /// Path to the wetware environment (default: current directory)
        #[arg(default_value = ".", value_name = "PATH")]
        path: PathBuf,

        /// IPFS HTTP API endpoint
        #[arg(long, default_value = "http://localhost:5001")]
        ipfs_url: String,

        /// Atom contract address (hex, 0x-prefixed). If provided, updates
        /// the on-chain HEAD to point to the published CID.
        #[arg(long)]
        stem: Option<String>,

        /// HTTP JSON-RPC URL for eth_sendTransaction.
        #[arg(long, default_value = "http://127.0.0.1:8545")]
        rpc_url: String,

        /// Private key (hex, 0x-prefixed) to sign contract transactions.
        /// Required only if --stem is provided.
        #[arg(long)]
        private_key: Option<String>,
    },

    /// Effectful operations that mutate state beyond the current directory.
    Perform {
        #[command(subcommand)]
        action: PerformAction,
    },

    /// Check the development environment for required and optional tools.
    ///
    /// Verifies that the Rust toolchain, wasm32-wasip2 target, and Cargo
    /// are installed. Optionally checks for Kubo (IPFS) and Ollama (LLM).
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

        /// Image layers to run (local paths or IPFS CIDs).
        #[arg(long, value_name = "PATH")]
        images: Vec<String>,
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
    Install,

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

    /// Self-update the ww binary via IPNS.
    ///
    /// Resolves /ipns/releases.wetware.run/Cargo.toml to check for a
    /// newer version, then fetches the platform binary and atomically
    /// replaces the running executable, then runs `update` to refresh
    /// WASM images and daemon configuration.
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

impl Commands {
    async fn run(self) -> Result<()> {
        match self {
            Commands::Init { name } => Self::init(name).await,
            Commands::Build { path } => Self::build(path).await,
            Commands::Run {
                mounts: mount_args,
                listen,
                wasm_debug,
                kernel,
                identity,
                insecure_ephemeral,
                stem,
                rpc_url,
                ws_url,
                confirmation_depth,
                epoch_drain_secs,
                executor_threads,
                http_listen,
                http_dial,
                runtime_cache_policy,
                with_http_admin,
                ipfs_url,
            } => {
                let mounts = ww::cell::mount::parse_args(&mount_args)?;
                Self::validate_backend_mount_policy(&mounts)?;
                let env_kernel = match std::env::var("WW_KERNEL") {
                    Ok(value) => Some(value),
                    Err(std::env::VarError::NotPresent) => None,
                    Err(error) => return Err(anyhow::anyhow!("failed to read WW_KERNEL: {error}")),
                };
                let kernel_source =
                    ww::kernel::select_kernel_source(kernel.as_deref(), env_kernel.as_deref())?;
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
                    identity_path,
                    insecure_ephemeral,
                    listen,
                    wasm_debug,
                    kernel_source,
                    stem,
                    rpc_url,
                    ws_url,
                    confirmation_depth,
                    epoch_drain_secs,
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
                } => Self::daemon_install(identity, listen, images, false).await,
                DaemonAction::Uninstall => Self::daemon_uninstall().await,
            },
            Commands::Push {
                path,
                ipfs_url,
                stem,
                rpc_url,
                private_key,
            } => Self::push(path, ipfs_url, stem, rpc_url, private_key).await,
            Commands::Keygen { output } => Self::keygen(output).await,
            Commands::Perform { action } => match action {
                PerformAction::Install => Self::perform_install().await,
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
capnp     = "0.23.2"
capnp-rpc = "0.23.0"
log       = "0.4"
wasip2    = "1.0.2"
system    = {{ path = "../../std/system" }}

[lib]
crate-type = ["cdylib"]

[build-dependencies]
capnpc    = "0.23.3"
"#
        );
        std::fs::write(target_dir.join("Cargo.toml"), cargo_toml)?;

        // build.rs — compiles schemas
        let build_rs = format!(
            r#"use std::env;
use std::path::Path;

fn main() {{
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let manifest_path = Path::new(&manifest_dir);
    let capnp_dir = manifest_path
        .join("../..")
        .join("capnp")
        .canonicalize()
        .expect("capnp dir not found");

    let local_schema = manifest_path
        .join("{name}.capnp")
        .canonicalize()
        .expect("{name}.capnp not found next to Cargo.toml");

    // Pass 1: shared schemas
    capnpc::CompilerCommand::new()
        .src_prefix(&capnp_dir)
        .file(capnp_dir.join("system.capnp"))
        .file(capnp_dir.join("routing.capnp"))
        .file(capnp_dir.join("auth.capnp"))
        .file(capnp_dir.join("membrane.capnp"))
        .file(capnp_dir.join("stem.capnp"))
        .file(capnp_dir.join("http.capnp"))
        .run()
        .expect("failed to compile shared capnp schemas");

    // Pass 2: local schema
    capnpc::CompilerCommand::new()
        .src_prefix(manifest_path)
        .file(&local_schema)
        .run()
        .expect("failed to compile {name}.capnp");

    for schema in &["system", "routing", "auth", "membrane", "stem", "http"] {{
        println!(
            "cargo:rerun-if-changed={{}}",
            capnp_dir.join(format!("{{schema}}.capnp")).display()
        );
    }}
    println!("cargo:rerun-if-changed={{}}", local_schema.display());
}}
"#,
        );
        std::fs::write(target_dir.join("build.rs"), build_rs)?;

        // src/lib.rs — guest entry point
        let lib_rs = format!(
            r#"use std::rc::Rc;

use capnp::capability::Promise;
use wasip2::exports::cli::run::Guest;

#[allow(dead_code)]
mod system_capnp {{
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
}}

#[allow(dead_code)]
mod stem_capnp {{
    include!(concat!(env!("OUT_DIR"), "/stem_capnp.rs"));
}}

#[allow(dead_code)]
mod auth_capnp {{
    include!(concat!(env!("OUT_DIR"), "/auth_capnp.rs"));
}}

#[allow(dead_code)]
mod membrane_capnp {{
    include!(concat!(env!("OUT_DIR"), "/membrane_capnp.rs"));
}}

#[allow(dead_code)]
mod routing_capnp {{
    include!(concat!(env!("OUT_DIR"), "/routing_capnp.rs"));
}}

#[allow(dead_code)]
mod http_capnp {{
    include!(concat!(env!("OUT_DIR"), "/http_capnp.rs"));
}}

#[allow(dead_code)]
mod {name}_capnp {{
    include!(concat!(env!("OUT_DIR"), "/{name}_capnp.rs"));
}}

type InitialGrants = membrane_capnp::initial_grants::Client;

/// Look up a typed capability by name from the initial grants list.
fn get_initial_grant<T: capnp::capability::FromClientHook>(
    caps: &capnp::struct_list::Reader<'_, membrane_capnp::export::Owned>,
    name: &str,
) -> Result<T, capnp::Error> {{
    for i in 0..caps.len() {{
        let entry = caps.get(i);
        let n = entry.get_name()?.to_str().map_err(|e| capnp::Error::failed(e.to_string()))?;
        if n == name {{
            return entry.get_cap().get_as_capability::<T>();
        }}
    }}
    Err(capnp::Error::failed(format!(
        "required initial grant '{{name}}' is missing"
    )))
}}

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
    fn run() -> Result<(), ()> {{
        match std::env::args().nth(1).as_deref() {{
            Some("serve") => {{
                log::info!("{name}: serve");
                system::run(|initial_grants: InitialGrants| async move {{
                    let grants_resp = initial_grants.get_request().send().promise.await?;
                    let grants = grants_resp.get()?.get_caps()?;
                    let host: system_capnp::host::Client =
                        get_initial_grant(&grants, "host")?;

                    let id_resp = host.id_request().send().promise.await?;
                    let peer_id = id_resp.get()?.get_peer_id()?;
                    log::info!("{name}: peer {{:?}}", peer_id);

                    // TODO: provide on DHT, discover peers, etc.

                    Ok(())
                }});
            }}
            _ => {{
                // Default (no args): export the service capability.
                let impl_ = {iface_name}Impl;
                let client: {name}_capnp::{snake_name}::Client = capnp_rpc::new_client(impl_);
                log::info!("{name}: cell mode");
                system::serve(client.client, |_initial_grants: InitialGrants| async move {{
                    std::future::pending().await
                }});
            }}
        }}
        Ok(())
    }}
}}

wasip2::cli::command::export!({iface_name}Guest);
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

        // Run cargo build for wasm32-wasip2 target
        let output = std::process::Command::new("cargo")
            .args([
                "build",
                "--target",
                "wasm32-wasip2",
                "--release",
                "--manifest-path",
                cargo_toml
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("Invalid path"))?,
            ])
            .output()
            .context("Failed to execute cargo build")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            if stderr.contains("cannot find `wasm32-wasip2` target")
                || stderr.contains("target `wasm32-wasip2` not installed")
                || stderr.contains("target `wasm32-wasip1` not installed")
            {
                bail!(
                    "wasm32-wasip2 target is not installed.\n\
                     \n\
                     Install it with:\n\
                       rustup target add wasm32-wasip2"
                );
            }

            bail!("cargo build failed:\n{}", stderr);
        }

        // Find the built WASM artifact
        let target_dir = path.join("target/wasm32-wasip2/release");
        let mut wasm_file = None;

        // Look for the first .wasm file (or the crate name if it matches)
        for entry in std::fs::read_dir(&target_dir).context("Failed to read target directory")? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "wasm") {
                wasm_file = Some(path);
                break;
            }
        }

        let src_wasm = wasm_file.ok_or_else(|| {
            anyhow::anyhow!(
                "WASM artifact not found in {}. Check your Cargo.toml configuration.",
                target_dir.display()
            )
        })?;

        // Copy WASM to bin/<name>.wasm
        let bin_dir = path.join("bin");
        std::fs::create_dir_all(&bin_dir).context("Failed to create bin directory")?;

        let crate_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("main");
        let dst_wasm = bin_dir.join(format!("{crate_name}.wasm"));
        std::fs::copy(&src_wasm, &dst_wasm).context(format!(
            "Failed to copy {} to {}",
            src_wasm.display(),
            dst_wasm.display()
        ))?;

        println!("  bin/{crate_name}.wasm");

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

    /// Backend policy: only root mounts are allowed.
    ///
    /// Backend virtual mode uses one data plane. Publish content to IPFS/IPNS
    /// and mount it as a root layer.
    fn validate_backend_mount_policy(mounts: &[ww::cell::mount::Mount]) -> Result<()> {
        let targeted: Vec<&ww::cell::mount::Mount> =
            mounts.iter().filter(|m| !m.is_root()).collect();
        if targeted.is_empty() {
            return Ok(());
        }

        let mut details = String::new();
        for mount in targeted {
            details.push_str(&format!(
                "\n  - {}:{}",
                mount.source,
                mount.target.display()
            ));
        }

        bail!(
            "targeted mounts are not supported in backend virtual mode.\n\
             Use root image layers (`/ipfs/...`, `/ipns/...`, or local dirs) and publish data explicitly.\n\
             Offending mount(s):{details}"
        );
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
        quiet: bool,
    ) -> Result<()> {
        daemon_cmd::daemon_install(identity, listen, images, quiet).await
    }

    /// Remove the platform service file.
    async fn daemon_uninstall() -> Result<()> {
        daemon_cmd::daemon_uninstall().await
    }

    /// Run a wetware environment from parsed mounts.
    #[allow(clippy::too_many_arguments)]
    async fn run_with_mounts(
        mounts: Vec<ww::cell::mount::Mount>,
        identity: Option<PathBuf>,
        insecure_ephemeral: bool,
        listen: Vec<Multiaddr>,
        wasm_debug: bool,
        kernel_source: ww::kernel::Source,
        stem: Option<String>,
        rpc_url: String,
        _ws_url: String,
        confirmation_depth: u64,
        epoch_drain_secs: u64,
        executor_threads: usize,
        http_listen: Option<String>,
        http_dial: Vec<String>,
        runtime_cache_policy: String,
        with_http_admin: Option<String>,
        ipfs_url: String,
    ) -> Result<()> {
        // Dev-mode compat: if a single local root mount has boot/main.wasm
        // but not bin/main.wasm, copy it over (the runtime expects bin/).
        for mount in &mounts {
            if mount.is_root() && !ipfs::is_ipfs_path(&mount.source) {
                let src = Path::new(&mount.source);
                let boot_wasm = src.join("boot/main.wasm");
                let bin_wasm = src.join("bin/main.wasm");
                if boot_wasm.exists() && !bin_wasm.exists() {
                    let bin_dir = src.join("bin");
                    std::fs::create_dir_all(&bin_dir).context("Failed to create bin directory")?;
                    std::fs::copy(&boot_wasm, &bin_wasm)
                        .context("Failed to prepare WASM artifact for runtime")?;
                }
            }
        }

        // Reject local configuration before Kubo readiness can intentionally
        // wait forever in production. A bad path must remain actionable.
        if !mounts.is_empty() || stem.is_none() {
            image::validate_mounts_virtual(&mounts)?;
        }
        let user_mounts = mounts.clone();
        let local_roots: Vec<&std::path::Path> = mounts
            .iter()
            .filter(|mount| mount.is_root() && !ww::ipfs::is_ipfs_path(&mount.source))
            .map(|mount| std::path::Path::new(&mount.source))
            .collect();
        let ns_configs = ww::ns::scan_namespace_configs(&local_roots)
            .context("Failed to scan namespace configs")?;

        ww::config::init_tracing_to_stderr(false);

        if epoch_drain_secs != 0 {
            tracing::warn!(
                epoch_drain_secs,
                "--epoch-drain-secs is deprecated and has no effect"
            );
        }

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
            kernel_identity: ww::kernel::IdentityState::pending(&kernel_source),
        };
        let kernel_identity = version_info.kernel_identity.clone();

        let mut supervisor = ww::services::Host::new();
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
        runtime_status.set_phase("resolving-kernel");
        let resolved_kernel = kernel_source
            .resolve(ipfs_client.clone(), EMBEDDED_KERNEL)
            .await
            .with_context(|| {
                format!(
                    "failed to resolve kernel source '{}'",
                    kernel_source.record()
                )
            })?;
        let resolved_identity = resolved_kernel.identity();
        tracing::info!(
            kernel_source = %resolved_kernel.source.log_value(),
            kernel_cid = %resolved_identity.cid,
            source_cid = ?resolved_identity.source_cid,
            bytes = resolved_identity.size,
            "Kernel source resolved"
        );
        kernel_identity.publish(resolved_identity)?;
        // Cleanup is deliberately detached: it may use at most a short,
        // internal deadline, but boot must not wait for best-effort hygiene.
        let sweep_ipfs_client = boot_ipfs_client.clone();
        tokio::spawn(async move {
            match image::sweep_stale_mfs_namespaces(&sweep_ipfs_client).await {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "Stale MFS merge namespace sweep complete"),
                Err(error) => tracing::warn!("Stale MFS merge namespace sweep skipped: {error}"),
            }
        });
        runtime_status.set_phase("resolving-configuration");

        let mut all_mounts: Vec<ww::cell::mount::Mount> = Vec::new();
        let stem_contract = stem.as_deref().map(parse_contract_address).transpose()?;

        // Namespace layers sit between stem (on-chain base) and user mounts.
        if !ns_configs.is_empty() {
            let resolved = tokio::select! {
                resolved = ww::ns::resolve_namespaces(
                    &ns_configs,
                    &boot_ipfs_client,
                    &runtime_status,
                ) => resolved.context("Failed to resolve namespace configs")?,
                service_exit = supervisor.next_service_exit() => {
                    return Err(service_exit_error(service_exit));
                }
            };
            for (name, ipfs_path) in &resolved {
                tracing::info!(ns = %name, path = %ipfs_path, "Mounting namespace");
                // Pin the namespace tree so subsequent boots use the local copy.
                if let Err(error) = boot_ipfs_client.pin_add_once(ipfs_path).await {
                    tracing::warn!(ns = %name, path = %ipfs_path, "Failed to pin namespace: {error}");
                }
                all_mounts.push(ww::cell::mount::Mount {
                    source: ipfs_path.clone(),
                    target: PathBuf::from("/"),
                });
            }
        }

        // Append user-specified mounts after namespace layers.
        // User mounts are highest priority — they override everything.
        let user_layer_start = all_mounts.len();
        all_mounts.extend(mounts);

        // Resolve mounts into a merged root CID. Guest filesystem reads are
        // lazy via CidTree and backed by the IPFS DAG.
        runtime_status.set_phase("resolving-mounts");
        tracing::debug!("resolving mounts (virtual)...");
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        let mut mount_resolution = Box::pin(image::resolve_mount_layers_virtual_with_cancel(
            &all_mounts,
            &boot_ipfs_client,
            &mut cancel_rx,
        ));
        let layer_cids = tokio::select! {
            result = &mut mount_resolution => result?,
            service_exit = supervisor.next_service_exit() => {
                let _ = cancel_tx.send(true);
                if let Err(cleanup_error) = mount_resolution.await {
                    tracing::warn!("mount resolution cancelled after service exit: {cleanup_error:#}");
                }
                return Err(service_exit_error(service_exit));
            }
        };
        for (mount, cid) in user_mounts
            .iter()
            .zip(layer_cids.iter().skip(user_layer_start))
        {
            if ipfs::is_ipfs_path(&mount.source) {
                if let Err(error) = boot_ipfs_client.pin_add_once(cid).await {
                    tracing::warn!(%cid, "Failed to pin user IPFS layer: {error}");
                }
            }
        }
        let frozen_layers = layer_cids;
        let staging_dir = std::env::temp_dir().join(format!("ww-staging-{}", std::process::id()));
        std::fs::create_dir_all(&staging_dir)
            .with_context(|| format!("failed to create staging dir {}", staging_dir.display()))?;

        // Host-wide IPFS pin/content cache. CidTree uses this to materialize
        // CID-backed file content on demand. 128 MiB budget for pinned entries.
        let pinset_cache = std::sync::Arc::new(
            cache::PinsetCache::new(std::sync::Arc::new(ipfs_client.clone()), 128 * 1024 * 1024)
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
        let executor_pool =
            ww::services::ExecutorPool::try_new(executor_threads, supervisor.shutdown_rx())
                .context("failed to start executor pool")?;

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
        let stem_source: Option<Box<dyn ww::stem::Source>> = stem_contract
            .map(|contract| {
                ww::stem::atom::Source::new(ww::stem::atom::Config::new(
                    rpc_url.clone(),
                    contract,
                    confirmation_depth,
                ))
                .map(|source| Box::new(source) as Box<dyn ww::stem::Source>)
            })
            .transpose()?;
        let deployment_config = ww::deployment::Config {
            source: stem_source,
            frozen_layers,
            ipfs_client: ipfs_client.clone(),
            staging_dir,
        };
        let mut deployment = tokio::select! {
            result = ww::deployment::Deployment::bootstrap(
                deployment_config,
                epoch_tx,
                epoch_rx.clone(),
            ) => result?,
            service_exit = supervisor.next_service_exit() => {
                return Err(service_exit_error(service_exit));
            }
        };
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
            ipfs_client.clone(),
            http_dial,
        );
        if let Some(ref registry) = route_registry {
            kernel_bootstrap = kernel_bootstrap.with_route_registry(registry.clone());
        }
        let kernel_launcher = ww::deployment::KernelLauncher::new(
            &executor_pool,
            resolved_kernel,
            kernel_ready_gate,
            kernel_bootstrap,
            ww::kernel::RuntimeInputs::new(
                wasm_debug,
                executor_pool.engine(),
                compile_tx,
                cache_policy,
                pinset_cache,
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
                    std::process::exit(1);
                }
            }
        };
        tracing::info!(code = exit_code, "Kernel exited");

        supervisor.shutdown();

        // Hold the CidTree alive until after guest exits (its staging dir
        // backs open file descriptors). ExecutorPool must also be dropped
        // after the kernel exits but before process exit, to join worker
        // threads cleanly.
        drop(executor_pool);
        drop(cid_tree);
        std::process::exit(exit_code);
    }

    /// Publish a wetware environment to IPFS
    async fn push(
        path: PathBuf,
        ipfs_url: String,
        stem: Option<String>,
        _rpc_url: String,
        _private_key: Option<String>,
    ) -> Result<()> {
        // Verify environment is built
        let boot_wasm = path.join("boot/main.wasm");
        if !boot_wasm.exists() {
            bail!(
                "boot/main.wasm not found at: {}\n\
                 \n\
                 Please run 'ww build' first to compile your guest program.",
                boot_wasm.display()
            );
        }

        // Create a compatibility layer: the runtime expects bin/main.wasm.
        // We copy boot/main.wasm to bin/main.wasm for the published image.
        let bin_dir = path.join("bin");
        std::fs::create_dir_all(&bin_dir).context("Failed to create bin directory")?;

        let bin_wasm = bin_dir.join("main.wasm");
        std::fs::copy(&boot_wasm, &bin_wasm).context("Failed to prepare WASM artifact for IPFS")?;

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
        println!("\nTo run this environment:");
        println!("  ww run /ipfs/{}", cid);

        // Optionally update on-chain Atom contract
        if let Some(stem_addr) = stem {
            if _private_key.is_none() {
                bail!("--private-key is required when --stem is provided");
            }

            println!("\nUpdating on-chain Atom contract...");

            let contract = parse_contract_address(&stem_addr)?;

            // Note: Full on-chain update implementation would go here.
            // For now, we'll just acknowledge the request.
            println!("Note: On-chain contract update via CLI is not yet implemented.");
            println!(
                "The CID can be manually updated at contract: 0x{}",
                hex::encode(contract)
            );
        }

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

    /// Refresh WASM images and the daemon service file
    /// to match the current binary. Safe to run repeatedly.
    async fn perform_update() -> Result<()> {
        use indicatif::{ProgressBar, ProgressStyle};
        use std::time::Duration;

        let spin = || {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::default_spinner()
                    .template("  \u{2699} {msg}")
                    .expect("valid template"),
            );
            pb.enable_steady_tick(Duration::from_millis(80));
            pb
        };
        let done = |msg: String| println!("  \u{2713} {msg}");
        let skip = |msg: String| println!("  \u{00b7} {msg}");
        let fail = |msg: String| println!("  \u{2717} {msg}");

        let home = dirs::home_dir().context("Cannot determine home directory")?;
        let ww_dir = home.join(".ww");

        if !ww_dir.exists() {
            bail!("~/.ww does not exist. Run `ww perform install` first.");
        }

        // Ensure subdirectories exist (may be missing if created by older version).
        for sub in &["bin", "etc/ns", "logs"] {
            let dir = ww_dir.join(sub);
            if !dir.exists() {
                std::fs::create_dir_all(&dir)?;
            }
        }

        // ── WASM images (embedded check) ─────────────────────────────
        // Embedded WASM blobs are served by EmbeddedLoader at runtime.
        // We no longer write them to disk; just check if they're present
        // in the binary to decide whether to republish the std namespace.
        // The kernel is named main.wasm (not kernel.wasm) because the
        // host validator requires bin/main.wasm at the merged image root.
        // Other cells keep their canonical names.
        let embedded_cells: &[(&str, &[u8])] = &[
            ("main.wasm", EMBEDDED_KERNEL),
            ("status.wasm", EMBEDDED_STATUS),
        ];
        let images_ok = embedded_cells.iter().all(|(_, bytes)| !bytes.is_empty());
        // Detect changes by hashing embedded blobs against last-published CID.
        let any_images_changed = {
            let cid_marker = ww_dir.join(".last-std-cid");
            let current_hash = {
                let mut hasher = blake3::Hasher::new();
                for (_, bytes) in embedded_cells {
                    hasher.update(bytes);
                }
                hasher.finalize().to_hex().to_string()
            };
            let changed = std::fs::read_to_string(&cid_marker)
                .map(|prev| prev.trim() != current_hash)
                .unwrap_or(true);
            if changed {
                let _ = std::fs::write(&cid_marker, &current_hash);
            }
            changed
        };
        if !images_ok {
            skip("WASM images (not embedded, build from source)".into());
        } else if any_images_changed {
            done("WASM images (updated)".into());
        } else {
            skip("WASM images (unchanged)".into());
        }

        // ── Standard namespace republish (if images changed + Kubo running) ──
        let ipfs_client = ipfs::HttpClient::new("http://localhost:5001".into());
        let kubo_ok = ipfs_client.kubo_info().await.is_ok();

        {
            let ns_path = ww_dir.join("etc/ns/ww");
            let std_cid = ww::namespace::WW_STD_CID;

            let mut config = if ns_path.exists() {
                let content = std::fs::read_to_string(&ns_path)?;
                ww::ns::NamespaceConfig::parse("ww", &content)
            } else {
                ww::ns::NamespaceConfig {
                    name: "ww".to_string(),
                    ipns: String::new(),
                    bootstrap: std_cid.to_string(),
                }
            };

            if kubo_ok && images_ok && any_images_changed {
                let sp = spin();
                sp.set_message("Indexing standard namespace...");

                let tmp = tempfile::TempDir::new()?;
                let tree = tmp.path();
                let bin_dir = tree.join("bin");
                std::fs::create_dir_all(&bin_dir)?;

                // Write WASM cells to bin/ (flat layout).
                for (wasm_name, bytes) in embedded_cells {
                    if !bytes.is_empty() {
                        std::fs::write(bin_dir.join(wasm_name), bytes)?;
                    }
                }

                match ipfs_client.add_dir(tree).await {
                    Ok(cid) => {
                        let ipfs_path = format!("/ipfs/{cid}");
                        config.bootstrap = ipfs_path.clone();
                        let _ = ipfs_client.pin_add(&ipfs_path).await;
                        if !config.ipns.is_empty() {
                            let _ = ipfs_client.name_publish(&ipfs_path, "ww").await;
                        }
                        sp.finish_and_clear();
                        done(format!("Standard namespace ({ipfs_path})"));
                    }
                    Err(e) => {
                        sp.finish_and_clear();
                        fail(format!("Standard namespace ({e})"));
                    }
                }
            } else if !kubo_ok {
                skip("Standard namespace (Kubo not running)".into());
            } else if !any_images_changed {
                skip("Standard namespace (images unchanged)".into());
            }

            config.write_to(&ns_path)?;
            if !config.bootstrap.is_empty() {
                let detail = if !config.ipns.is_empty() {
                    format!("ipns={}", config.ipns)
                } else {
                    format!("bootstrap={}", config.bootstrap)
                };
                done(format!("Namespace ww ({detail})"));
            }
        }

        // ── Daemon config + service file (unconditional) ─────────────
        let identity_path = ww_dir.join("identity");
        // Mount ~/.ww as the single root layer. WASM cells are resolved
        // from the embedded loader or IPNS namespace.
        let image_layers: Vec<String> = vec![ww_dir.display().to_string()];
        Self::daemon_install(Some(identity_path), Vec::new(), image_layers, true).await?;
        done("Background daemon".into());

        // ── Restart daemon (only if images changed) ─────────────────
        if !any_images_changed {
            skip("Daemon restart (nothing changed)".into());
        } else {
            match Self::restart_user_daemon(&home) {
                Some(true) => done("Daemon restarted".into()),
                Some(false) => {
                    if cfg!(target_os = "macos") {
                        let plist_path = home.join("Library/LaunchAgents/io.wetware.ww.plist");
                        fail(format!(
                            "Daemon start (try: launchctl load {})",
                            plist_path.display()
                        ));
                    } else {
                        fail("Daemon restart (try: systemctl --user restart ww)".into());
                    }
                }
                None => skip("Daemon start (no service file)".into()),
            }
        }

        Ok(())
    }

    /// Bootstrap the ~/.ww user layer and daemon.
    ///
    /// If ~/.ww already exists, delegates directly to `perform_update`.
    /// Otherwise performs first-time bootstrap then calls `perform_update`.
    async fn perform_install() -> Result<()> {
        let home = dirs::home_dir().context("Cannot determine home directory")?;
        let ww_dir = home.join(".ww");

        // Already bootstrapped — just refresh.
        if ww_dir.exists() {
            return Self::perform_update().await;
        }

        // ── Cold start: first-time bootstrap ─────────────────────────
        let done = |msg: String| println!("  \u{2713} {msg}");

        // ── Directories ──────────────────────────────────────────────
        let subdirs = ["bin", "etc/ns", "logs"];
        for sub in &subdirs {
            let dir = ww_dir.join(sub);
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("Failed to create {}", dir.display()))?;
        }
        done("Directories".into());

        // ── Symlink binary to ~/.ww/bin/ww ──────────────────────────
        let current_exe =
            std::env::current_exe().context("Cannot determine path of running binary")?;
        let current_exe = current_exe
            .canonicalize()
            .unwrap_or_else(|_| current_exe.clone());
        let symlink_path = ww_dir.join("bin/ww");
        let _ = std::fs::remove_file(&symlink_path);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&current_exe, &symlink_path).with_context(|| {
            format!(
                "symlink {} -> {}",
                symlink_path.display(),
                current_exe.display()
            )
        })?;
        done(format!("Binary symlink ({})", symlink_path.display()));

        // ── Identity ─────────────────────────────────────────────────
        let identity_path = ww_dir.join("identity");
        let sk = ww::keys::generate()?;
        ww::keys::save(&sk, &identity_path)?;
        let kp = ww::keys::to_libp2p(&sk)?;
        let peer_id = kp.public().to_peer_id();
        done(format!("Identity ({peer_id})"));

        // ── IPNS key (first-time only, before update so publish works) ─
        let ipfs_client = ipfs::HttpClient::new("http://localhost:5001".into());
        let kubo_ok = ipfs_client.kubo_info().await.is_ok();

        if kubo_ok {
            use indicatif::{ProgressBar, ProgressStyle};
            use std::time::Duration;

            let spin = || {
                let pb = ProgressBar::new_spinner();
                pb.set_style(
                    ProgressStyle::default_spinner()
                        .template("  \u{2699} {msg}")
                        .expect("valid template"),
                );
                pb.enable_steady_tick(Duration::from_millis(80));
                pb
            };
            let fail = |msg: String| println!("  \u{2717} {msg}");

            let keys = ipfs_client.key_list().await.unwrap_or_default();
            if !keys.iter().any(|k| k == "ww") {
                let sp = spin();
                sp.set_message("Generating IPNS key...");
                match ipfs_client.key_gen("ww").await {
                    Ok(id) => {
                        // Write the key into namespace config so perform_update
                        // can publish to IPNS on the first install.
                        let ns_path = ww_dir.join("etc/ns/ww");
                        let config = ww::ns::NamespaceConfig {
                            name: "ww".to_string(),
                            ipns: id.clone(),
                            bootstrap: ww::namespace::WW_STD_CID.to_string(),
                        };
                        let _ = config.write_to(&ns_path);
                        sp.finish_and_clear();
                        done(format!("IPNS key ({id})"));
                    }
                    Err(e) => {
                        sp.finish_and_clear();
                        fail(format!("IPNS key ({e})"));
                    }
                }
            }
        }

        // ── Update: WASM images, stdlib, daemon ─────────────────────
        Self::perform_update().await?;

        // ── PATH + summary ───────────────────────────────────────────
        let ww_bin_dir = ww_dir.join("bin");
        let in_path = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .any(|p| std::path::Path::new(p) == ww_bin_dir);

        println!();
        println!("\u{2697}\u{fe0f}  Next steps:");
        println!();
        if !in_path {
            let shell = std::env::var("SHELL").unwrap_or_default();
            if shell.ends_with("/fish") {
                println!("  fish_add_path {}", ww_bin_dir.display());
            } else {
                println!("  export PATH=\"{}:$PATH\"", ww_bin_dir.display());
            }
        }
        // The cold-install entry point (scripts/install.sh) probes the
        // status endpoint after this command returns; it owns the
        // "wait for daemon, then point at curl" UX. Here we just print
        // the URL — `ww perform install` may also be invoked manually
        // outside the install script, in which case the user can hit
        // /status whenever the daemon is up.
        println!("  curl http://localhost:2080/status");
        println!();
        println!("  Uninstall:  ww perform uninstall");

        Ok(())
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

    /// Self-update the ww binary via IPNS.
    async fn perform_upgrade(ipfs_url: String) -> Result<()> {
        let ipfs_client = ipfs::HttpClient::new(ipfs_url);

        // 1. Resolve IPNS to get latest release tree.
        eprintln!("Resolving /ipns/releases.wetware.run ...");
        let resolved = ipfs_client
            .name_resolve("/ipns/releases.wetware.run")
            .await
            .context(
                "IPNS resolution failed.\n\
                 \n\
                 Make sure Kubo is running (`ipfs daemon`) and connected to the DHT.\n\
                 If IPNS is slow, try again — DHT resolution can take 10-60s on a fresh node.",
            )?;
        let base = resolved.trim_end_matches('/');

        // 2. Fetch VERSION file (one line: "x.y.z").
        let version_path = format!("{base}/VERSION");
        eprintln!("Checking latest version ...");
        let version_bytes = ipfs_client
            .cat(&version_path)
            .await
            .context("Failed to fetch VERSION from IPFS release")?;
        let remote_version = String::from_utf8(version_bytes)
            .context("VERSION is not valid UTF-8")?
            .trim()
            .to_string();

        // 3. Compare with running version.
        let current_version = env!("CARGO_PKG_VERSION");
        if remote_version == current_version {
            println!("Already up to date ({current_version}).");
            return Ok(());
        }

        eprintln!("Upgrade available: {current_version} -> {remote_version}");

        // 4. Detect platform.
        let os = match std::env::consts::OS {
            "linux" => "linux",
            "macos" => "macos",
            os => bail!("Unsupported OS for upgrade: {os}"),
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "aarch64",
            arch => bail!("Unsupported architecture for upgrade: {arch}"),
        };

        // 5. Fetch binary.
        let binary_path = format!("{base}/bin/ww/{os}/{arch}/ww");
        eprintln!("Fetching {binary_path} ...");
        let binary = ipfs_client
            .cat(&binary_path)
            .await
            .with_context(|| format!("Failed to fetch binary for {os}/{arch}"))?;

        // 6. Fetch and verify checksums.
        let checksums_path = format!("{base}/CHECKSUMS.txt");
        let checksums_bytes = ipfs_client
            .cat(&checksums_path)
            .await
            .context("Failed to fetch CHECKSUMS.txt")?;
        let checksums =
            String::from_utf8(checksums_bytes).context("CHECKSUMS.txt is not valid UTF-8")?;

        Self::verify_checksum(&binary, &format!("bin/ww/{os}/{arch}/ww"), &checksums)
            .context("Checksum verification failed — aborting upgrade")?;

        // 7. Atomic replace of current binary.
        let current_exe =
            std::env::current_exe().context("Cannot determine path of running binary")?;
        let current_exe = current_exe
            .canonicalize()
            .unwrap_or_else(|_| current_exe.clone());
        let old_exe = current_exe.with_extension("old");

        // On Linux, rename the running binary first (rename(2) works on
        // running binaries — the old inode stays alive until the process
        // exits). On macOS this also works fine.
        std::fs::rename(&current_exe, &old_exe).with_context(|| {
            format!(
                "Failed to rename {} to {}.\n\
                 \n\
                 If permission denied, try: sudo ww perform upgrade",
                current_exe.display(),
                old_exe.display()
            )
        })?;

        if let Err(e) = std::fs::write(&current_exe, &binary) {
            // Restore old binary on failure.
            let _ = std::fs::rename(&old_exe, &current_exe);
            return Err(e).context("Failed to write new binary — old binary restored");
        }

        // chmod +x
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&current_exe, perms)
                .context("Failed to set executable permissions on new binary")?;
        }

        // Clean up old binary (best-effort; on Linux the running process
        // still holds the inode, but the directory entry is removed).
        let _ = std::fs::remove_file(&old_exe);

        // Re-symlink ~/.ww/bin/ww to the new binary location.
        if let Some(home) = dirs::home_dir() {
            let symlink_path = home.join(".ww/bin/ww");
            if symlink_path.exists() || symlink_path.symlink_metadata().is_ok() {
                let _ = std::fs::remove_file(&symlink_path);
                #[cfg(unix)]
                let _ = std::os::unix::fs::symlink(&current_exe, &symlink_path);
            }
        }

        println!("Upgraded ww to {remote_version}. Running update...");
        println!();
        Self::perform_update().await
    }

    /// Verify a binary against CHECKSUMS.txt (blake3 or sha256).
    fn verify_checksum(binary: &[u8], filename: &str, checksums: &str) -> Result<()> {
        // CHECKSUMS.txt format (produced by b3sum and sha256sum):
        //   <hash>  <filename>
        // Try blake3 first (above the "# sha256" separator), then sha256.
        for line in checksums.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Split on whitespace: "<hash>  <file>" or "<hash> <file>"
            let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
            if parts.len() != 2 {
                continue;
            }
            let expected_hash = parts[0];
            let file = parts[1].trim();
            if file != filename {
                continue;
            }

            // Determine hash type by length: blake3 = 64 hex chars, sha256 = 64 hex chars.
            // Both are 64 hex chars, so try blake3 first.
            let actual_hash = {
                let mut hasher = blake3::Hasher::new();
                hasher.update(binary);
                hasher.finalize().to_hex().to_string()
            };

            if actual_hash == expected_hash {
                eprintln!("Checksum OK (blake3)");
                return Ok(());
            }

            // Try sha256.
            // sha256 is not in deps; blake3 match above should cover it.
            // If blake3 didn't match, report mismatch.
            bail!(
                "Checksum mismatch for {filename}:\n  expected: {expected_hash}\n  got:      {actual_hash}\n\
                 \n\
                 The downloaded binary may be corrupted. Aborting upgrade."
            );
        }

        bail!(
            "No checksum found for {filename} in CHECKSUMS.txt.\n\
             Cannot verify integrity — aborting upgrade."
        )
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_validate_backend_mount_policy_accepts_root_mounts() {
        let mounts =
            ww::cell::mount::parse_args(&[".".to_string(), "/ipfs/bafybeigdyrzt".to_string()])
                .unwrap();
        Commands::validate_backend_mount_policy(&mounts).unwrap();
    }

    #[test]
    fn test_validate_backend_mount_policy_rejects_targeted_mounts() {
        let mounts = ww::cell::mount::parse_args(&[
            ".".to_string(),
            "~/.ww/identity:/etc/identity".to_string(),
        ])
        .unwrap();
        let err = Commands::validate_backend_mount_policy(&mounts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("targeted mounts are not supported in backend virtual mode"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("/etc/identity"),
            "error should include offending target path: {msg}"
        );
    }

    #[test]
    fn test_run_command_rejects_targeted_mounts_preflight() {
        let cmd = Commands::Run {
            mounts: vec![".".to_string(), "~/.ww/identity:/etc/identity".to_string()],
            listen: Vec::new(),
            wasm_debug: false,
            kernel: None,
            identity: None,
            insecure_ephemeral: false,
            stem: None,
            rpc_url: "http://127.0.0.1:8545".to_string(),
            ws_url: "ws://127.0.0.1:8545".to_string(),
            confirmation_depth: 6,
            epoch_drain_secs: 1,
            executor_threads: 0,
            http_listen: None,
            http_dial: Vec::new(),
            runtime_cache_policy: "shared".to_string(),
            with_http_admin: "off".to_string(),
            ipfs_url: "http://localhost:5001".to_string(),
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(cmd.run()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("targeted mounts are not supported in backend virtual mode"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("/etc/identity"),
            "error should include offending target path: {msg}"
        );
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
    fn run_admin_off_disables_case_insensitively() {
        for value in ["off", "OFF", " Off "] {
            let configured = parse_run_admin_addr(&["ww", "run", "--with-http-admin", value]);
            assert_eq!(admin_listen_addr(configured), None, "{value}");
        }
    }

    #[test]
    fn run_cli_accepts_explicit_kernel_source() {
        let command = Cli::try_parse_from([
            "ww",
            "run",
            "std/status",
            "--kernel",
            "cid:bafkr4if3s6yv23hd3hgfvftj2g2uwdrqazv53p36p5lqyy7n77d5t5p54a",
        ])
        .expect("parse --kernel")
        .command;

        match command {
            Commands::Run { mounts, kernel, .. } => {
                assert_eq!(mounts, vec!["std/status"]);
                assert_eq!(
                    kernel.as_deref(),
                    Some("cid:bafkr4if3s6yv23hd3hgfvftj2g2uwdrqazv53p36p5lqyy7n77d5t5p54a")
                );
            }
            _ => panic!("expected run command"),
        }
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

    #[test]
    fn test_embedded_loader_does_not_shadow_with_empty_kernel_blob() {
        use ww::cell::Loader;

        let loader = embedded_loader();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(loader.load("/ipfs/example/bin/main.wasm"));

        if EMBEDDED_KERNEL.is_empty() {
            assert!(
                result.is_err(),
                "embedded loader should not resolve bin/main.wasm when kernel blob is absent"
            );
        } else {
            let bytes = result.expect("expected embedded kernel bytes");
            assert!(
                !bytes.is_empty(),
                "embedded kernel resolution returned empty bytes"
            );
        }
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
            images: Vec::new(),
            http_listen: Some(addr.to_string()),
        }
    }

    fn config_no_http_listen() -> daemon_cmd::DaemonServiceConfig {
        daemon_cmd::DaemonServiceConfig {
            listen: vec!["/ip4/0.0.0.0/tcp/2025".to_string()],
            identity: PathBuf::from("/tmp/identity"),
            images: Vec::new(),
            http_listen: None,
        }
    }

    #[test]
    fn test_launchd_plist_emits_http_listen_when_set() {
        let home = tempfile::TempDir::new().unwrap();
        let config = config_with_http_listen("127.0.0.1:2080");
        let ww_bin = std::path::Path::new("/usr/local/bin/ww");

        daemon_cmd::write_launchd_plist(ww_bin, &config, home.path(), "/tmp/identity", true)
            .expect("write plist should succeed");

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

        daemon_cmd::write_systemd_unit(ww_bin, &config, home.path(), "/tmp/identity", true)
            .expect("write unit should succeed");

        let unit = std::fs::read_to_string(home.path().join(".config/systemd/user/ww.service"))
            .expect("unit should exist");

        assert!(
            unit.contains("--http-listen 127.0.0.1:2080"),
            "systemd unit should emit --http-listen flag with addr, got:\n{unit}"
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
}
