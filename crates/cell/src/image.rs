//! Mount-based FHS image resolution for the virtual filesystem.
//!
//! Root layers compose left-to-right through Wetware Composer v1. The composer
//! reads immutable UnixFS directory blocks and writes changed directory nodes.
//! Kubo stores and retains the result; composition does not use MFS.

use std::future::Future;

use anyhow::{bail, Context, Result};
use cid::Cid;
use ipfs::cid_identity::{decode_cid, parse_cid};
use ipld_dagpb::PbNode;
use sha2::{Digest, Sha256};

use crate::mount::Mount;

mod capture;
mod codec;
mod composer;
mod operation;
mod resolution;
mod snapshot;
mod verify_capture;

pub use capture::{
    CaptureBatch, CaptureCancellation, CaptureRequest, CaptureStats, CapturedSource,
};
pub use operation::{CompositionError, FrozenMount, FrozenSource, GuestTarget, SourceKind};
pub use resolution::{resolve_frozen_mounts_with_cancel, MountResolutionFailure, ResolvedMounts};
pub use snapshot::{PublishedSources, SnapshotPublicationFailure};

/// Versioned identity profile for structural image composition.
pub const COMPOSER_PROFILE: &str = composer::PROFILE;

/// Exercises the exact Composer v1 DAG-PB decoder from an external fuzz target.
#[doc(hidden)]
pub fn fuzz_composer_dagpb_decode(data: &[u8]) -> Result<()> {
    let digest = Sha256::digest(data);
    let hash = cid::multihash::Multihash::wrap(0x12, &digest)
        .context("invalid fuzz-input SHA-256 digest")?;
    let cid = Cid::new_v1(0x70, hash);
    let decoded = codec::decode(&cid, data.to_vec())?;

    let wire = PbNode::from_bytes(bytes::Bytes::copy_from_slice(data))
        .expect("production DAG-PB decode accepted bytes the wire decoder rejected");
    let cumulative_size = wire.links.iter().fold(data.len() as u64, |size, link| {
        size.checked_add(
            link.size
                .expect("production DAG-PB decode accepted a link without Tsize"),
        )
        .expect("production DAG-PB decode accepted cumulative-size overflow")
    });

    match decoded {
        codec::Node::Directory(directory) => {
            assert_eq!(wire.data.as_deref(), Some(directory.data.as_ref()));
            assert_eq!(wire.links.len(), directory.entries.len());
            for link in &wire.links {
                let name = link
                    .name
                    .as_ref()
                    .expect("production directory decode accepted a nameless link");
                let entry = directory
                    .entries
                    .get(name)
                    .expect("decoded directory omitted a wire link");
                assert_eq!(entry.cid, link.cid);
                assert_eq!(entry.size, link.size.unwrap());
            }

            let encoded = codec::encode(&directory)
                .expect("production directory decode produced an unencodable directory");
            assert_eq!(encoded.2, cumulative_size);
            let redecoded = codec::decode(&encoded.0, encoded.1.clone())
                .expect("production directory encoding did not decode");
            assert_eq!(redecoded, codec::Node::Directory(directory));
            let codec::Node::Directory(redecoded_directory) = redecoded else {
                unreachable!("directory equality checked above")
            };
            assert_eq!(
                codec::encode(&redecoded_directory)
                    .expect("redecoded production directory was not encodable"),
                encoded
            );
        }
        codec::Node::File { file, size } => {
            assert_eq!(size, cumulative_size);
            assert_eq!(file.links.len(), wire.links.len());
            assert_eq!(
                file.logical_size,
                file.links.iter().fold(file.inline_size, |size, link| {
                    size.checked_add(link.logical_size)
                        .expect("production file decode accepted logical-size overflow")
                })
            );
            for (decoded_link, wire_link) in file.links.iter().zip(&wire.links) {
                assert_eq!(decoded_link.cid, wire_link.cid);
                assert_eq!(decoded_link.size, wire_link.size.unwrap());
            }
        }
        codec::Node::Raw { size, .. } | codec::Node::Symlink { size } => {
            assert_eq!(size, cumulative_size);
            assert!(wire.links.is_empty());
        }
    }
    Ok(())
}

/// Exercises the exact Composer v1 UnixFS metadata decoder from a fuzz target.
#[doc(hidden)]
pub fn fuzz_composer_unixfs_metadata(data: &[u8]) -> Result<()> {
    codec::validate_metadata(data)
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
    let cids = cids
        .iter()
        .map(|value| parse_cid(value).context("invalid merge layer CID"))
        .collect::<Result<Vec<_>>>()?;
    dag_merge_cids(&cids, client, cancel)
        .await
        .map(|cid| cid.to_string())
}

/// Compose validated layer identities and return the pinned effective root.
///
/// Retains CID version, codec, and multihash throughout composition and import.
pub async fn dag_merge_cids(
    cids: &[Cid],
    client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Cid> {
    await_or_cancel(cancel, async {
        let composition = composer::compose(client, cids).await?;
        client
            .import_composed(&composition.root, &composition.blocks)
            .await
            .context("storing and pinning composed root")?;
        Ok(composition.root)
    })
    .await
}

/// Inspect an immutable source already retained and available to the caller.
///
/// Reads standalone raw bytes to verify their CID and cumulative size. Directory
/// raw-child links retain the existing Composer v1 opaque-size contract. This
/// value does not acquire a pin; the caller must keep the source retained.
pub async fn inspect_source(
    cid: Cid,
    client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<FrozenSource> {
    await_or_cancel(cancel, composer::inspect_source(client, cid)).await
}

// A cancelled preparation cannot return its residual-retention report. Keep
// the exact candidate identity in a drop diagnostic while import is in flight.
struct PendingRootRetention {
    cid: Cid,
    acknowledged: bool,
}

impl Drop for PendingRootRetention {
    fn drop(&mut self) {
        if !self.acknowledged {
            tracing::warn!(cid = %self.cid, "Composed-root retention was not acknowledged; residual blocks or pins may remain");
        }
    }
}

/// Apply ordered frozen operations over an optional retained directory root.
///
/// Strict ancestors preserve metadata. Exact directory targets merge; other
/// collisions replace the entry. With no initial root, targeted operations start
/// from a deterministic empty directory. All input DAGs must remain retained by
/// the caller. Success requires the existing recursive CAR pin acknowledgement.
pub async fn compose_mounts(
    initial_root: Option<Cid>,
    mounts: &[FrozenMount],
    client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Cid> {
    await_or_cancel(cancel, async {
        let composition = composer::compose_mounts(client, initial_root, mounts).await?;
        let mut retention = PendingRootRetention { cid: composition.root, acknowledged: false };
        client
            .import_composed(&composition.root, &composition.blocks)
            .await
            .with_context(|| format!(
                "storing and pinning composed root {}; failed acknowledgement may leave residual blocks or pins",
                composition.root
            ))?;
        retention.acknowledged = true;
        Ok(composition.root)
    })
    .await
}

/// Validate all mount syntax before waiting for Kubo. Empty batches are valid
/// when the caller supplies an authoritative head separately.
pub fn validate_mounts_virtual(mounts: &[Mount]) -> Result<Vec<&Mount>> {
    anyhow::ensure!(
        mounts.len() <= crate::mount::MAX_MOUNT_OPERATIONS,
        "mount operation limit exceeded"
    );
    for mount in mounts {
        crate::mount::classify_source(&mount.source)?;
    }
    Ok(mounts.iter().collect())
}

/// Resolve and compose mounts with the caller's admitted filesystem lifecycle.
/// Deployment calls `resolve_frozen_mounts_with_cancel` directly so retained
/// identities enter deployment ownership before composition.
pub async fn resolve_mounts_virtual_with_cancel(
    mounts: &[Mount],
    client: &ipfs::BootClient,
    transaction: &cache::ForegroundTransaction,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(String, Vec<String>)> {
    anyhow::ensure!(!mounts.is_empty(), "No mounts provided");
    let resolved =
        resolve_frozen_mounts_with_cancel(mounts, client, transaction, false, cancel).await?;
    let root = compose_mounts(None, &resolved.mounts, client, cancel).await?;
    Ok((
        root.to_string(),
        resolved
            .retained_roots
            .iter()
            .map(ToString::to_string)
            .collect(),
    ))
}

/// Convert raw binary CID bytes to an IPFS path string.
///
/// CIDv0 renders as `/ipfs/Qm...` (base58btc), CIDv1 as `/ipfs/bafy...` (base32lower).
pub fn cid_bytes_to_ipfs_path(cid_bytes: &[u8]) -> Result<String> {
    if cid_bytes.is_empty() {
        bail!("Empty CID bytes");
    }
    let cid = decode_cid(cid_bytes).context("Failed to parse CID from bytes")?;
    Ok(format!("/ipfs/{cid}"))
}

#[cfg(test)]
#[path = "image/tests.rs"]
mod security_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn composer_dagpb_fuzz_seam_reaches_the_production_decoder() {
        let empty_directory = [0x0a, 0x02, 0x08, 0x01];
        assert!(fuzz_composer_dagpb_decode(&empty_directory).is_ok());

        let unsupported_flat_file = [0x0a, 0x02, 0x08, 0x03];
        assert!(fuzz_composer_dagpb_decode(&unsupported_flat_file).is_err());
    }

    #[test]
    fn composer_unixfs_metadata_fuzz_seam_accepts_supported_local_shapes() {
        for metadata in [
            &[0x08, 0x01][..],
            &[0x08, 0x02, 0x12, 0x01, b'x', 0x18, 0x01],
            &[0x08, 0x00, 0x12, 0x01, b'x', 0x18, 0x01],
            &[0x08, 0x04, 0x12, 0x01, b'x'],
        ] {
            assert!(
                fuzz_composer_unixfs_metadata(metadata).is_ok(),
                "rejected supported metadata {metadata:?}"
            );
        }
    }

    #[test]
    fn composer_unixfs_metadata_fuzz_seam_rejects_invalid_local_shapes() {
        for metadata in [
            &[0x08, 0x01, 0x08, 0x01][..],
            &[0x08, 0x01, 0x38, 0x80, 0x20],
            &[0x08, 0x01, 0x42, 0x00],
            &[0x08, 0x05, 0x28, 0x22, 0x30, 0x80, 0x02],
            &[0x08, 0x01, 0x12, 0x00],
            &[0x08, 0x02, 0x28, 0x00],
            &[0x08, 0x02, 0x12, 0x01, b'x', 0x18, 0x02],
            &[0x08, 0x00, 0x12, 0x01, b'x'],
            &[0x08, 0x04, 0x12, 0x01, 0xff],
        ] {
            assert!(
                fuzz_composer_unixfs_metadata(metadata).is_err(),
                "accepted invalid metadata {metadata:?}"
            );
        }
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

    #[test]
    fn test_cid_bytes_to_ipfs_path_rejects_binary_aliases() {
        let canonical = [0x01, 0x55, 0x00, 0x01, b'x'];
        let mut trailing = canonical.to_vec();
        trailing.push(0);
        let mut concatenated = canonical.to_vec();
        concatenated.extend_from_slice(&canonical);
        let mut nonminimal = vec![0x81, 0x00];
        nonminimal.extend_from_slice(&canonical[1..]);
        for bytes in [
            trailing,
            concatenated,
            nonminimal,
            cid::multibase::decode("f8180808080808080800255000178")
                .unwrap()
                .1,
            cid::multibase::decode("f0155008180808080808080800278")
                .unwrap()
                .1,
        ] {
            assert!(
                cid_bytes_to_ipfs_path(&bytes).is_err(),
                "accepted invalid CID bytes: {bytes:02x?}"
            );
        }
    }
}
