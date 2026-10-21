//! Freeze mount syntax into retained immutable operations once per invocation.

use std::{fmt, path::Path};

use anyhow::{ensure, Context, Result};
use cid::Cid;
use tokio::sync::watch;

use crate::mount::{classify_source, Mount, MountSource, MAX_MOUNT_OPERATIONS};

use super::{
    await_or_cancel, composer, CaptureBatch, CaptureCancellation, CaptureRequest, FrozenMount,
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

/// All local paths must already be absolute invocation-time paths. Captures the
/// complete local batch before publishing any local source. Name resolution
/// happens once; subsequent traversal and deployment use immutable identities.
/// Cancellation during capture waits for the worker to release descriptors.
/// Dropped futures still require the caller's filesystem completion barrier.
pub async fn resolve_frozen_mounts_with_cancel(
    mounts: &[Mount],
    client: &ipfs::BootClient,
    transaction: &cache::ForegroundTransaction,
    authoritative_head: bool,
    cancel: &mut watch::Receiver<bool>,
) -> Result<ResolvedMounts> {
    ensure!(
        mounts.len() + usize::from(authoritative_head) <= MAX_MOUNT_OPERATIONS,
        "mount operation limit exceeded (including authoritative head)"
    );
    // Complete pure validation before admission, capture, or network work.
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
    let mut retained_roots = Vec::new();
    let mut uncertain_publication = false;
    let result = async {
        ensure!(
            !*cancel.borrow() && cancel.has_changed().is_ok(),
            "mount resolution cancelled"
        );
        let captured = if requests.is_empty() {
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
        let mut budget = composer::InspectionBudget::default();
        let mut local_sources = match captured {
            Some(batch) => {
                // On cancellation the publication layer reports uncertainty.
                match batch.publish_with_budget(client, cancel, &mut budget).await {
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
                }
            }
            None => Vec::new().into_iter(),
        };
        let mut frozen = Vec::with_capacity(mounts.len());
        for (mount, source) in mounts.iter().zip(sources) {
            let source = match source {
                MountSource::Local => local_sources
                    .next()
                    .context("missing captured mount source")?,
                source => {
                    let (root, components) = match source {
                        MountSource::Immutable { root, components } => (root, components),
                        MountSource::Named { name, components } => {
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
                    // Retain the original immutable root before reading any path.
                    // Failed acknowledgements are never compensated by unpin.
                    uncertain_publication = true;
                    // CAR import pins offline. Fetch and recursively pin through
                    // Kubo's network first; the CAR response then confirms the
                    // exact root using the strict acknowledgement parser.
                    await_or_cancel(cancel, client.pin_add_once(&root.to_string()))
                        .await
                        .with_context(|| {
                            format!("fetching and retaining immutable mount source root {root}")
                        })?;
                    await_or_cancel(cancel, client.import_composed(&root, &[]))
                        .await
                        .with_context(|| format!("retaining immutable mount source root {root}"))?;
                    retained_roots.push(root);
                    uncertain_publication = false;
                    let selected = await_or_cancel(
                        cancel,
                        composer::inspect_path(client, root, &components, &mut budget),
                    )
                    .await?;
                    if selected.cid() != root {
                        uncertain_publication = true;
                        await_or_cancel(cancel, client.import_composed(&selected.cid(), &[]))
                            .await
                            .with_context(|| {
                                format!("retaining selected immutable source {}", selected.cid())
                            })?;
                        retained_roots.push(selected.cid());
                        uncertain_publication = false;
                    }
                    selected
                }
            };
            frozen.push(FrozenMount::new(source, mount.target.clone())?);
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
