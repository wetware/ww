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
use std::path::Path;

use anyhow::{bail, Context, Result};
use cid::Cid;

use crate::mount::Mount;
use ipfs;

// ── DAG merge via IPFS MFS ─────────────────────────────────────────

/// RAII guard that cleans up an MFS namespace on drop.
struct MfsNamespaceGuard<'a> {
    client: &'a ipfs::HttpClient,
    path: String,
}

impl<'a> MfsNamespaceGuard<'a> {
    async fn new(client: &'a ipfs::HttpClient) -> Result<Self> {
        let id: u64 = rand::random();
        let path = format!("/ww-merge-{id:016x}");
        // Don't pre-create the directory — files_cp will create it when
        // copying the base layer, and fails if the destination already exists.
        Ok(Self { client, path })
    }

    fn path(&self) -> &str {
        &self.path
    }

    async fn cleanup(&self) {
        if let Err(e) = self.client.mfs().files_rm(&self.path, true).await {
            tracing::warn!(path = %self.path, "MFS cleanup failed: {e}");
        }
    }
}

/// Merge multiple root layer CIDs using IPFS MFS operations.
///
/// Layers are applied left-to-right. Later layers win on file conflicts.
/// Directories are merged recursively. Returns the root CID of the merged tree.
async fn dag_merge(cids: &[String], client: &ipfs::HttpClient) -> Result<String> {
    if cids.is_empty() {
        bail!("No CIDs to merge");
    }
    if cids.len() == 1 {
        return Ok(cids[0].clone());
    }

    let guard = MfsNamespaceGuard::new(client).await?;

    // Copy the base layer (O(1) DAG link).
    client
        .mfs()
        .files_cp(&format!("/ipfs/{}", cids[0]), guard.path())
        .await
        .context("Failed to copy base layer to MFS")?;

    // Overlay each subsequent layer.
    for cid in &cids[1..] {
        merge_overlay_recursive(client, guard.path(), &format!("/ipfs/{cid}"))
            .await
            .with_context(|| format!("Failed to merge overlay {cid}"))?;
    }

    // Stat to get merged root CID.
    let stat = client
        .mfs()
        .files_stat(guard.path(), true)
        .await
        .context("Failed to stat merged MFS namespace")?;

    guard.cleanup().await;
    Ok(stat.hash)
}

/// Recursively merge an overlay into the MFS namespace.
///
/// For each entry in the overlay:
/// - Not in base → `files cp` (add)
/// - Both directories → recurse
/// - Any conflict → `files rm` + `files cp` (replace)
fn merge_overlay_recursive<'a>(
    client: &'a ipfs::HttpClient,
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
    client: &ipfs::HttpClient,
    mfs_path: &str,
    overlay_path: &str,
) -> Result<()> {
    let mfs = client.mfs();

    // List overlay entries via the regular ls API.
    let overlay_entries = client
        .ls(overlay_path)
        .await
        .with_context(|| format!("ls overlay {overlay_path}"))?;

    // List existing MFS entries (may be empty if dir is new).
    let mfs_entries = mfs.files_ls(mfs_path).await.unwrap_or_default();
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
    ipfs_client: &ipfs::HttpClient,
) -> Result<(
    String,
    std::collections::HashMap<std::path::PathBuf, crate::vfs::LocalOverride>,
)> {
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

    // Resolve all root mounts to CIDs.
    let mut cids = Vec::with_capacity(root_mounts.len());
    for mount in &root_mounts {
        if ipfs::is_ipfs_path(&mount.source) {
            // `is_ipfs_path` accepts /ipfs/, /ipns/, /ipld/. /ipfs/ goes
            // through directly. /ipns/<hash>[/<sub>] resolves to
            // /ipfs/<cid>[/<sub>] via Kubo's name/resolve. /ipld/ falls
            // through with the same strip behavior as /ipfs/.
            let ipfs_path = if mount.source.starts_with("/ipns/") {
                resolve_ipns_to_ipfs(&mount.source, ipfs_client).await?
            } else {
                mount.source.clone()
            };
            let cid_with_subpath = ipfs_path
                .strip_prefix("/ipfs/")
                .with_context(|| format!("expected resolved /ipfs/ path, got {ipfs_path}"))?;
            cids.push(cid_with_subpath.to_string());
        } else {
            // Add local directory to IPFS.
            let cid = ipfs_client
                .add_dir(Path::new(&mount.source))
                .await
                .with_context(|| format!("Failed to add local layer to IPFS: {}", mount.source))?;
            cids.push(cid);
        }
    }

    // DAG merge to produce a single root CID.
    let root_cid = dag_merge(&cids, ipfs_client).await?;
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
async fn resolve_ipns_to_ipfs(ipns_path: &str, ipfs_client: &ipfs::HttpClient) -> Result<String> {
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

    fn stub_ipfs_client() -> ipfs::HttpClient {
        ipfs::HttpClient::new("http://localhost:5001".into())
    }

    fn root_mount(path: &str) -> Mount {
        Mount {
            source: path.to_string(),
            target: PathBuf::from("/"),
        }
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
