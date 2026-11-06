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

pub(super) fn directory(path: &Path) -> Result<()> {
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

pub(super) fn exists(path: &Path) -> Result<bool> {
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

pub(super) fn open_read(path: &Path) -> Result<File> {
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

pub(super) fn read_record(path: &Path) -> Result<Option<Vec<u8>>> {
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

pub(super) fn write_record(path: &Path, bytes: &[u8]) -> Result<()> {
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

pub(super) fn rename(from: &Path, to: &Path) -> Result<()> {
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

pub(super) fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("transaction file parent")?;
    directory(parent)?;
    checkpoint("directory.sync.before")?;
    File::open(parent)?
        .sync_all()
        .with_context(|| format!("sync directory {}", parent.display()))?;
    checkpoint("directory.sync.after")?;
    Ok(())
}

pub(super) fn remove(path: &Path) -> Result<()> {
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
