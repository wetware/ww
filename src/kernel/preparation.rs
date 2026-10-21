//! Authority-neutral immutable kernel preparation for future FHS consumers.

use std::sync::Arc;

use anyhow::Result;
use cell::engine::RuntimeEngine;
use cell::image::VerifiedKernelFile;
use cell::proc::ValidatedKernel;
use cid::Cid;

use super::Compilation;

/// A verified file and the production validation of its exact component bytes.
///
/// The constructor accepts only a file produced by explicit-root selection.
/// Neither selection nor compilation publishes active identity or grants any
/// authority. A future deployment must bind this value to its accepted epoch.
pub struct PreparedKernel {
    file: VerifiedKernelFile,
    validated: Arc<ValidatedKernel>,
}

impl PreparedKernel {
    pub async fn prepare(
        file: VerifiedKernelFile,
        compilation: &Compilation,
        runtime: RuntimeEngine,
    ) -> Result<Self> {
        let validated = compilation.prepare(runtime, file.bytes().to_vec()).await?;
        Ok(Self { file, validated })
    }

    pub fn effective_root(&self) -> Cid {
        self.file.effective_root()
    }

    pub fn source_file(&self) -> Cid {
        self.file.source_file()
    }

    pub fn runtime_cid(&self) -> Cid {
        self.validated.runtime_cid()
    }

    pub fn validated(&self) -> Arc<ValidatedKernel> {
        self.validated.clone()
    }
}
