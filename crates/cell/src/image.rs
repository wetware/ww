//! Mount-based FHS image resolution for the virtual filesystem.
//!
//! Root layers compose left-to-right through Wetware Composer v1. The composer
//! reads immutable UnixFS directory blocks and writes changed directory nodes.
//! Kubo stores and retains the result; composition does not use MFS.

use std::future::Future;
use std::path::Path;

use anyhow::{bail, Context, Result};
use cid::Cid;

use crate::mount::Mount;

mod codec;
mod composer;

/// Versioned identity profile for structural image composition.
pub const COMPOSER_PROFILE: &str = composer::PROFILE;

/// Parse a bare CID, without the path-prefix and trailing-byte tolerance of
/// `Cid::from_str`. Only the canonical rendering may enter an IPFS source path.
fn parse_dag_cid(value: &str) -> Result<Cid> {
    let bytes = if value.starts_with("Qm") {
        cid::multibase::Base::Base58Btc.decode(value)
    } else {
        cid::multibase::decode(value).map(|(_, bytes)| bytes)
    }
    .context("invalid bare CID encoding")?;
    let mut remaining = bytes.as_slice();
    let cid = Cid::read_bytes(&mut remaining).context("invalid CID")?;
    if !remaining.is_empty() {
        bail!("invalid CID: trailing bytes");
    }
    // Parsing can accept overflowing varints. Require canonical binary bytes,
    // while allowing alternate textual multibase representations.
    if cid.to_bytes() != bytes {
        bail!("invalid CID: noncanonical binary encoding");
    }
    Ok(cid)
}

async fn await_or_cancel<T, F>(
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    operation: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    if *cancel.borrow() {
        bail!("mount resolution cancelled");
    }
    tokio::select! {
        biased;
        // Check cancellation before a simultaneously ready operation result.
        changed = cancel.changed() => match changed {
            Ok(()) if *cancel.borrow() => Err(anyhow::anyhow!("mount resolution cancelled")),
            Ok(()) => Err(anyhow::anyhow!("mount resolution cancellation channel changed unexpectedly")),
            Err(_) => Err(anyhow::anyhow!("mount resolution cancellation channel closed")),
        },
        result = operation => result,
    }
}

/// Compose ordinary UnixFS directory layers using Wetware Composer v1.
///
/// Layers apply left-to-right. Directory collisions merge recursively; other
/// collisions use the later CID. Only changed directories are encoded.
/// Input DAGs must already be available locally and retained by the caller.
/// Production deployment owns these pins. The returned root is recursively
/// pinned before success. See `doc/composer-v1.md` for the bounded profile.
pub async fn dag_merge(
    cids: &[String],
    client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<String> {
    if cids.is_empty() {
        bail!("No CIDs to merge");
    }
    let cids = cids
        .iter()
        .map(|value| parse_dag_cid(value).context("invalid merge layer CID"))
        .collect::<Result<Vec<_>>>()?;
    await_or_cancel(cancel, async {
        let composition = composer::compose(client, &cids).await?;
        client
            .import_composed(&composition.root, &composition.blocks)
            .await
            .context("storing and pinning composed root")?;
        Ok(composition.root.to_string())
    })
    .await
}

// ── Virtual mount resolution (lazy CidTree path) ─────────────────

/// Resolve mounts into a root CID for the virtual filesystem.
///
/// Performs the DAG merge to produce a merged root CID.
/// Targeted mounts are rejected in backend mode to avoid a second,
/// host-local filesystem path.
///
/// IPFS/IPNS input DAGs must already be locally available and retained by the
/// caller, as required by [`dag_merge`]. Local uploads are pinned by Kubo add.
pub async fn resolve_mounts_virtual(
    mounts: &[Mount],
    ipfs_client: &ipfs::BootClient,
) -> Result<(String, Vec<String>)> {
    let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    resolve_mounts_virtual_with_cancel(mounts, ipfs_client, &mut cancel).await
}

/// Validate local mount configuration before waiting for Kubo.
pub fn validate_mounts_virtual(mounts: &[Mount]) -> Result<Vec<&Mount>> {
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

    Ok(root_mounts)
}

/// Cancellable mount resolution with the same input-retention requirement as
/// [`resolve_mounts_virtual`]. Production deployment resolves layers separately
/// and owns their pins before calling [`dag_merge`].
pub async fn resolve_mounts_virtual_with_cancel(
    mounts: &[Mount],
    ipfs_client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(String, Vec<String>)> {
    if mounts.is_empty() {
        bail!("No mounts provided");
    }
    let cids = resolve_mount_layers_virtual_with_cancel(mounts, ipfs_client, cancel).await?;
    let root_cid = dag_merge(&cids, ipfs_client, cancel).await?;
    tracing::info!(cid = %root_cid, layers = cids.len(), "Virtual DAG merge complete");

    Ok((root_cid, cids))
}

/// Resolve configured root layers to immutable CIDs without composing them.
///
/// Deployment owns composition because a Stem head, when configured, is the
/// first layer and can change after boot. An empty layer list is valid only for
/// callers that supply a Stem head separately.
pub async fn resolve_mount_layers_virtual_with_cancel(
    mounts: &[Mount],
    ipfs_client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Vec<String>> {
    if mounts.is_empty() {
        return Ok(Vec::new());
    }
    let root_mounts = validate_mounts_virtual(mounts)?;
    let mut cids = Vec::with_capacity(root_mounts.len());
    for mount in root_mounts {
        if ipfs::is_ipfs_path(&mount.source) {
            let ipfs_path = if mount.source.starts_with("/ipns/") {
                await_or_cancel(cancel, resolve_ipns_to_ipfs(&mount.source, ipfs_client)).await?
            } else {
                mount.source.clone()
            };
            cids.push(resolve_bare_cid(&ipfs_path, ipfs_client, cancel).await?);
        } else {
            let cid = await_or_cancel(cancel, ipfs_client.add_dir(Path::new(&mount.source)))
                .await
                .with_context(|| format!("Failed to add local layer to IPFS: {}", mount.source))?;
            cids.push(cid);
        }
    }
    Ok(cids)
}

async fn resolve_bare_cid(
    ipfs_path: &str,
    ipfs_client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<String> {
    let cid_with_subpath = ipfs_path
        .strip_prefix("/ipfs/")
        .with_context(|| format!("expected resolved /ipfs/ path, got {ipfs_path}"))?;
    let candidate = if cid_with_subpath.contains('/') {
        let resolved = await_or_cancel(cancel, ipfs_client.resolve(ipfs_path)).await?;
        resolved
            .strip_prefix("/ipfs/")
            .with_context(|| format!("expected resolved /ipfs/ path, got {resolved}"))?
            .to_owned()
    } else {
        cid_with_subpath.to_owned()
    };
    parse_dag_cid(&candidate)
        .with_context(|| format!("invalid resolved CID {candidate}"))
        .map(|cid| cid.to_string())
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
#[path = "image/tests.rs"]
mod security_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::io::AsyncReadExt;
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

    // ── resolve_mounts_virtual tests (production path) ──
    //
    // Two pure-validation cases live here (no IPFS roundtrip needed).
    //
    // Fake-Kubo merge boundary and layer semantics tests live in image/tests.rs.

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
        let (hash, sub) = split_ipns_path("/ipns/k51abc/a/b/c/main.wasm").unwrap();
        assert_eq!(hash, "k51abc");
        assert_eq!(sub, "a/b/c/main.wasm");
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
