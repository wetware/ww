//! Descriptor and process primitives for native upgrade qualification.
//!
//! These types establish byte equality and sampled process identity, not release
//! authorization, startup admission, service membership, or readiness. A Linux
//! pidfd and start ticks survive exec, including exec of the same file. Consumers
//! must separately bind a fresh challenge, launch nonce, close-on-exec socket,
//! startup contract, and manager observations. No native adapter uses these
//! primitives until the complete platform contract is qualified.
//!
//! Deadlines are checked around bounded reads and observations. They cannot
//! cancel a filesystem syscall already in progress.

use anyhow::{ensure, Context, Result};
use sha2::Digest;
use std::fs::{File, Metadata};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::time::Instant;

use super::{storage, ArtifactIdentity, DigestAlgorithm};

/// Identity of the held executable inode. Paths are not part of this identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutableFileIdentity {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
}

#[derive(PartialEq, Eq)]
struct FileObservation {
    identity: ExecutableFileIdentity,
    uid: u32,
    gid: u32,
    mode: u32,
    links: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileObservation {
    fn read(file: &File) -> Result<Self> {
        let metadata = file.metadata().context("inspect executable descriptor")?;
        // A running executable can have been unlinked during replacement. More
        // than one link remains outside the managed installation custody rules.
        // SAFETY: geteuid takes no pointers.
        let uid = unsafe { libc::geteuid() };
        ensure!(
            metadata.is_file()
                && metadata.uid() == uid
                && metadata.nlink() <= 1
                && metadata.mode() & 0o022 == 0
                && metadata.mode() & 0o111 != 0,
            "executable descriptor must identify an owned executable regular file without hardlinks or other-user writes"
        );
        Ok(Self {
            identity: file_identity(&metadata),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.mode(),
            links: metadata.nlink(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

fn file_identity(metadata: &Metadata) -> ExecutableFileIdentity {
    ExecutableFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
    }
}

/// Full-byte-verified read-only descriptor; not an authenticated release proof.
///
/// Fields are private and this type cannot be deserialized. Construction checks
/// every byte against the supplied identity. The caller must independently
/// authenticate that identity before using the descriptor for upgrade admission.
#[derive(Debug)]
pub struct VerifiedExecutable {
    file: File,
    artifact: ArtifactIdentity,
    identity: ExecutableFileIdentity,
}

impl VerifiedExecutable {
    pub fn open(path: &Path, expected: &ArtifactIdentity, deadline: Instant) -> Result<Self> {
        check_deadline(deadline)?;
        let file = storage::open_read(path)?;
        Self::from_file(file, expected, deadline)
    }

    fn from_file(file: File, expected: &ArtifactIdentity, deadline: Instant) -> Result<Self> {
        check_deadline(deadline)?;
        expected.validate()?;
        let observed = FileObservation::read(&file)?;
        let verified = Self {
            file,
            artifact: expected.clone(),
            identity: observed.identity,
        };
        verified.revalidate(deadline)?;
        Ok(verified)
    }

    pub fn artifact(&self) -> &ArtifactIdentity {
        &self.artifact
    }

    /// Borrow the pinned read-only descriptor for deliberate Linux preflight
    /// inheritance. The spawn owner must reattest the child after execution.
    #[cfg(target_os = "linux")]
    pub(crate) fn descriptor(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.file.as_fd()
    }

    pub fn file_identity(&self) -> &ExecutableFileIdentity {
        &self.identity
    }

    /// Rehash the held descriptor. This does not prove that a process still
    /// executes this inode; use `PinnedProcess::revalidate` for that observation.
    pub fn revalidate(&self, deadline: Instant) -> Result<()> {
        check_deadline(deadline)?;
        let before = FileObservation::read(&self.file)?;
        ensure!(
            before.identity == self.identity && before.identity.size == self.artifact.size,
            "executable descriptor identity or size changed"
        );
        let mut sha256 = sha2::Sha256::new();
        let mut blake3 = blake3::Hasher::new();
        let mut offset = 0;
        let mut buffer = [0; 64 * 1024];
        loop {
            check_deadline(deadline)?;
            let remaining = (self.artifact.size + 1).saturating_sub(offset);
            ensure!(remaining != 0, "executable exceeds expected size");
            let limit = usize::try_from(remaining.min(buffer.len() as u64))?;
            let count = match self.file.read_at(&mut buffer[..limit], offset) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result.context("read executable descriptor")?,
            };
            if count == 0 {
                break;
            }
            match self.artifact.algorithm {
                DigestAlgorithm::Sha256 => sha256.update(&buffer[..count]),
                DigestAlgorithm::Blake3 => {
                    blake3.update(&buffer[..count]);
                }
            }
            offset += count as u64;
        }
        let digest = match self.artifact.algorithm {
            DigestAlgorithm::Sha256 => hex::encode(sha256.finalize()),
            DigestAlgorithm::Blake3 => blake3.finalize().to_hex().to_string(),
        };
        ensure!(
            offset == self.artifact.size && digest == self.artifact.digest,
            "executable descriptor bytes differ from expected artifact"
        );
        ensure!(
            FileObservation::read(&self.file)? == before,
            "executable descriptor metadata changed during verification"
        );
        check_deadline(deadline)
    }
}

/// Process-birth evidence. This token is unchanged by same-process exec.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProcessStartToken {
    Linux { boot_id: String, start_ticks: u64 },
}

/// A pinned Linux process and its independently measured executable descriptor.
///
/// This type deliberately supplies no `RunningEvidence` or readiness constructor.
/// macOS capture refuses until its separate main-image mechanism is qualified.
#[derive(Debug)]
pub struct PinnedProcess {
    pid: u32,
    start: ProcessStartToken,
    executable: VerifiedExecutable,
    #[cfg(target_os = "linux")]
    pidfd: File,
}

impl PinnedProcess {
    pub fn capture(pid: u32, expected: &ArtifactIdentity, deadline: Instant) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            check_deadline(deadline)?;
            ensure!(pid != 0 && pid <= i32::MAX as u32, "invalid process ID");
            // SAFETY: pidfd_open takes an integer PID and zero flags. On success
            // the returned descriptor is uniquely owned and has close-on-exec.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
            ensure!(
                raw >= 0,
                "pidfd_open unavailable: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: successful pidfd_open returned a new owned descriptor.
            let pidfd = unsafe { File::from_raw_fd(raw as libc::c_int) };
            linux::alive(&pidfd)?;
            let start = linux::start_token(pid, deadline)?;
            let executable = Self::read_executable(pid, expected, deadline)?;
            let pinned = Self {
                pid,
                start,
                executable,
                pidfd,
            };
            pinned.revalidate(deadline)?;
            Ok(pinned)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (pid, expected, deadline);
            anyhow::bail!("exact running-executable evidence is not qualified on this platform")
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn executable(&self) -> &VerifiedExecutable {
        &self.executable
    }

    pub fn start_token(&self) -> &ProcessStartToken {
        &self.start
    }

    /// Reobserve the process and reopen `/proc/<pid>/exe`; the old executable FD
    /// alone survives both pathname replacement and exec into another image.
    /// Same-file exec remains indistinguishable without the evidence protocol.
    pub fn revalidate(&self, deadline: Instant) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.revalidate_birth(deadline)?;
            self.executable.revalidate(deadline)?;
            let current = Self::read_executable(self.pid, self.executable.artifact(), deadline)?;
            ensure!(
                current.file_identity() == self.executable.file_identity(),
                "process executable inode changed"
            );
            self.revalidate_birth(deadline)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = deadline;
            anyhow::bail!("exact running-executable evidence is not qualified on this platform")
        }
    }

    /// Check the connected socket's kernel peer credentials and sampled process
    /// identity. The caller still owns nonce, contract, readiness, and manager checks.
    pub fn verify_peer(
        &self,
        socket: &std::os::unix::net::UnixStream,
        deadline: Instant,
    ) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            self.revalidate(deadline)?;
            let mut credentials = libc::ucred {
                pid: 0,
                uid: 0,
                gid: 0,
            };
            let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            // SAFETY: the writable credentials buffer and size match ucred;
            // socket owns a live Unix socket descriptor for the call duration.
            let result = unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut size,
                )
            };
            ensure!(
                result == 0,
                "read socket peer credentials: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: geteuid takes no pointers.
            let uid = unsafe { libc::geteuid() };
            ensure!(
                size as usize == std::mem::size_of::<libc::ucred>()
                    && credentials.pid > 0
                    && credentials.pid as u32 == self.pid
                    && credentials.uid == uid,
                "evidence socket peer differs from pinned process or owner"
            );
            self.revalidate(deadline)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (socket, deadline);
            anyhow::bail!("socket process evidence is not qualified on this platform")
        }
    }

    #[cfg(target_os = "linux")]
    fn read_executable(
        pid: u32,
        expected: &ArtifactIdentity,
        deadline: Instant,
    ) -> Result<VerifiedExecutable> {
        use std::os::unix::fs::OpenOptionsExt;
        check_deadline(deadline)?;
        // /proc/<pid>/exe is a kernel magic link. O_NOFOLLOW would reject the
        // evidence source; opening it deliberately obtains the process's inode.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(format!("/proc/{pid}/exe"))
            .context("open process executable through procfs")?;
        VerifiedExecutable::from_file(file, expected, deadline)
    }

    #[cfg(target_os = "linux")]
    fn revalidate_birth(&self, deadline: Instant) -> Result<()> {
        check_deadline(deadline)?;
        linux::alive(&self.pidfd)?;
        ensure!(
            linux::start_token(self.pid, deadline)? == self.start,
            "process birth identity changed"
        );
        linux::alive(&self.pidfd)?;
        check_deadline(deadline)
    }
}

fn check_deadline(deadline: Instant) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "native executable observation deadline expired"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    pub(super) fn alive(pidfd: &File) -> Result<()> {
        let mut poll = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll points to one initialized pollfd; zero timeout cannot wait.
        let result = unsafe { libc::poll(&mut poll, 1, 0) };
        ensure!(
            result >= 0,
            "observe pidfd: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            result == 0 && poll.revents == 0,
            "pinned process exited or pidfd is invalid"
        );
        Ok(())
    }

    fn read_bounded(path: &Path, limit: usize, deadline: Instant) -> Result<String> {
        check_deadline(deadline)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .with_context(|| format!("read process identity {}", path.display()))?;
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= limit,
            "process identity exceeds observation bound"
        );
        check_deadline(deadline)?;
        String::from_utf8(bytes).context("process identity is not UTF-8")
    }

    pub(super) fn start_token(pid: u32, deadline: Instant) -> Result<ProcessStartToken> {
        let boot = read_bounded(Path::new("/proc/sys/kernel/random/boot_id"), 64, deadline)?;
        let boot_id = boot.trim();
        ensure!(
            boot_id.len() == 36
                && boot_id.bytes().enumerate().all(|(index, byte)| {
                    if [8, 13, 18, 23].contains(&index) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                    }
                }),
            "invalid Linux boot identity"
        );
        let stat = read_bounded(Path::new(&format!("/proc/{pid}/stat")), 64 * 1024, deadline)?;
        let (prefix, fields) = stat
            .rsplit_once(')')
            .context("malformed process stat command")?;
        ensure!(
            prefix.starts_with(&format!("{pid} (")),
            "process stat PID differs"
        );
        // comm may contain spaces and ')'; fields following the final ')' start
        // at field 3 (state). starttime is field 22, hence zero-based index 19.
        let start_ticks = fields
            .split_whitespace()
            .nth(19)
            .context("missing process start ticks")?
            .parse()
            .context("invalid process start ticks")?;
        Ok(ProcessStartToken::Linux {
            boot_id: boot_id.to_owned(),
            start_ticks,
        })
    }
}
