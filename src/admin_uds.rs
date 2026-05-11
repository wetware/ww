//! AdminUdsService — local admin gate over a Unix Domain Socket.
//!
//! Exposes the daemon's full membrane unattenuated on
//! `~/.ww/run/<peer-id>.sock`. Whoever can write to that directory has
//! full admin access — by design, matching the local-socket convention
//! of `/var/run/docker.sock`, `~/.ipfs/api`, `~/.podman/podman.sock`.
//!
//! Architecture: peer of [`SwarmService`] and [`EpochService`] — its own
//! thread with a `current_thread` runtime + `LocalSet`. Constructs its
//! own `Runtime` client (sharing the supervisor's backing state),
//! pre-loads `shell.wasm`, and binds the UDS at startup. Per-connection:
//! spawn a fresh shell cell instance and bridge the `UnixStream` to it
//! via the existing `handle_vat_connection_spawn` (generic over
//! `AsyncRead + AsyncWrite + 'static`).
//!
//! [`SwarmService`]: crate::services::SwarmService
//! [`EpochService`]: crate::services::EpochService
#![cfg(not(target_arch = "wasm32"))]

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio_util::compat::TokioAsyncReadCompatExt;

use ::membrane::{Epoch, EpochGuard};
use ed25519_dalek::SigningKey;

use crate::discovery;
use crate::host::SwarmCommand;
use crate::launcher::create_runtime_client;
use crate::services::Service;
use crate::system_capnp;
use rpc::{
    routing::RoutingImpl, vat_listener::handle_vat_connection_spawn, CachePolicy, HostImpl,
    NetworkState,
};

/// Configuration + shared state for the admin UDS endpoint.
///
/// Constructed in the supervisor (`src/cli/main.rs::run_command`) once
/// the daemon's shared state is in place (after `SwarmService` reports
/// ready). Owns clones of the backing state it needs; everything is
/// `Send + Clone` at the boundary so the service can be moved onto its
/// own thread.
pub struct AdminUdsService {
    /// Daemon's libp2p peer ID, used to compute the socket and metadata paths.
    pub peer_id: String,
    /// Bytes of `shell.wasm`, loaded into the per-thread `Runtime` at startup.
    pub shell_wasm: Vec<u8>,
    /// Listen multiaddrs (transport-only — no `/p2p/` suffix), written into
    /// the metadata file for tooling consumers.
    pub multiaddrs: Vec<String>,
    /// `ww` binary version string, written into the metadata file.
    pub version: String,
    pub network_state: NetworkState,
    pub swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    pub wasm_debug: bool,
    pub signing_key: Option<Arc<SigningKey>>,
    pub stream_control: libp2p_stream::Control,
    pub ipfs_client: ipfs::HttpClient,
    pub http_dial: Vec<String>,
    pub cache_policy: CachePolicy,
}

impl Service for AdminUdsService {
    fn run(self, shutdown: watch::Receiver<()>) -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build admin-uds runtime")?;
        let _span = tracing::info_span!("admin-uds").entered();

        rt.block_on(async move {
            let local = tokio::task::LocalSet::new();
            local.run_until(self.serve(shutdown)).await
        })
    }
}

impl AdminUdsService {
    async fn serve(self, mut shutdown: watch::Receiver<()>) -> Result<()> {
        // ── 1. Sentinel epoch guard for admin-scope caps. ─────────────────
        //
        // Admin does not enforce epoch-based capability expiry; the shell
        // remains usable for the daemon's lifetime regardless of stem epoch
        // advancement. We construct a guard whose `check()` never fires by
        // pointing it at a never-changing watch channel.
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            provenance: ::membrane::Provenance::Block(0),
        });
        // Hold the sender across the lifetime of this service so the
        // receiver remains valid.
        let _keep_alive = epoch_tx;
        let guard = EpochGuard {
            issued_seq: 0,
            receiver: epoch_rx.clone(),
        };

        // ── 2. Build the `Runtime` client on this thread. ────────────────
        //
        // The `Runtime` capability is `!Send` and lives on this single-
        // threaded runtime. Internally it shares backing state with the
        // rest of the daemon via the cloned channels and handles passed in.
        //
        // The epoch_rx is load-bearing: `src/launcher.rs:359` selects
        // between `build_membrane_rpc` (which exports the cell's
        // bootstrap capability through the WASI duplex back to the host)
        // and `build_peer_rpc` (which does NOT) based on whether epoch_rx
        // is `Some`. We need bootstrap export for `handle_vat_connection_spawn`
        // to retrieve the cell's bootstrap cap, so we pass our sentinel
        // receiver. The receiver never sees an epoch advance — admin scope
        // is exempt from epoch-based capability expiry by design.
        let runtime = create_runtime_client(
            self.network_state.clone(),
            self.swarm_cmd_tx.clone(),
            self.wasm_debug,
            None, // no epoch guard on the RuntimeImpl itself
            Some(epoch_rx.clone()),
            self.signing_key.clone(),
            Some(self.stream_control.clone()),
            self.cache_policy,
            self.ipfs_client.clone(),
            self.http_dial.clone(),
        );

        // ── 3. Pre-load shell.wasm. ──────────────────────────────────────
        //
        // The `Runtime` caches compiled `Executor` clients by content hash;
        // subsequent loads of the same bytes return the cached executor
        // immediately. Loading once at startup means per-connection spawn
        // skips the wasmtime compile entirely.
        let executor: system_capnp::executor::Client = {
            let mut req = runtime.load_request();
            req.get().set_wasm(&self.shell_wasm);
            let resp = req
                .send()
                .promise
                .await
                .context("runtime.load(shell.wasm) failed")?;
            resp.get()?.get_executor()?
        };
        tracing::info!(
            bytes = self.shell_wasm.len(),
            "shell.wasm loaded into admin runtime"
        );

        // ── 4. Assemble the full caps list. ──────────────────────────────
        //
        // Mirrors `HostGraftBuilder` (in `crates/rpc/src/graft.rs`) but as
        // a flat `Vec<(name, client, schema_bytes)>` for direct hand-off
        // to `handle_vat_connection_spawn` on every connection.
        let caps = build_full_caps(
            &self.network_state,
            &self.swarm_cmd_tx,
            self.wasm_debug,
            &guard,
            &self.stream_control,
            &runtime,
            &self.ipfs_client,
        );

        // ── 5. Bind the UDS with stale-socket recovery. ──────────────────
        let socket_path = discovery::socket_path(&self.peer_id);
        let listener = bind_with_recovery(&socket_path)
            .await
            .with_context(|| format!("bind admin UDS at {socket_path:?}"))?;
        tracing::info!(?socket_path, "admin UDS bound");

        // ── 6. Write the metadata file for tooling. ──────────────────────
        let metadata_path = discovery::metadata_path(&self.peer_id);
        if let Err(e) = write_metadata(
            &metadata_path,
            &self.peer_id,
            &self.multiaddrs,
            &self.version,
        ) {
            // Non-fatal: tooling-only artifact.
            tracing::warn!(?metadata_path, error = %e, "failed to write admin metadata");
        }

        // ── 7. Accept loop. ──────────────────────────────────────────────
        let result = loop {
            tokio::select! {
                accept = listener.accept() => match accept {
                    Ok((stream, _peer_addr)) => {
                        let exec = executor.clone();
                        let caps = caps.clone();
                        tokio::task::spawn_local(async move {
                            // `UnixStream: tokio::io::AsyncRead + AsyncWrite`.
                            // `.compat()` adapts it into the
                            // `futures::io::AsyncRead + AsyncWrite` traits
                            // that `VatNetwork` expects internally.
                            let stream = stream.compat();
                            if let Err(e) = handle_vat_connection_spawn(
                                exec,
                                caps,
                                stream,
                                "local",
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "admin UDS connection ended with error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "admin UDS accept error (continuing)");
                    }
                },
                _ = shutdown.changed() => {
                    tracing::info!("admin-uds shutting down");
                    break Ok::<(), anyhow::Error>(());
                }
            }
        };

        // ── 8. Cleanup. ──────────────────────────────────────────────────
        //
        // Remove both files on graceful shutdown. SIGKILL is handled by
        // the next start's stale-socket recovery (see `bind_with_recovery`).
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&metadata_path);

        result
    }
}

/// Assemble the full membrane cap collection for an admin-scope shell cell.
///
/// Mirrors `HostGraftBuilder` (in `crates/rpc/src/graft.rs`) but returns the
/// flat `Vec<(name, client, schema_bytes)>` shape that
/// `handle_vat_connection_spawn` expects for forwarding into the spawned
/// cell's graft response.
///
/// Includes: `host`, `runtime`, `routing`. Admin scope = full set.
/// (Today's shell cell uses only `host` and `routing` from graft; we also
/// expose `runtime` for future-proofing and to mirror the canonical graft
/// shape.)
#[allow(clippy::too_many_arguments)]
fn build_full_caps(
    network_state: &NetworkState,
    swarm_cmd_tx: &mpsc::Sender<SwarmCommand>,
    wasm_debug: bool,
    guard: &EpochGuard,
    stream_control: &libp2p_stream::Control,
    runtime: &system_capnp::runtime::Client,
    ipfs_client: &ipfs::HttpClient,
) -> Vec<(String, capnp::capability::Client, Vec<u8>)> {
    let mut caps: Vec<(String, capnp::capability::Client, Vec<u8>)> = Vec::new();

    // host
    let host_impl = HostImpl::new(
        network_state.clone(),
        swarm_cmd_tx.clone(),
        wasm_debug,
        Some(guard.clone()),
        Some(stream_control.clone()),
    );
    let host: system_capnp::host::Client = capnp_rpc::new_client(host_impl);
    caps.push(("host".to_string(), host.client, schema_for("host")));

    // runtime (clone the singleton client)
    caps.push((
        "runtime".to_string(),
        runtime.clone().client,
        schema_for("runtime"),
    ));

    // routing
    let routing_impl = RoutingImpl::new(swarm_cmd_tx.clone(), guard.clone(), ipfs_client.clone());
    let routing: ::membrane::routing_capnp::routing::Client = capnp_rpc::new_client(routing_impl);
    caps.push(("routing".to_string(), routing.client, schema_for("routing")));

    caps
}

/// Look up canonical Schema.Node bytes for a core capability name.
///
/// Core caps (`host`, `runtime`, `routing`, etc.) have their schemas baked
/// into the binary at build time by `crates/membrane/build.rs`. Unknown
/// names return an empty Vec; the graft path tolerates that with a warning.
fn schema_for(name: &str) -> Vec<u8> {
    ::membrane::schema_registry::schema_by_name(name)
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default()
}

/// Bind a Unix Domain Socket with stale-socket recovery.
///
/// On Linux and macOS, UDS pathnames persist on the filesystem after the
/// listener closes — the kernel does *not* unlink them. If a previous
/// daemon was SIGKILLed, a stale socket file blocks `bind()` with
/// `EADDRINUSE`. The recovery protocol:
///
/// 1. Attempt `bind()` directly.
/// 2. On `EADDRINUSE`, probe the existing socket by attempting `connect()`
///    with a 1-second tokio timeout.
/// 3. If the probe succeeds, another daemon is listening — bail out.
/// 4. If the probe fails (typically `ECONNREFUSED`), the file is stale —
///    `unlink` and retry `bind`.
async fn bind_with_recovery(socket_path: &PathBuf) -> std::io::Result<tokio::net::UnixListener> {
    use std::time::Duration;

    // Ensure parent directory exists. Best-effort; the caller already
    // selected a writable run dir.
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match tokio::net::UnixListener::bind(socket_path) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Stale-socket recovery.
            let probe = tokio::time::timeout(
                Duration::from_secs(1),
                tokio::net::UnixStream::connect(socket_path),
            )
            .await;
            match probe {
                Ok(Ok(_)) => Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("another daemon is already listening on {socket_path:?}"),
                )),
                _ => {
                    // Either timeout (Err on the outer Result) or connect
                    // refused (Ok(Err(...))): treat as stale.
                    tracing::warn!(
                        ?socket_path,
                        "stale UDS detected (connect probe failed); unlinking and rebinding"
                    );
                    std::fs::remove_file(socket_path)?;
                    tokio::net::UnixListener::bind(socket_path)
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// Write the admin metadata JSON to disk.
///
/// Schema (consumed by `ww status`, MCP tooling, and `ww shell` discovery):
/// ```json
/// {
///   "peer_id":    "12D3KooW...",          // bs58
///   "multiaddrs": ["/ip4/127.0.0.1/tcp/2025", "..."],
///   "started_at": "2026-05-11T17:30:00Z", // RFC 3339 UTC
///   "pid":        12345,                  // OS process ID
///   "version":    "0.1.0"                 // ww binary version
/// }
/// ```
///
/// All fields required. Consumers should treat a missing or malformed
/// file as "no metadata available" and fall back to `.sock` connect-probe
/// for liveness.
fn write_metadata(
    path: &PathBuf,
    peer_id: &str,
    multiaddrs: &[String],
    version: &str,
) -> Result<()> {
    let pid = std::process::id();
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let json = serde_json::json!({
        "peer_id": peer_id,
        "multiaddrs": multiaddrs,
        "started_at": started_at,
        "pid": pid,
        "version": version,
    });
    let body = serde_json::to_vec_pretty(&json).context("serialize metadata json")?;
    std::fs::write(path, body).with_context(|| format!("write {path:?}"))?;
    Ok(())
}
