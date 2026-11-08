//! Fixed-path durable I/O for the native Host transaction.

use anyhow::{ensure, Context, Result};
use sha2::Digest;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::{ArtifactIdentity, DigestAlgorithm};

pub(super) const MAX_RECORD: usize = 64 * 1024;
pub(super) const MAX_EXECUTABLE: u64 = 1024 * 1024 * 1024;

pub(super) struct Paths {
    pub executable: PathBuf,
    pub journal: PathBuf,
    pub retained: PathBuf,
    pub receipt: PathBuf,
    pub store: PathBuf,
    pub locator: PathBuf,
    pub proofs: PathBuf,
}

impl Paths {
    pub fn open(home: &Path) -> Result<Self> {
        let root = home.join(".ww");
        directory(home)?;
        directory(&root)?;
        directory(&root.join("bin"))?;
        let store = root.join("bin/.ww-host-upgrade");
        if !exists(&store)? {
            checkpoint("store.mkdir.before")?;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&store)
                .context("create Host recovery directory")?;
            checkpoint("store.mkdir.after")?;
        }
        directory(&store)?;
        // Reopening must complete a directory creation interrupted before its
        // parent sync; existence alone is not evidence of durable admission.
        sync_parent(&store)?;
        Ok(Self {
            executable: root.join("bin/ww"),
            journal: root.join("host-upgrade.json"),
            retained: root.join("host-upgrade-retained.json"),
            receipt: root.join("installation.json"),
            locator: root.join("installed-release.json"),
            proofs: store.join("proofs"),
            store,
        })
    }

    /// Open existing metadata paths without creating or syncing directories.
    pub(super) fn existing(home: &Path) -> Result<Self> {
        let root = home.join(".ww");
        let store = root.join("bin/.ww-host-upgrade");
        for path in [
            home,
            root.as_path(),
            root.join("bin").as_path(),
            store.as_path(),
        ] {
            directory(path)?;
        }
        Ok(Self {
            executable: root.join("bin/ww"),
            journal: root.join("host-upgrade.json"),
            retained: root.join("host-upgrade-retained.json"),
            receipt: root.join("installation.json"),
            locator: root.join("installed-release.json"),
            proofs: store.join("proofs"),
            store,
        })
    }

    pub fn artifact(&self, identity: &ArtifactIdentity) -> PathBuf {
        let algorithm = match identity.algorithm {
            DigestAlgorithm::Sha256 => "sha256",
            DigestAlgorithm::Blake3 => "blake3",
        };
        self.store.join(format!(
            "{algorithm}-{}-{}.ww",
            identity.digest, identity.size
        ))
    }
}

pub fn directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect directory {}", path.display()))?;
    // SAFETY: geteuid has no pointer arguments.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o022 == 0,
        "{} must be a directory owned by this user without symlinks or other-user writes",
        path.display()
    );
    Ok(())
}

pub fn exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn regular(file: &File, path: &Path) -> Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no pointer arguments.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == uid
            && metadata.mode() & 0o022 == 0,
        "{} must be a regular file owned by this user without hardlinks or other-user writes",
        path.display()
    );
    Ok(())
}

pub fn open_read(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    regular(&file, path)?;
    Ok(file)
}

fn open_write(path: &Path, executable: bool) -> Result<File> {
    // Validate before truncation: O_NOFOLLOW alone does not reject hardlinks.
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    regular(&file, path)?;
    file.set_len(0)?;
    file.set_permissions(std::fs::Permissions::from_mode(if executable {
        0o755
    } else {
        0o600
    }))?;
    Ok(file)
}

pub fn read_record(path: &Path) -> Result<Option<Vec<u8>>> {
    if !exists(path)? {
        return Ok(None);
    }
    let file = open_read(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_RECORD,
        "{} exceeds the 64 KiB record limit",
        path.display()
    );
    Ok(Some(bytes))
}

pub(super) fn read_private_record(path: &Path) -> Result<Option<Vec<u8>>> {
    if !exists(path)? {
        return Ok(None);
    }
    let file = open_read(path)?;
    ensure!(
        file.metadata()?.mode() & 0o177 == 0,
        "guarded record must be private and non-executable"
    );
    let mut bytes = Vec::new();
    file.take(MAX_RECORD as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_RECORD, "guarded record exceeds 64 KiB");
    Ok(Some(bytes))
}

pub fn write_record(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= MAX_RECORD,
        "transaction record exceeds 64 KiB"
    );
    if exists(path)? {
        open_read(path)?;
    }
    let temporary = path.with_file_name(format!(
        ".{}.pending",
        path.file_name()
            .context("record filename")?
            .to_string_lossy()
    ));
    checkpoint("record.open.before")?;
    let mut file = open_write(&temporary, false)?;
    checkpoint("record.open.after")?;
    file.write_all(bytes)?;
    checkpoint("record.write.after")?;
    file.sync_all()?;
    checkpoint("record.sync.after")?;
    rename(&temporary, path)?;
    Ok(())
}

pub fn rename(from: &Path, to: &Path) -> Result<()> {
    open_read(from)?;
    if exists(to)? {
        open_read(to)?;
    }
    checkpoint("rename.before")?;
    std::fs::rename(from, to).with_context(|| format!("replace {}", to.display()))?;
    checkpoint("rename.after")?;
    sync_parent(to)?;
    // These paths normally share a parent. Moving across directories must also
    // persist removal of the source name.
    if from.parent() != to.parent() {
        sync_parent(from)?;
    }
    Ok(())
}

pub fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("transaction file parent")?;
    directory(parent)?;
    checkpoint("directory.sync.before")?;
    File::open(parent)?
        .sync_all()
        .with_context(|| format!("sync directory {}", parent.display()))?;
    checkpoint("directory.sync.after")?;
    Ok(())
}

pub fn remove(path: &Path) -> Result<()> {
    if !exists(path)? {
        return Ok(());
    }
    open_read(path)?;
    checkpoint("remove.before")?;
    std::fs::remove_file(path)
        .with_context(|| format!("remove transaction file {}", path.display()))?;
    checkpoint("remove.after")?;
    sync_parent(path)
}

struct Hashes {
    sha256: sha2::Sha256,
    blake3: blake3::Hasher,
    size: u64,
}

impl Hashes {
    fn new() -> Self {
        Self {
            sha256: sha2::Sha256::new(),
            blake3: blake3::Hasher::new(),
            size: 0,
        }
    }
    fn update(&mut self, bytes: &[u8]) {
        self.sha256.update(bytes);
        self.blake3.update(bytes);
        self.size += bytes.len() as u64;
    }
    fn finish(self, expected: &ArtifactIdentity) -> Result<String> {
        let blake3 = self.blake3.finalize().to_hex().to_string();
        let digest = match expected.algorithm {
            DigestAlgorithm::Sha256 => hex::encode(self.sha256.finalize()),
            DigestAlgorithm::Blake3 => blake3.clone(),
        };
        ensure!(
            self.size == expected.size,
            "native artifact size mismatch: expected {}, got {}",
            expected.size,
            self.size
        );
        ensure!(
            digest == expected.digest,
            "native artifact {:?} digest mismatch",
            expected.algorithm
        );
        Ok(blake3)
    }
}

pub(super) fn verify(path: &Path, expected: &ArtifactIdentity) -> Result<String> {
    expected.validate()?;
    let mut reader = open_read(path)?.take(expected.size + 1);
    let mut hashes = Hashes::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hashes.update(&buffer[..count]);
    }
    hashes
        .finish(expected)
        .with_context(|| format!("verify {}", path.display()))
}

pub(super) fn stage(
    mut reader: impl Read,
    path: &Path,
    expected: &ArtifactIdentity,
) -> Result<String> {
    expected.validate()?;
    checkpoint("stage.open.before")?;
    let mut file = open_write(path, true)?;
    checkpoint("stage.open.after")?;
    let mut hashes = Hashes::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let remaining = (expected.size + 1).saturating_sub(hashes.size);
        ensure!(remaining > 0, "native artifact exceeds selected size");
        let limit = usize::try_from(remaining.min(buffer.len() as u64))?;
        let count = reader
            .read(&mut buffer[..limit])
            .context("read selected native artifact")?;
        if count == 0 {
            break;
        }
        file.write_all(&buffer[..count])?;
        hashes.update(&buffer[..count]);
        checkpoint("stage.write.after")?;
    }
    let digest = hashes.finish(expected)?;
    file.sync_all()?;
    checkpoint("stage.sync.after")?;
    sync_parent(path)?;
    Ok(digest)
}

pub(super) fn checkpoint(label: &str) -> Result<()> {
    #[cfg(test)]
    return faults::checkpoint(label);
    #[cfg(not(test))]
    {
        let _ = label;
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod faults {
    use anyhow::{bail, Result};
    use std::cell::RefCell;
    use std::time::{Duration, Instant};
    #[derive(Default)]
    struct State {
        fail_at: Option<usize>,
        exit_process: bool,
        trace: Vec<String>,
        advance_at: Option<usize>,
        elapsed: Duration,
    }
    thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }
    pub fn configure(fail_at: Option<usize>) {
        STATE.with(|state| {
            *state.borrow_mut() = State {
                fail_at,
                exit_process: false,
                ..State::default()
            }
        });
    }
    pub fn exit_at(point: usize) {
        STATE.with(|state| {
            *state.borrow_mut() = State {
                fail_at: Some(point),
                exit_process: true,
                ..State::default()
            }
        });
    }
    pub fn advance_clock_at(point: usize) {
        STATE.with(|state| {
            *state.borrow_mut() = State {
                advance_at: Some(point),
                ..State::default()
            };
        });
    }
    pub fn clock_now(now: Instant) -> Instant {
        STATE.with(|state| now + state.borrow().elapsed)
    }
    pub fn trace() -> Vec<String> {
        STATE.with(|state| state.borrow().trace.clone())
    }
    pub fn checkpoint(label: &str) -> Result<()> {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.trace.push(label.to_owned());
            if state.advance_at == Some(state.trace.len()) {
                state.elapsed = Duration::from_secs(60);
            }
            if state.fail_at == Some(state.trace.len()) {
                if state.exit_process {
                    std::process::exit(86);
                }
                bail!(
                    "injected process interruption at {}: {label}",
                    state.trace.len()
                );
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod proof_tests {
    use super::*;

    #[test]
    fn proof_storage_is_bounded_immutable_and_private() {
        faults::configure(None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proof");
        write_proof(&path, b"retained signed envelope").unwrap();
        assert_eq!(read_proof(&path).unwrap(), b"retained signed envelope");
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        write_proof(&path, b"retained signed envelope").unwrap();
        assert!(write_proof(&path, b"different signed envelope").is_err());
        assert_eq!(read_proof(&path).unwrap(), b"retained signed envelope");
        assert!(write_proof(&dir.path().join("too-large"), &vec![0; MAX_PROOF + 1]).is_err());
        assert!(!dir.path().join("too-large").exists());
        let large = dir.path().join("oversize-read");
        std::fs::write(&large, vec![0; MAX_PROOF + 1]).unwrap();
        std::fs::set_permissions(&large, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_proof(&large).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_proof(&path).is_err());
    }

    #[test]
    fn proof_storage_refuses_symlinks_and_hardlinks_without_target_changes() {
        use std::os::unix::fs::symlink;
        faults::configure(None);
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        write_proof(&original, b"preserved").unwrap();
        let link = dir.path().join("link");
        symlink(&original, &link).unwrap();
        assert!(write_proof(&link, b"overwrite").is_err());
        assert!(read_proof(&link).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::hard_link(&original, &link).unwrap();
        assert!(write_proof(&link, b"overwrite").is_err());
        assert!(read_proof(&link).is_err());
        assert_eq!(std::fs::read(original).unwrap(), b"preserved");
    }
}

const MAX_PROOF: usize = 8 * 1024 * 1024;

pub(super) fn read_proof(path: &Path) -> Result<Vec<u8>> {
    let file = open_read(path)?;
    ensure!(
        file.metadata()?.mode() & 0o177 == 0,
        "retained proof must be private and non-executable"
    );
    let mut bytes = Vec::new();
    file.take(MAX_PROOF as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_PROOF, "retained proof exceeds 8 MiB");
    Ok(bytes)
}

/// Persist one immutable, journal-owned envelope. The transaction must first
/// record ownership of this exact proof reference. Existing evidence is never
/// overwritten merely because it has the same transport-root filename.
pub(super) fn write_proof(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_PROOF,
        "retained proof must contain at most 8 MiB"
    );
    if exists(path)? {
        ensure!(
            read_proof(path)? == bytes,
            "existing retained proof has different bytes"
        );
        // A prior process may have died after rename but before parent sync.
        open_read(path)?.sync_all()?;
        checkpoint("proof.reuse.sync.after")?;
        sync_parent(path)?;
        return Ok(());
    }
    let temporary = path.with_file_name(format!(
        ".{}.pending",
        path.file_name()
            .context("proof filename")?
            .to_string_lossy()
    ));
    if exists(&temporary)? {
        let file = open_read(&temporary)?;
        ensure!(
            file.metadata()?.mode() & 0o177 == 0,
            "pending proof must be private and non-executable"
        );
    }
    checkpoint("proof.open.before")?;
    let mut file = open_write(&temporary, false)?;
    checkpoint("proof.open.after")?;
    file.write_all(bytes)?;
    checkpoint("proof.write.after")?;
    file.sync_all()?;
    checkpoint("proof.sync.after")?;
    rename(&temporary, path)?;
    ensure!(read_proof(path)? == bytes, "persisted proof changed");
    Ok(())
}

impl Paths {
    pub(super) fn proof(&self, reference: &super::provenance::ProofReference) -> Result<PathBuf> {
        reference.validate()?;
        Ok(self.proofs.join(&reference.root))
    }

    /// Called only after initialization intent or the transaction journal owns
    /// the reference. Directory creation alone grants no release authority.
    pub(super) fn create_proof_directory(&self) -> Result<()> {
        if !exists(&self.proofs)? {
            checkpoint("proof.directory.before")?;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&self.proofs)?;
            checkpoint("proof.directory.after")?;
        }
        directory(&self.proofs)?;
        ensure!(
            std::fs::symlink_metadata(&self.proofs)?.mode() & 0o077 == 0,
            "retained proof directory must be private"
        );
        sync_parent(&self.proofs)
    }
}
