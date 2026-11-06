//! Shared installation exclusion and existing version-1 ownership receipts.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Exclusive installation mutation guard.
pub struct MutationLock {
    // The open descriptor holds flock until Drop, including on error returns.
    _file: File,
    home: PathBuf,
}

impl MutationLock {
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Retain exclusion while an existing lifecycle command executes.
    pub fn inherit_to(&self, command: &mut std::process::Command) -> Result<()> {
        use std::os::unix::process::CommandExt;
        let file = self
            ._file
            .try_clone()
            .context("retain installation lock for lifecycle command")?;
        // SAFETY: the closure only calls async-signal-safe fcntl after fork.
        // Capturing File keeps the descriptor alive through spawn. Clearing
        // CLOEXEC in the child leaves the parent's descriptor policy intact.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

    pub fn acquire(home: &Path) -> Result<Self> {
        ensure!(home.is_absolute(), "installation HOME must be absolute");
        ensure!(
            std::fs::symlink_metadata(home)?.is_dir(),
            "installation HOME must be a directory without symlink traversal"
        );
        // Keep the inode outside the software tree: legacy software removal
        // must not unlink a held lock and let a second mutator acquire another.
        let path = home.join(".ww-installation.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)
            .with_context(|| format!("open installation lock {}", path.display()))?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no pointer arguments or ownership requirements.
        let uid = unsafe { libc::geteuid() };
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == uid
                && metadata.mode() & 0o022 == 0,
            "installation lock must be a private regular file owned by this user"
        );
        // SAFETY: file owns a valid descriptor; flock does not retain a pointer.
        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if status != 0 {
            return Err(std::io::Error::last_os_error())
                .context("installation mutation already in progress; retry after it completes");
        }
        Ok(Self {
            _file: file,
            home: home.to_owned(),
        })
    }

    pub fn reject_pending(&self) -> Result<()> {
        let root = self.home.join(".ww");
        match std::fs::symlink_metadata(&root) {
            Ok(metadata) => ensure!(
                metadata.is_dir(),
                "installation directory is not a directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inspect installation directory"),
        }
        match std::fs::symlink_metadata(root.join("host-upgrade.json")) {
            Ok(_) => anyhow::bail!(
                "pending Host upgrade; recover the recorded transaction before modifying the installation"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("inspect pending Host upgrade"),
        }
        Ok(())
    }
}

/// Existing five-artifact ownership receipt. Digests are explicitly BLAKE3;
/// they establish local ownership, never release authentication.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u8,
    pub hashes: [String; 5],
    pub previous: Option<[String; 5]>,
    pub complete: bool,
}

impl Receipt {
    /// Read known complete or incomplete receipts for the existing installer.
    pub fn read(ww_dir: &Path) -> Result<Option<Self>> {
        let path = ww_dir.join("installation.json");
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("read installation ownership metadata"),
        };
        ensure!(
            file.metadata()?.is_file(),
            "installation ownership metadata is not a regular file"
        );
        let mut bytes = Vec::new();
        (&mut file).take(65537).read_to_end(&mut bytes)?;
        Self::decode(&bytes).map(Some)
    }

    /// Parse the exact receipt bytes retained by a transaction, without rereading.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= 65536,
            "installation ownership metadata exceeds 64 KiB"
        );
        let state: Self = serde_json::from_slice(bytes)
            .context("invalid installation ownership metadata; back up and reinstall")?;
        ensure!(
            state.version == 1,
            "unsupported installation ownership metadata; use the current installer"
        );
        Ok(state)
    }

    /// Upgrades cannot relabel an interrupted multi-artifact installation.
    pub fn load(ww_dir: &Path) -> Result<Self> {
        let state = Self::read(ww_dir)?.context("installation ownership receipt is missing")?;
        ensure!(
            state.complete,
            "installation is incomplete; recover the original installation before upgrading"
        );
        Ok(state)
    }

    pub fn executable_digest(&self) -> &str {
        &self.hashes[0]
    }

    /// Preserve every field except the native executable ownership slot.
    pub fn with_executable_digest(&self, digest: String) -> Self {
        let mut next = self.clone();
        next.hashes[0] = digest;
        next
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reacquire_after_drop(home: &Path) -> Result<MutationLock> {
        // Another parallel test can fork between close and reacquire. CLOEXEC
        // releases unrelated inherited descriptors at exec, not at fork.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match MutationLock::acquire(home) {
                Ok(lock) => return Ok(lock),
                Err(error) if std::time::Instant::now() >= deadline => return Err(error),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
    }

    #[test]
    fn manager_child_retains_lock_when_parent_guard_drops() {
        let home = tempfile::tempdir().unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "read ignored"])
            .stdin(std::process::Stdio::piped());
        lock.inherit_to(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        drop(command);
        drop(lock);
        let contended = MutationLock::acquire(home.path()).is_err();
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(
            contended,
            "active manager child lost installation exclusion"
        );
        assert!(reacquire_after_drop(home.path()).is_ok());
    }

    #[test]
    fn exclusive_lock_survives_software_directory_removal() {
        let home = tempfile::tempdir().unwrap();
        let first = MutationLock::acquire(home.path()).unwrap();
        std::fs::create_dir(home.path().join(".ww")).unwrap();
        std::fs::remove_dir(home.path().join(".ww")).unwrap();
        assert!(MutationLock::acquire(home.path()).is_err());
        drop(first);
        assert!(reacquire_after_drop(home.path()).is_ok());
    }

    #[test]
    fn pending_upgrade_excludes_legacy_mutators() {
        let home = tempfile::tempdir().unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        lock.reject_pending().unwrap();
        std::fs::create_dir(home.path().join(".ww")).unwrap();
        std::fs::write(home.path().join(".ww/host-upgrade.json"), b"interrupted").unwrap();
        assert!(lock.reject_pending().is_err());
    }

    #[test]
    fn lock_symlink_does_not_touch_target() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("identity");
        std::fs::write(&target, b"private key sentinel").unwrap();
        std::os::unix::fs::symlink(&target, home.path().join(".ww-installation.lock")).unwrap();
        assert!(MutationLock::acquire(home.path()).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"private key sentinel");
    }

    #[test]
    fn receipt_preserves_other_ownership_and_requires_complete_known_format() {
        let home = tempfile::tempdir().unwrap();
        let receipt = Receipt {
            version: 1,
            hashes: std::array::from_fn(|n| format!("artifact-{n}")),
            previous: Some(std::array::from_fn(|n| format!("previous-{n}"))),
            complete: true,
        };
        let path = home.path().join("installation.json");
        let original = receipt.encode().unwrap();
        std::fs::write(&path, &original).unwrap();
        let updated = Receipt::load(home.path())
            .unwrap()
            .with_executable_digest("new-native".into());
        assert_eq!(updated.executable_digest(), "new-native");
        assert_eq!(updated.hashes[1..], receipt.hashes[1..]);
        assert_eq!(updated.previous, receipt.previous);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let mut pending = receipt.clone();
        pending.complete = false;
        std::fs::write(&path, pending.encode().unwrap()).unwrap();
        assert!(Receipt::load(home.path()).is_err());
        assert!(!Receipt::read(home.path()).unwrap().unwrap().complete);
        pending.version = 2;
        std::fs::write(&path, pending.encode().unwrap()).unwrap();
        assert!(Receipt::read(home.path()).is_err());
    }

    #[test]
    fn receipt_refuses_symlink_fifo_oversize_and_unknown_fields() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("installation.json");
        std::os::unix::fs::symlink("absent", &path).unwrap();
        assert!(Receipt::read(home.path()).is_err());
        std::fs::remove_file(&path).unwrap();
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: cpath is a valid terminated path; mode contains permission bits.
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert!(Receipt::read(home.path()).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, vec![b' '; 65537]).unwrap();
        assert!(Receipt::read(home.path())
            .unwrap_err()
            .to_string()
            .contains("64 KiB"));
        let receipt = Receipt {
            version: 1,
            hashes: std::array::from_fn(|_| "hash".into()),
            previous: None,
            complete: true,
        };
        let mut json = serde_json::to_value(receipt).unwrap();
        json["future"] = true.into();
        std::fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(Receipt::read(home.path()).is_err());
    }
}
