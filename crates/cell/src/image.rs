//! Mount-based FHS image resolution (CidTree path).
//!
//! Every positional arg to `ww run` is a mount: `source[:target]`.
//! Root mounts (target `/`) are traditional image layers. Targeted
//! mounts are currently rejected in backend virtual mode.
//!
//! Mounts are applied left-to-right via `resolve_mounts_virtual`:
//! root layers are DAG-merged at the IPFS MFS level (file blocks never
//! touched, only directory nodes get new CIDs). No file content is
//! materialized to disk by this module.
//!
//! Pre-#416 this file also exposed an `apply_mounts` API that
//! materialized a merged FHS into a `TempDir` and was preopened
//! directly to the WASI guest. That path was removed once every
//! production cell switched to `CidTree`. The merge algorithm itself
//! (`dag_merge` + `merge_overlay_recursive`) is preserved here and
//! used by `resolve_mounts_virtual`.

use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use cid::Cid;

use crate::mount::Mount;
use ipfs;

// ── DAG merge via IPFS MFS ─────────────────────────────────────────

// Merge workspaces live below a versioned, private-to-ww MFS root. Older
// `/ww-merge-*` paths deliberately remain outside the sweeper's authority:
// their origin and liveness cannot be established safely.
const MFS_MERGE_ROOT: &str = "/wetware-ww/merge-v1";
const MFS_MERGE_WORKSPACE: &str = "root";
const MFS_OWNER_MARKER_PREFIX: &str = ".wetware-ww-owner-v1-";
const MFS_REAPABLE_MARKER_PREFIX: &str = ".wetware-ww-reapable-v1-";
// A DAG merge is bounded by the boot operation watchdog. Keep namespaces for
// much longer than a normal boot so a temporarily unhealthy Kubo cannot cause
// a later boot to remove a live merge namespace.
const MFS_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const MFS_SWEEP_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
const MFS_CLEANUP_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

fn mfs_namespace_name(created_at_secs: u64, token: u128) -> String {
    format!("{created_at_secs:016x}-{token:032x}")
}

fn mfs_namespace_parts(name: &str) -> Option<(u64, &str)> {
    let (created_at, token) = name.split_once('-')?;
    if created_at.len() != 16
        || token.len() != 32
        || !created_at.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    u64::from_str_radix(created_at, 16)
        .ok()
        .map(|created_at| (created_at, token))
}

fn mfs_owner_marker(token: &str) -> String {
    format!("{MFS_OWNER_MARKER_PREFIX}{token}")
}

fn mfs_reapable_marker(token: &str) -> String {
    format!("{MFS_REAPABLE_MARKER_PREFIX}{token}")
}

fn mfs_entry_is_directory(entries: &[ipfs::MfsEntry], name: &str) -> bool {
    entries
        .iter()
        .any(|entry| entry.entry_type == 1 && entry.name == name)
}

/// Remove abandoned merge namespaces created by this version of `ww`.
///
/// A namespace is eligible only when it is below the versioned ww root, has
/// both an unguessable owner marker and a reapable marker written after its
/// merge attempt ended, and has aged for a full day. An active workspace has
/// no reapable marker, so it is never swept even if its clock timestamp is
/// old or skewed. The sweep is best-effort and must never block boot.
pub async fn sweep_stale_mfs_namespaces(client: &ipfs::BootClient) -> Result<usize> {
    // One deadline bounds the *entire* background pass. Giving every entry a
    // fresh timeout would let a large collection of abandoned namespaces hold
    // this task indefinitely.
    let deadline = tokio::time::Instant::now() + MFS_SWEEP_OPERATION_TIMEOUT;
    let entries = tokio::time::timeout_at(deadline, client.client().mfs().files_ls(MFS_MERGE_ROOT))
        .await
        .map_err(|_| anyhow::anyhow!("timed out listing MFS merge root for stale namespaces"))??;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs();
    let mut removed = 0;

    for entry in entries {
        if entry.entry_type != 1 {
            continue;
        }
        let Some((created_at, token)) = mfs_namespace_parts(&entry.name) else {
            continue;
        };
        if now.saturating_sub(created_at) < MFS_STALE_AFTER.as_secs() {
            continue;
        }

        let path = format!("{MFS_MERGE_ROOT}/{}", entry.name);
        // A name alone is never enough to establish ownership. Refuse to
        // remove a workspace unless its paired, random owner and reapable
        // markers are both present as directories.
        let children = match tokio::time::timeout_at(
            deadline,
            client.client().mfs().files_ls(&path),
        )
        .await
        {
            Ok(Ok(children)) => children,
            Ok(Err(error)) => {
                tracing::warn!(%path, "Failed to inspect MFS merge namespace before sweeping: {error}");
                continue;
            }
            Err(_) => {
                tracing::warn!(%path, "Timed out inspecting MFS merge namespace before sweeping");
                break;
            }
        };
        if !mfs_entry_is_directory(&children, &mfs_owner_marker(token))
            || !mfs_entry_is_directory(&children, &mfs_reapable_marker(token))
        {
            continue;
        }

        match tokio::time::timeout_at(deadline, client.client().mfs().files_rm(&path, true)).await {
            Ok(Ok(())) => {
                removed += 1;
                tracing::info!(%path, "Removed stale MFS merge namespace");
            }
            Ok(Err(error)) => {
                tracing::warn!(%path, "Failed to remove stale MFS merge namespace: {error}");
            }
            Err(_) => {
                tracing::warn!(%path, "Timed out removing stale MFS merge namespace");
            }
        }
    }

    Ok(removed)
}

/// RAII guard that cleans up an MFS namespace even when a boot watchdog
/// cancels an in-progress DAG merge.
struct MfsNamespaceGuard {
    boot_client: ipfs::BootClient,
    client: ipfs::HttpClient,
    namespace_path: String,
    workspace_path: String,
    owner_marker_path: String,
    reapable_marker_path: String,
    owned: bool,
    cleaned: bool,
}

async fn await_or_cancel<T, F>(
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    operation: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        result = operation => result,
        changed = cancel.changed() => match changed {
            Ok(()) if *cancel.borrow() => Err(anyhow::anyhow!("mount resolution cancelled")),
            Ok(()) => Err(anyhow::anyhow!("mount resolution cancellation channel changed unexpectedly")),
            Err(_) => Err(anyhow::anyhow!("mount resolution cancellation channel closed")),
        }
    }
}

impl MfsNamespaceGuard {
    fn new(client: &ipfs::BootClient) -> Self {
        let token: u128 = rand::random();
        let created_at_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let namespace_path = format!(
            "{MFS_MERGE_ROOT}/{}",
            mfs_namespace_name(created_at_secs, token)
        );
        let token = format!("{token:032x}");
        Self {
            boot_client: client.clone(),
            client: client.client().clone(),
            workspace_path: format!("{namespace_path}/{MFS_MERGE_WORKSPACE}"),
            owner_marker_path: format!("{namespace_path}/{}", mfs_owner_marker(&token)),
            reapable_marker_path: format!("{namespace_path}/{}", mfs_reapable_marker(&token)),
            namespace_path,
            owned: false,
            cleaned: false,
        }
    }

    fn path(&self) -> &str {
        &self.workspace_path
    }

    async fn prepare(&mut self) -> Result<()> {
        self.boot_client
            .mfs()
            .files_mkdir(&self.namespace_path, true)
            .await
            .context("creating MFS merge namespace")?;
        self.boot_client
            .mfs()
            .files_mkdir(&self.owner_marker_path, false)
            .await
            .context("marking MFS merge namespace ownership")?;
        self.owned = true;
        Ok(())
    }

    async fn mark_reapable(&self, deadline: tokio::time::Instant) {
        if !self.owned {
            return;
        }
        match tokio::time::timeout_at(
            deadline,
            self.client
                .mfs()
                .files_mkdir(&self.reapable_marker_path, false),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(path = %self.namespace_path, "Failed to mark inactive MFS merge namespace reapable: {error}")
            }
            Err(_) => {
                tracing::warn!(path = %self.namespace_path, "Timed out marking inactive MFS merge namespace reapable")
            }
        }
    }

    async fn cleanup(mut self, reapable: bool) {
        // A timed-out or cancelled Kubo request can still be running on the
        // daemon after its client future is dropped. Such a namespace may be
        // cleaned directly here, but must never become sweep-eligible later.
        // Only a fully completed merge is known not to be active.
        let deadline = tokio::time::Instant::now() + MFS_CLEANUP_OPERATION_TIMEOUT;
        if reapable {
            self.mark_reapable(deadline).await;
        }
        match tokio::time::timeout_at(
            deadline,
            self.client.mfs().files_rm(&self.namespace_path, true),
        )
        .await
        {
            Ok(Ok(())) => self.cleaned = true,
            Ok(Err(error)) => {
                // A terminal Kubo response will not become useful on a second
                // identical remove. A transient failure, however, needs the
                // Drop-time best-effort cleanup to avoid leaking the namespace.
                self.cleaned = !ipfs::is_retryable_kubo_error(&error);
                tracing::warn!(path = %self.namespace_path, "MFS cleanup failed: {error}");
            }
            // The server may have completed a timed-out remove. Do not replay
            // this non-idempotent operation from Drop.
            Err(_) => {
                self.cleaned = true;
                tracing::warn!(path = %self.namespace_path, "MFS cleanup timed out");
            }
        }
    }
}

impl Drop for MfsNamespaceGuard {
    fn drop(&mut self) {
        if self.cleaned || !self.owned {
            return;
        }
        let client = self.client.clone();
        let namespace_path = self.namespace_path.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let result = tokio::time::timeout(
                    MFS_CLEANUP_OPERATION_TIMEOUT,
                    client.mfs().files_rm(&namespace_path, true),
                )
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(path = %namespace_path, "MFS cancellation cleanup failed: {error}")
                    }
                    Err(_) => tracing::warn!(path = %namespace_path, "MFS cancellation cleanup timed out"),
                }
            });
        } else {
            tracing::warn!(path = %self.namespace_path, "MFS namespace dropped without a Tokio runtime; manual MFS cleanup may be required");
        }
    }
}

/// Merge multiple root layer CIDs using IPFS MFS operations.
///
/// Layers are applied left-to-right. Later layers win on file conflicts.
/// Directories are merged recursively. Returns the root CID of the merged tree.
async fn dag_merge(
    cids: &[String],
    client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<String> {
    if cids.is_empty() {
        bail!("No CIDs to merge");
    }
    if cids.len() == 1 {
        return Ok(cids[0].clone());
    }

    let started = Instant::now();
    let deadline = client
        .retry_max_duration()
        .map(|duration| started + duration);
    let mut backoff = Duration::from_millis(500);
    let mut attempt = 0u64;

    loop {
        attempt += 1;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            bail!(
                "Kubo boot operation MFS DAG merge did not complete after {}s",
                started.elapsed().as_secs()
            );
        }

        // Each retry receives a new namespace. This is essential because a
        // timed-out copy or remove may have completed inside Kubo, making a
        // replay against the previous path non-idempotent.
        let mut guard = MfsNamespaceGuard::new(client);
        let result = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(remaining, async {
                    guard.prepare().await?;
                    dag_merge_attempt(cids, client, cancel, guard.path()).await
                })
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(anyhow::anyhow!(
                        "Kubo boot operation MFS DAG merge did not complete after {}s",
                        started.elapsed().as_secs()
                    )),
                }
            }
            None => {
                async {
                    guard.prepare().await?;
                    dag_merge_attempt(cids, client, cancel, guard.path()).await
                }
                .await
            }
        };
        let reapable = result.is_ok();
        guard.cleanup(reapable).await;

        match result {
            Ok(root) => return Ok(root),
            Err(error) if ipfs::is_retryable_kubo_error(&error) => {
                tracing::warn!(
                    operation = "MFS DAG merge",
                    attempt,
                    error = %error,
                    "Kubo boot operation will retry with a fresh MFS namespace"
                );
                client.report_retry("MFS DAG merge", attempt);

                let delay = deadline
                    .map(|deadline| backoff.min(deadline.saturating_duration_since(Instant::now())))
                    .unwrap_or(backoff);
                if delay.is_zero() {
                    bail!(
                        "Kubo boot operation MFS DAG merge did not complete after {}s",
                        started.elapsed().as_secs()
                    );
                }
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    changed = cancel.changed() => return match changed {
                        Ok(()) if *cancel.borrow() => Err(anyhow::anyhow!("mount resolution cancelled")),
                        Ok(()) => Err(anyhow::anyhow!("mount resolution cancellation channel changed unexpectedly")),
                        Err(_) => Err(anyhow::anyhow!("mount resolution cancellation channel closed")),
                    },
                }
                backoff = (backoff * 2).min(Duration::from_secs(15));
            }
            Err(error) => return Err(error),
        }
    }
}

async fn dag_merge_attempt(
    cids: &[String],
    client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    mfs_path: &str,
) -> Result<String> {
    if *cancel.borrow() {
        bail!("mount resolution cancelled");
    }

    let merge = async {
        // Copy the base layer (O(1) DAG link).
        client
            .mfs()
            .files_cp(&format!("/ipfs/{}", cids[0]), mfs_path)
            .await
            .context("Failed to copy base layer to MFS")?;

        // Overlay each subsequent layer.
        for cid in &cids[1..] {
            merge_overlay_recursive(client, mfs_path, &format!("/ipfs/{cid}"))
                .await
                .with_context(|| format!("Failed to merge overlay {cid}"))?;
        }

        // Stat to get merged root CID.
        let stat = client
            .mfs()
            .files_stat(mfs_path, true)
            .await
            .context("Failed to stat merged MFS namespace")?;
        Ok(stat.hash)
    };
    await_or_cancel(cancel, merge).await
}

/// Recursively merge an overlay into the MFS namespace.
///
/// For each entry in the overlay:
/// - Not in base → `files cp` (add)
/// - Both directories → recurse
/// - Any conflict → `files rm` + `files cp` (replace)
fn merge_overlay_recursive<'a>(
    client: &'a ipfs::BootClient,
    mfs_path: &'a str,
    overlay_path: &'a str,
) -> futures::future::BoxFuture<'a, Result<()>> {
    Box::pin(merge_overlay_recursive_inner(
        client,
        mfs_path,
        overlay_path,
    ))
}

async fn merge_overlay_recursive_inner(
    client: &ipfs::BootClient,
    mfs_path: &str,
    overlay_path: &str,
) -> Result<()> {
    let mfs = client.mfs();

    // List overlay entries via the regular ls API.
    let overlay_entries = client
        .ls(overlay_path)
        .await
        .with_context(|| format!("ls overlay {overlay_path}"))?;

    // Recursion only descends into directories already copied into MFS, so a
    // listing error is an operation failure rather than evidence of an empty dir.
    let mfs_entries = mfs.files_ls(mfs_path).await?;
    let mfs_names: HashSet<&str> = mfs_entries.iter().map(|e| e.name.as_str()).collect();

    for entry in &overlay_entries {
        let child_mfs = format!("{}/{}", mfs_path, entry.name);
        let child_overlay = format!("{}/{}", overlay_path, entry.name);
        let is_overlay_dir = entry.entry_type == 1;

        if mfs_names.contains(entry.name.as_str()) {
            // Entry exists in base. Check if both are directories.
            let existing = mfs_entries
                .iter()
                .find(|e| e.name == entry.name)
                .context("entry in mfs_names but not in mfs_entries")?;
            let is_existing_dir = existing.entry_type == 1;

            if is_overlay_dir && is_existing_dir {
                // Both dirs → recurse.
                merge_overlay_recursive(client, &child_mfs, &child_overlay).await?;
            } else {
                // Conflict: replace.
                mfs.files_rm(&child_mfs, true)
                    .await
                    .with_context(|| format!("rm {child_mfs}"))?;
                mfs.files_cp(&format!("/ipfs/{}", entry.hash), &child_mfs)
                    .await
                    .with_context(|| format!("cp overlay entry {}", entry.name))?;
            }
        } else {
            // New entry → cp.
            mfs.files_cp(&format!("/ipfs/{}", entry.hash), &child_mfs)
                .await
                .with_context(|| format!("cp new entry {}", entry.name))?;
        }
    }

    Ok(())
}

// ── Virtual mount resolution (lazy CidTree path) ─────────────────

/// Resolve mounts into a root CID and local overrides for the virtual filesystem.
///
/// Performs the DAG merge to produce a merged root CID.
/// Targeted mounts are rejected in backend mode to avoid a second,
/// host-local filesystem path.
///
/// Returns `(root_cid, local_overrides)` suitable for constructing a `CidTree`.
pub async fn resolve_mounts_virtual(
    mounts: &[Mount],
    ipfs_client: &ipfs::BootClient,
) -> Result<(
    String,
    std::collections::HashMap<std::path::PathBuf, crate::vfs::LocalOverride>,
)> {
    let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    resolve_mounts_virtual_with_cancel(mounts, ipfs_client, &mut cancel).await
}

/// Validate local mount configuration before waiting for Kubo.
pub fn validate_mounts_virtual(mounts: &[Mount]) -> Result<(Vec<&Mount>, Vec<&Mount>)> {
    if mounts.is_empty() {
        bail!("No mounts provided");
    }

    let (root_mounts, targeted_mounts): (Vec<&Mount>, Vec<&Mount>) =
        mounts.iter().partition(|m| m.is_root());

    if root_mounts.is_empty() {
        bail!("No root mounts provided (at least one required)");
    }

    if !targeted_mounts.is_empty() {
        bail!(
            "targeted mounts are not supported in backend virtual mode (received {} targeted mount(s)); \
             publish content to IPFS/IPNS and mount as a root layer",
            targeted_mounts.len()
        );
    }

    for mount in &root_mounts {
        if !ipfs::is_ipfs_path(&mount.source) && !Path::new(&mount.source).is_dir() {
            bail!(
                "local root mount must be an existing directory: {}",
                mount.source
            );
        }
    }

    Ok((root_mounts, targeted_mounts))
}

/// Cancellable variant used during host boot so service failure waits for MFS
/// cleanup rather than abandoning a detached task.
pub async fn resolve_mounts_virtual_with_cancel(
    mounts: &[Mount],
    ipfs_client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(
    String,
    std::collections::HashMap<std::path::PathBuf, crate::vfs::LocalOverride>,
)> {
    let (root_mounts, _) = validate_mounts_virtual(mounts)?;

    // Resolve all root mounts to CIDs.
    let mut cids = Vec::with_capacity(root_mounts.len());
    for mount in &root_mounts {
        if ipfs::is_ipfs_path(&mount.source) {
            let ipfs_path = if mount.source.starts_with("/ipns/") {
                await_or_cancel(cancel, resolve_ipns_to_ipfs(&mount.source, ipfs_client)).await?
            } else {
                mount.source.clone()
            };
            let cid_with_subpath = ipfs_path
                .strip_prefix("/ipfs/")
                .with_context(|| format!("expected resolved /ipfs/ path, got {ipfs_path}"))?;
            cids.push(cid_with_subpath.to_string());
        } else {
            let cid = await_or_cancel(cancel, ipfs_client.add_dir(Path::new(&mount.source)))
                .await
                .with_context(|| format!("Failed to add local layer to IPFS: {}", mount.source))?;
            cids.push(cid);
        }
    }

    let root_cid = dag_merge(&cids, ipfs_client, cancel).await?;
    tracing::info!(cid = %root_cid, layers = cids.len(), "Virtual DAG merge complete");

    Ok((root_cid, std::collections::HashMap::new()))
}

/// Split `/ipns/<hash>[/<subpath>]` into `(hash, subpath)`. `subpath`
/// is `""` when the path has no subpath component.
///
/// Pure function — kept separate from `resolve_ipns_to_ipfs` so the
/// parsing can be unit-tested without an IPFS daemon.
fn split_ipns_path(path: &str) -> Result<(&str, &str)> {
    let after_prefix = path
        .strip_prefix("/ipns/")
        .with_context(|| format!("expected /ipns/ prefix, got {path}"))?;
    if after_prefix.is_empty() {
        bail!("empty IPNS hash in path: {path}");
    }
    Ok(match after_prefix.find('/') {
        Some(i) => (&after_prefix[..i], &after_prefix[i + 1..]),
        None => (after_prefix, ""),
    })
}

/// Resolve `/ipns/<hash>[/<subpath>]` to `/ipfs/<cid>[/<subpath>]`.
///
/// Kubo's `name/resolve` only resolves the IPNS hash — it doesn't
/// preserve any subpath, so we splice the subpath back ourselves.
async fn resolve_ipns_to_ipfs(ipns_path: &str, ipfs_client: &ipfs::BootClient) -> Result<String> {
    let (hash, subpath) = split_ipns_path(ipns_path)?;
    let resolved = ipfs_client
        .name_resolve(hash)
        .await
        .with_context(|| format!("failed to resolve IPNS name: {hash}"))?;
    Ok(if subpath.is_empty() {
        resolved
    } else {
        format!("{}/{}", resolved.trim_end_matches('/'), subpath)
    })
}

/// Read the current head from an Atom contract via one-shot `eth_call`.
///
/// Returns `CurrentHead { seq, cid }` where `cid` is raw binary bytes
/// from the contract's `head()` view function.
pub async fn read_contract_head(rpc_url: &str, contract: &[u8; 20]) -> Result<atom::CurrentHead> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("Failed to build HTTP client")?;

    let params = serde_json::json!([{
        "to": format!("0x{}", hex::encode(contract)),
        "data": format!("0x{}", hex::encode(atom::abi::HEAD_SELECTOR)),
    }, "latest"]);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": params,
    });

    let resp = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("eth_call request failed")?;

    let json: serde_json::Value = resp.json().await.context("Failed to parse RPC response")?;

    if let Some(err) = json.get("error") {
        bail!("RPC error: {err}");
    }

    let result_str = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing result in RPC response"))?;

    let bytes = hex::decode(result_str.strip_prefix("0x").unwrap_or(result_str))
        .context("Failed to decode hex from eth_call result")?;

    atom::abi::decode_head_return(&bytes).context("Failed to decode head() return data")
}

/// Convert raw binary CID bytes to an IPFS path string.
///
/// CIDv0 renders as `/ipfs/Qm...` (base58btc), CIDv1 as `/ipfs/bafy...` (base32lower).
pub fn cid_bytes_to_ipfs_path(cid_bytes: &[u8]) -> Result<String> {
    if cid_bytes.is_empty() {
        bail!("Empty CID bytes");
    }
    let cid = Cid::read_bytes(cid_bytes).context("Failed to parse CID from bytes")?;
    Ok(format!("/ipfs/{cid}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn stub_ipfs_client() -> ipfs::BootClient {
        ipfs::BootClient::new(ipfs::HttpClient::new("http://localhost:5001".into()), 1, 1)
    }

    fn root_mount(path: &str) -> Mount {
        Mount {
            source: path.to_string(),
            target: PathBuf::from("/"),
        }
    }

    #[test]
    fn merge_namespace_name_roundtrips_its_creation_time() {
        let name = mfs_namespace_name(1_752_000_000, 0xdead_beef);
        assert_eq!(
            mfs_namespace_parts(&name).map(|(created_at, _)| created_at),
            Some(1_752_000_000)
        );
    }

    #[test]
    fn merge_namespace_parser_ignores_legacy_and_unrecognized_names() {
        assert_eq!(mfs_namespace_parts("ww-merge-deadbeefdeadbeef"), None);
        assert_eq!(mfs_namespace_parts("not-a-time-deadbeefdeadbeef"), None);
        assert_eq!(mfs_namespace_parts("unrelated"), None);
    }

    fn mfs_listing_response(entries: Vec<serde_json::Value>) -> Vec<u8> {
        let body = serde_json::json!({ "Entries": entries }).to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn mfs_directory_entry(name: impl Into<String>) -> serde_json::Value {
        serde_json::json!({ "Name": name.into(), "Hash": "Qmfixture", "Size": 0, "Type": 1 })
    }

    #[tokio::test]
    async fn stale_mfs_sweep_removes_only_owned_reapable_workspaces() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stale_time = now - MFS_STALE_AFTER.as_secs() - 1;
        let foreign = mfs_namespace_name(stale_time, 0x11);
        let malformed = "ww-merge-legacy-path";
        let recent = mfs_namespace_name(now, 0x22);
        let stale = mfs_namespace_name(stale_time, 0x33);
        let active = mfs_namespace_name(stale_time, 0x44);
        let (_, stale_token) = mfs_namespace_parts(&stale).unwrap();
        let (_, active_token) = mfs_namespace_parts(&active).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = Arc::clone(&requests);

        let root_listing = mfs_listing_response(vec![
            mfs_directory_entry(&foreign),
            mfs_directory_entry(malformed),
            mfs_directory_entry(&recent),
            mfs_directory_entry(&stale),
            mfs_directory_entry(&active),
        ]);
        let foreign_listing = mfs_listing_response(vec![
            mfs_directory_entry(mfs_owner_marker("00000000000000000000000000000000")),
            mfs_directory_entry(mfs_reapable_marker("00000000000000000000000000000000")),
        ]);
        let stale_listing = mfs_listing_response(vec![
            mfs_directory_entry(mfs_owner_marker(stale_token)),
            mfs_directory_entry(mfs_reapable_marker(stale_token)),
        ]);
        let active_listing =
            mfs_listing_response(vec![mfs_directory_entry(mfs_owner_marker(active_token))]);
        let responses = vec![
            (
                format!("/api/v0/files/ls?arg={MFS_MERGE_ROOT}&long=true"),
                root_listing,
            ),
            (
                format!("/api/v0/files/ls?arg={MFS_MERGE_ROOT}/{foreign}&long=true"),
                foreign_listing,
            ),
            (
                format!("/api/v0/files/ls?arg={MFS_MERGE_ROOT}/{stale}&long=true"),
                stale_listing,
            ),
            (
                format!("/api/v0/files/rm?arg={MFS_MERGE_ROOT}/{stale}&recursive=true"),
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ),
            (
                format!("/api/v0/files/ls?arg={MFS_MERGE_ROOT}/{active}&long=true"),
                active_listing,
            ),
        ];
        let server = tokio::spawn(async move {
            for (expected, response) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let bytes = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..bytes]).into_owned();
                assert!(request.contains(&expected), "unexpected request: {request}");
                server_requests.lock().unwrap().push(request);
                stream.write_all(&response).await.unwrap();
            }
        });

        let client =
            ipfs::BootClient::new(ipfs::HttpClient::new(format!("http://{address}")), 1, 1);
        assert_eq!(sweep_stale_mfs_namespaces(&client).await.unwrap(), 1);
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("fake Kubo must receive only the safe sweep requests")
            .unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests.iter().all(|request| !request.contains(&recent)));
        assert!(requests.iter().all(|request| !request.contains(malformed)));
        assert!(requests
            .iter()
            .filter(|request| request.contains("/api/v0/files/rm"))
            .all(|request| request.contains(&stale)));
    }

    // ── resolve_mounts_virtual tests (production path) ──
    //
    // Two pure-validation cases live here (no IPFS roundtrip needed).
    //
    // Merge correctness (`dag_merge` over multiple layers) is NOT unit-tested
    // here: those paths require Kubo to `add_dir` local layers, and CI's
    // daemon does not reliably accept ephemeral `tempfile::TempDir` paths
    // inside the test runner. The previous `apply_mounts` / `merge_layers`
    // tests only worked because the deleted code had an all-local
    // `copy_merge` fast path that never hit IPFS — now gone.

    #[tokio::test]
    async fn test_virtual_empty_mounts_errors() {
        let client = stub_ipfs_client();
        let result = resolve_mounts_virtual(&[], &client).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No mounts"));
    }

    #[tokio::test]
    async fn test_virtual_nonexistent_root_errors() {
        let client = stub_ipfs_client();
        let result =
            resolve_mounts_virtual(&[root_mount("/nonexistent/path/abc123")], &client).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dag_merge_retries_a_transient_copy_in_a_fresh_namespace() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            let mut copies = 0;
            // Two attempts: mkdir + owner marker + copy, then direct cleanup
            // after the uncertain failed attempt. The successful attempt adds
            // overlay/MFS listings and stat before marking itself reapable and
            // cleaning up.
            for _ in 0..12 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let bytes = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..bytes]).into_owned();
                let response = if request.contains("/api/v0/files/cp") {
                    copies += 1;
                    if copies == 1 {
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"Message\":\"busy\"}".as_slice()
                    } else {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .as_slice()
                    }
                } else if request.contains("/api/v0/ls?arg=/ipfs/overlay") {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 26\r\nConnection: close\r\n\r\n{\"Objects\":[{\"Links\":[]}]}".as_slice()
                } else if request.contains("/api/v0/files/ls") {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{\"Entries\":[]}".as_slice()
                } else if request.contains("/api/v0/files/stat") {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 45\r\nConnection: close\r\n\r\n{\"Hash\":\"merged\",\"Size\":0,\"Type\":\"directory\"}".as_slice()
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
                };
                server_requests.lock().unwrap().push(request);
                stream.write_all(response).await.unwrap();
            }
        });
        let client =
            ipfs::BootClient::new(ipfs::HttpClient::new(format!("http://{address}")), 3, 1);

        let result = resolve_mounts_virtual(
            &[root_mount("/ipfs/base"), root_mount("/ipfs/overlay")],
            &client,
        )
        .await;
        assert_eq!(result.unwrap().0, "merged");
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("fake Kubo must receive every expected request")
            .unwrap();

        let requests = requests.lock().unwrap();
        let copies: Vec<_> = requests
            .iter()
            .filter(|request| request.contains("/api/v0/files/cp"))
            .collect();
        assert_eq!(copies.len(), 2);
        assert_ne!(
            copies[0], copies[1],
            "a transient copy must retry in a fresh MFS namespace"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| {
                    request.contains("/api/v0/files/mkdir")
                        && request.contains(MFS_REAPABLE_MARKER_PREFIX)
                })
                .count(),
            1,
            "only the completed merge may become sweep-eligible"
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_root_ipns_resolution() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        let client =
            ipfs::BootClient::new(ipfs::HttpClient::new(format!("http://{address}")), 0, 1);
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        let mounts = [root_mount("/ipns/k51-test")];
        let resolution = resolve_mounts_virtual_with_cancel(&mounts, &client, &mut cancel_rx);
        tokio::pin!(resolution);

        tokio::select! {
            result = &mut resolution => panic!("root resolution completed before cancellation: {result:?}"),
            started = started_rx => started.expect("fake Kubo must receive the root IPNS request before cancellation"),
        }
        cancel_tx.send(true).unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), &mut resolution)
            .await
            .expect("root IPNS resolution must observe cancellation")
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        server.abort();
    }

    #[tokio::test]
    async fn test_virtual_targeted_mounts_rejected() {
        let client = stub_ipfs_client();
        let mounts = vec![
            Mount {
                source: "/ipfs/bafybeigdyrzt".to_string(),
                target: PathBuf::from("/"),
            },
            Mount {
                source: "./local-secret".to_string(),
                target: PathBuf::from("/etc/identity"),
            },
        ];
        let result = resolve_mounts_virtual(&mounts, &client).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("targeted mounts are not supported in backend virtual mode"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("received 1 targeted mount(s)"),
            "error should include targeted mount count: {msg}"
        );
        assert!(
            msg.contains("publish content to IPFS/IPNS and mount as a root layer"),
            "error should include migration guidance: {msg}"
        );
    }

    // ── split_ipns_path: pure parsing, IPNS-to-IPFS subpath split ──

    #[test]
    fn split_ipns_path_with_subpath_returns_hash_and_subpath() {
        let (hash, sub) =
            split_ipns_path("/ipns/k51qzi5uqu5dg9eci41ad4b1wyf9kocngntfviq12qjuvusra3nt94xlx98me1/examples/snap-hello-rs")
                .unwrap();
        assert_eq!(
            hash,
            "k51qzi5uqu5dg9eci41ad4b1wyf9kocngntfviq12qjuvusra3nt94xlx98me1"
        );
        assert_eq!(sub, "examples/snap-hello-rs");
    }

    #[test]
    fn split_ipns_path_no_subpath_returns_empty_subpath() {
        let (hash, sub) =
            split_ipns_path("/ipns/k51qzi5uqu5dg9eci41ad4b1wyf9kocngntfviq12qjuvusra3nt94xlx98me1")
                .unwrap();
        assert_eq!(
            hash,
            "k51qzi5uqu5dg9eci41ad4b1wyf9kocngntfviq12qjuvusra3nt94xlx98me1"
        );
        assert_eq!(sub, "");
    }

    #[test]
    fn split_ipns_path_trailing_slash_yields_empty_subpath() {
        let (hash, sub) = split_ipns_path("/ipns/abc/").unwrap();
        assert_eq!(hash, "abc");
        assert_eq!(sub, "");
    }

    #[test]
    fn split_ipns_path_empty_hash_errors() {
        let err = split_ipns_path("/ipns/").unwrap_err();
        assert!(err.to_string().contains("empty IPNS hash"));
    }

    #[test]
    fn split_ipns_path_missing_prefix_errors() {
        let err = split_ipns_path("/ipfs/abc").unwrap_err();
        assert!(err.to_string().contains("expected /ipns/ prefix"));
    }

    #[test]
    fn split_ipns_path_nested_subpath_preserved() {
        // A deeper subpath: every '/' after the hash is part of the subpath.
        let (hash, sub) = split_ipns_path("/ipns/k51abc/a/b/c/d.glia").unwrap();
        assert_eq!(hash, "k51abc");
        assert_eq!(sub, "a/b/c/d.glia");
    }

    #[test]
    fn test_cid_bytes_to_ipfs_path_v0() {
        let mut cid_bytes = vec![0x12, 0x20];
        cid_bytes.extend_from_slice(&[0xAB; 32]);
        let path = cid_bytes_to_ipfs_path(&cid_bytes).unwrap();
        assert!(
            path.starts_with("/ipfs/Qm"),
            "CIDv0 should start with /ipfs/Qm, got: {path}"
        );
    }

    #[test]
    fn test_cid_bytes_to_ipfs_path_v1() {
        let mut mh_bytes = vec![0x12, 0x20];
        mh_bytes.extend_from_slice(&[0xAB; 32]);
        let mh = cid::multihash::Multihash::from_bytes(&mh_bytes).unwrap();
        let cid = Cid::new_v1(0x70, mh);
        let cid_bytes = cid.to_bytes();
        let path = cid_bytes_to_ipfs_path(&cid_bytes).unwrap();
        assert!(
            path.starts_with("/ipfs/bafy"),
            "CIDv1 should start with /ipfs/bafy, got: {path}"
        );
    }

    #[test]
    fn test_cid_bytes_to_ipfs_path_empty_errors() {
        let result = cid_bytes_to_ipfs_path(&[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Empty CID bytes"));
    }
}
