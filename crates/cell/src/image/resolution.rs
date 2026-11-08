//! Freeze mount syntax into retained immutable operations once per invocation.

use std::{fmt, path::Path};

use anyhow::{ensure, Context, Result};
use cid::Cid;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::mount::{classify_source, Mount, MountSource, MAX_MOUNT_OPERATIONS};

use super::{
    await_or_cancel, composer, CaptureBatch, CaptureCancellation, CaptureRequest, CompositionError,
    FrozenMount, GuestTarget, SourceKind,
};

/// Acknowledged identities, not independent pin ownership claims. Every root
/// must enter deployment protection before any preparation starts.
#[derive(Debug)]
pub struct ResolvedMounts {
    pub mounts: Vec<FrozenMount>,
    pub retained_roots: Vec<Cid>,
}

/// Failure does not unpin acknowledged, shared, or uncertain source roots.
#[derive(Debug)]
pub struct MountResolutionFailure {
    pub retained_roots: Vec<Cid>,
    pub uncertain_publication: bool,
    error: anyhow::Error,
}

impl fmt::Display for MountResolutionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "mount resolution failed: {:#}; acknowledged source roots remain retained: {:?}",
            self.error, self.retained_roots
        )?;
        if self.uncertain_publication {
            write!(
                f,
                "; publication outcome is uncertain and may have left blocks or pins"
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for MountResolutionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Captured startup operations. Local bytes and resolved immutable identities
/// remain owned until publication; this value owns no Kubo pins.
pub struct CapturedMounts {
    local: Option<CaptureBatch>,
    operations: Vec<CapturedOperation>,
    operation_digests: Vec<[u8; 32]>,
    digest: [u8; 32],
    inspection: composer::InspectionBudget,
}

#[derive(Debug)]
enum CapturedOperation {
    Local { target: GuestTarget },
    Immutable { root: Cid, mount: Box<FrozenMount> },
}

impl fmt::Debug for CapturedMounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedMounts")
            .field("operations", &self.operations)
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

impl CapturedMounts {
    /// SHA-256 over versioned, length-prefixed ordered operation identities.
    /// This digest binds captured content, source kind and exact guest targets.
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// Identities in invocation order. The caller can bind its baseline
    /// separately without recapturing that operation.
    pub fn operation_digests(&self) -> &[[u8; 32]] {
        &self.operation_digests
    }

    /// Publish the admitted capture without reopening paths or resolving names.
    /// Failure preserves acknowledged roots and reports uncertain publication.
    pub async fn publish_with_cancel(
        self,
        client: &ipfs::BootClient,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<ResolvedMounts> {
        let Self {
            local,
            operations,
            mut inspection,
            ..
        } = self;
        let mut retained_roots = Vec::new();
        let mut uncertain_publication = false;
        let result = async {
            ensure!(
                !*cancel.borrow() && cancel.has_changed().is_ok(),
                "mount resolution cancelled before publication"
            );
            let mut local_sources = match local {
                Some(batch) => match batch
                    .publish_with_budget(client, cancel, &mut inspection)
                    .await
                {
                    Ok(published) => {
                        let sources = published.into_sources();
                        retained_roots.extend(sources.iter().map(|source| source.cid()));
                        sources.into_iter()
                    }
                    Err(error) => {
                        retained_roots.extend(error.acknowledged_roots.iter().copied());
                        uncertain_publication = error.uncertain_source.is_some();
                        return Err(anyhow::Error::new(error));
                    }
                },
                None => Vec::new().into_iter(),
            };
            let mut frozen = Vec::with_capacity(operations.len());
            for operation in operations {
                match operation {
                    CapturedOperation::Local { target } => {
                        let source = local_sources
                            .next()
                            .context("missing captured mount source")?;
                        frozen.push(FrozenMount::new(source, target)?);
                    }
                    CapturedOperation::Immutable { root, mount } => {
                        // Inspection was read-only. Fetch and recursively pin the
                        // original immutable root before acknowledging retention.
                        // A disappeared root fails; it never triggers re-resolution.
                        uncertain_publication = true;
                        await_or_cancel(cancel, client.pin_add_once(&root.to_string()))
                            .await
                            .with_context(|| {
                                format!("fetching and retaining immutable mount source root {root}")
                            })?;
                        await_or_cancel(cancel, client.import_composed(&root, &[]))
                            .await
                            .with_context(|| {
                                format!("retaining immutable mount source root {root}")
                            })?;
                        retained_roots.push(root);
                        uncertain_publication = false;
                        if mount.source().cid() != root {
                            uncertain_publication = true;
                            await_or_cancel(
                                cancel,
                                client.import_composed(&mount.source().cid(), &[]),
                            )
                            .await
                            .with_context(|| {
                                format!(
                                    "retaining selected immutable source {}",
                                    mount.source().cid()
                                )
                            })?;
                            retained_roots.push(mount.source().cid());
                            uncertain_publication = false;
                        }
                        frozen.push(*mount);
                    }
                }
            }
            Ok(frozen)
        }
        .await;
        match result {
            Ok(mounts) => Ok(ResolvedMounts {
                mounts,
                retained_roots,
            }),
            Err(error) => Err(MountResolutionFailure {
                retained_roots,
                uncertain_publication,
                error,
            }
            .into()),
        }
    }
}

/// Capture all local paths and inspect all immutable references before any
/// explicit pin/import/publication. Named sources resolve exactly once.
/// Local paths must already be absolute invocation-time paths.
///
/// Dropping the future requests local-worker cancellation. The caller still
/// waits for its filesystem completion barrier before releasing that lifecycle.
pub async fn capture_frozen_mounts_with_cancel(
    mounts: &[Mount],
    client: &ipfs::BootClient,
    transaction: &cache::ForegroundTransaction,
    authoritative_head: bool,
    cancel: &mut watch::Receiver<bool>,
) -> Result<CapturedMounts> {
    ensure!(
        mounts.len() + usize::from(authoritative_head) <= MAX_MOUNT_OPERATIONS,
        "mount operation limit exceeded (including authoritative head)"
    );
    let sources = mounts
        .iter()
        .map(|mount| classify_source(&mount.source))
        .collect::<Result<Vec<_>>>()?;
    let requests = mounts
        .iter()
        .zip(&sources)
        .filter_map(|(mount, source)| matches!(source, MountSource::Local).then_some(mount))
        .map(|mount| {
            ensure!(
                Path::new(&mount.source).is_absolute(),
                "local mount must be frozen against invocation CWD before resolution: {}",
                mount.source
            );
            Ok(CaptureRequest {
                path: mount.source.clone().into(),
                root_mount: mount.is_root(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let result = async {
        ensure!(
            !*cancel.borrow() && cancel.has_changed().is_ok(),
            "mount resolution cancelled"
        );
        let local = if requests.is_empty() {
            None
        } else {
            let cancellation = CaptureCancellation::default();
            let capture = CaptureBatch::capture(requests, transaction, cancellation.clone());
            tokio::pin!(capture);
            Some(tokio::select! {
                biased;
                _ = cancel.changed() => {
                    cancellation.cancel();
                    let _ = capture.await;
                    anyhow::bail!("mount resolution cancelled during capture");
                },
                result = &mut capture => result?,
            })
        };
        let mut local_sources = local
            .as_ref()
            .map_or(&[][..], |batch| batch.sources())
            .iter();
        let mut inspection = composer::InspectionBudget::default();
        let mut operations = Vec::with_capacity(mounts.len());
        let mut operation_digests: Vec<[u8; 32]> = Vec::with_capacity(mounts.len());
        for (mount, source) in mounts.iter().zip(sources) {
            let mut identity = Sha256::new();
            hash_field(&mut identity, b"wetware-captured-mount-v1");
            hash_field(&mut identity, mount.target.to_string().as_bytes());
            let operation = match source {
                MountSource::Local => {
                    hash_field(&mut identity, b"local");
                    hash_field(&mut identity, mount.source.as_bytes());
                    let source = local_sources
                        .next()
                        .context("missing captured mount source")?;
                    ensure!(
                        !mount.is_root()
                            || source
                                .entries()
                                .first()
                                .is_some_and(|entry| entry.data.is_none()),
                        CompositionError::InvalidSource(
                            "root mount source is not a directory".into()
                        )
                    );
                    identity.update((source.entries().len() as u64).to_be_bytes());
                    for entry in source.entries() {
                        hash_field(&mut identity, entry.path.as_bytes());
                        match &entry.data {
                            None => hash_field(&mut identity, b"directory"),
                            Some(bytes) => {
                                hash_field(&mut identity, b"file");
                                identity.update((bytes.len() as u64).to_be_bytes());
                                hash_field(
                                    &mut identity,
                                    source
                                        .digests()
                                        .get(&entry.path)
                                        .context("missing captured file digest")?,
                                );
                            }
                        }
                    }
                    CapturedOperation::Local {
                        target: mount.target.clone(),
                    }
                }
                source => {
                    let (root, components) = match source {
                        MountSource::Immutable { root, components } => {
                            hash_field(&mut identity, b"immutable");
                            (root, components)
                        }
                        MountSource::Named { name, components } => {
                            hash_field(&mut identity, b"named");
                            hash_field(&mut identity, name.as_bytes());
                            hash_components(&mut identity, &components);
                            let resolved =
                                await_or_cancel(cancel, client.name_resolve(&name)).await?;
                            let (root, mut resolved_components) =
                                crate::mount::parse_immutable_path(&resolved)?;
                            ensure!(
                                resolved_components.len() + components.len() <= 128,
                                "resolved IPNS source depth limit exceeded"
                            );
                            resolved_components.extend(components);
                            (root, resolved_components)
                        }
                        MountSource::Local => unreachable!(),
                    };
                    hash_field(&mut identity, &root.to_bytes());
                    hash_components(&mut identity, &components);
                    let selected = await_or_cancel(
                        cancel,
                        composer::inspect_path(client, root, &components, &mut inspection),
                    )
                    .await?;
                    hash_field(&mut identity, &selected.cid().to_bytes());
                    hash_field(
                        &mut identity,
                        match selected.kind() {
                            SourceKind::Directory => b"directory",
                            SourceKind::File => b"file",
                        },
                    );
                    identity.update(selected.dag_size().to_be_bytes());
                    CapturedOperation::Immutable {
                        root,
                        mount: Box::new(FrozenMount::new(selected, mount.target.clone())?),
                    }
                }
            };
            operation_digests.push(identity.finalize().into());
            operations.push(operation);
        }
        let mut identity = Sha256::new();
        hash_field(&mut identity, b"wetware-captured-mounts-v1");
        identity.update((operation_digests.len() as u64).to_be_bytes());
        for digest in &operation_digests {
            hash_field(&mut identity, digest);
        }
        Ok(CapturedMounts {
            local,
            operations,
            operation_digests,
            digest: identity.finalize().into(),
            inspection,
        })
    }
    .await;
    result.map_err(|error| {
        MountResolutionFailure {
            retained_roots: Vec::new(),
            uncertain_publication: false,
            error,
        }
        .into()
    })
}

fn hash_field(identity: &mut Sha256, bytes: &[u8]) {
    identity.update((bytes.len() as u64).to_be_bytes());
    identity.update(bytes);
}

fn hash_components(identity: &mut Sha256, components: &[String]) {
    identity.update((components.len() as u64).to_be_bytes());
    for component in components {
        hash_field(identity, component.as_bytes());
    }
}

/// Resolve and publish one capture for callers without a separate admission gate.
pub async fn resolve_frozen_mounts_with_cancel(
    mounts: &[Mount],
    client: &ipfs::BootClient,
    transaction: &cache::ForegroundTransaction,
    authoritative_head: bool,
    cancel: &mut watch::Receiver<bool>,
) -> Result<ResolvedMounts> {
    capture_frozen_mounts_with_cancel(mounts, client, transaction, authoritative_head, cancel)
        .await?
        .publish_with_cancel(client, cancel)
        .await
}
