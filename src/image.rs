//! Authority-neutral image preparation shared by deployment and `image check`.
//!
//! Callers retain all source roots before preparation. Composition imports and
//! recursively pins the effective root; a successful return acknowledges that
//! pin, not exclusive ownership. Neither function creates deployment authority,
//! a guest instance, a readiness gate, or runtime services.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use cell::engine::RuntimeEngine;
use cell::image::{compose_mounts, inspect_source, select_kernel_file, FrozenMount, GuestTarget};
use cid::Cid;
use tokio::sync::watch;

use crate::ipfs::BootClient;
use crate::kernel::{Compilation, PreparedKernel};

pub(crate) const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) struct OperationTimeout {
    pub(crate) operation: &'static str,
    pub(crate) timeout: Duration,
}

impl std::fmt::Display for OperationTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "deployment operation {} made no progress within {}s",
            self.operation,
            self.timeout.as_secs_f64()
        )
    }
}

impl std::error::Error for OperationTimeout {}

#[derive(Debug)]
pub(crate) struct PermanentPreparationError(pub(crate) String);

impl std::fmt::Display for PermanentPreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PermanentPreparationError {}

pub(crate) async fn bounded<T>(
    operation: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(OPERATION_TIMEOUT, future)
        .await
        .map_err(|_| OperationTimeout {
            operation,
            timeout: OPERATION_TIMEOUT,
        })?
}

/// Compose retained inputs in boot order: installed baseline, authoritative
/// head, then frozen namespace and positional operations.
///
/// An invalid or unavailable head fails even if a baseline exists. Missing
/// files in a valid head can inherit baseline files under Composer's policy.
pub async fn compose_root(
    head: Option<Cid>,
    installed_baseline: Option<&FrozenMount>,
    frozen_mounts: &[FrozenMount],
    client: &BootClient,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Cid> {
    if *cancel.borrow() {
        anyhow::bail!("deployment preparation cancelled");
    }
    if head.is_none() && installed_baseline.is_none() && frozen_mounts.is_empty() {
        return Err(PermanentPreparationError(
            "deployment has neither a Stem head nor configured root layers".to_owned(),
        )
        .into());
    }
    let mut operations = Vec::new();
    let initial_root = if let Some(baseline) = installed_baseline {
        if let Some(head) = head {
            let source = inspect_source(head, client, cancel).await?;
            operations.push(FrozenMount::new(source, GuestTarget::root())?);
        }
        Some(baseline.source().cid())
    } else {
        head
    };
    operations.extend_from_slice(frozen_mounts);
    bounded(
        "effective-root merge",
        compose_mounts(initial_root, &operations, client, cancel),
    )
    .await
    .context("composing effective deployment root")
}

/// Select verified `/boot/kernel.wasm` bytes and run production P3 static
/// validation. Deployment can prewarm its filesystem before this step.
///
/// Cancellation releases the async waiter. An admitted native compiler retains
/// its physical slot until Wasmtime returns; no guest code executes.
pub async fn prepare_kernel(
    effective: Cid,
    client: &BootClient,
    runtime: RuntimeEngine,
    compilation: &Compilation,
    cancel: &mut watch::Receiver<bool>,
) -> Result<PreparedKernel> {
    let file = select_kernel_file(effective, client, cancel)
        .await
        .context("selecting /boot/kernel.wasm from effective deployment root")?;
    tokio::select! {
        biased;
        _ = preparation_cancelled(cancel) => anyhow::bail!("deployment preparation cancelled"),
        result = PreparedKernel::prepare(file, compilation, runtime) => result,
    }
}

async fn preparation_cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}
