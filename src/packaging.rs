//! The publishable FHS contains only the two mandatory component payloads.
//! Identity, installation metadata, and runtime state belong outside this tree.

use std::fs;
use std::path::Path;

use anyhow::{ensure, Context, Result};

pub const KERNEL_PATH: &str = "boot/kernel.wasm";
pub const STATUS_PATH: &str = "bin/status.wasm";

/// Check the exact bytes against the Host's production static validation seams.
/// No guest executes during validation. Status uses the ordinary Cell contract.
pub fn validate_payload(kernel: &[u8], status: &[u8]) -> Result<()> {
    ensure!(!kernel.is_empty(), "kernel payload is empty");
    ensure!(!status.is_empty(), "status payload is empty");
    let (runtime, _clock) = cell::engine::runtime_engine()?;
    cell::proc::ValidatedKernel::compile(&runtime, kernel)
        .context("invalid packaged boot/kernel.wasm")?;
    cell::proc::validate_component(&runtime, status).context("invalid packaged bin/status.wasm")?;
    Ok(())
}

/// Read a mandatory regular file without accepting symlinks or empty payloads.
pub fn read_payload(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("missing payload: {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "payload must be a regular file: {}",
        path.display()
    );
    ensure!(metadata.len() > 0, "payload is empty: {}", path.display());
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    ensure!(!bytes.is_empty(), "payload is empty: {}", path.display());
    Ok(bytes)
}

/// Reject extra paths and symlinks, including private state accidentally copied
/// from an installation. Missing managed files are permitted while staging.
fn check_layout(root: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(root)?.file_type().is_dir(),
        "FHS must be a directory: {}",
        root.display()
    );
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(root)?;
        let expected_directory = relative == Path::new("boot") || relative == Path::new("bin");
        let expected_file =
            relative == Path::new(KERNEL_PATH) || relative == Path::new(STATUS_PATH);
        ensure!(
            (expected_directory && entry.file_type().is_dir())
                || (expected_file && entry.file_type().is_file()),
            "unexpected path or file type in publishable FHS: {}",
            relative.display()
        );
    }
    Ok(())
}

fn verify_contents(root: &Path, kernel: &[u8], status: &[u8]) -> Result<()> {
    check_layout(root)?;
    ensure!(
        read_payload(&root.join(KERNEL_PATH))? == kernel,
        "stale or mismatched boot/kernel.wasm"
    );
    ensure!(
        read_payload(&root.join(STATUS_PATH))? == status,
        "stale or mismatched bin/status.wasm"
    );
    Ok(())
}

/// Validate an existing package and compare both files with canonical producers.
pub fn verify_fhs(root: &Path, kernel: &[u8], status: &[u8]) -> Result<()> {
    verify_contents(root, kernel, status)?;
    validate_payload(kernel, status)
}

/// Stage only validated producer bytes. Each replacement is atomic. A stopped
/// two-file update is detected by verify_fhs and repaired by rerunning staging.
pub fn stage_fhs(root: &Path, kernel: &[u8], status: &[u8]) -> Result<()> {
    validate_payload(kernel, status)?;
    if root.symlink_metadata().is_ok() {
        check_layout(root)?;
    }
    for (relative, bytes) in [(KERNEL_PATH, kernel), (STATUS_PATH, status)] {
        let destination = root.join(relative);
        let parent = destination.parent().context("payload has no parent")?;
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut temporary, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o644))?;
        }
        temporary.as_file().sync_all()?;
        temporary.persist(destination)?;
    }
    verify_contents(root, kernel, status)
}
