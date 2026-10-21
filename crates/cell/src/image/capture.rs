//! Bounded local mount capture. Source trees must be quiescent: metadata and
//! entry rechecks detect common mutations, but do not create an atomic snapshot.
//! No source paths survive capture and transport retries only see frozen bytes.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use ipfs::snapshot::CapturedEntry;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, Stat, CWD};
use sha2::{Digest, Sha256};

const MAX_OPERATIONS: usize = 128;
const MAX_DEPTH: usize = 128;
const MAX_NODES: usize = 100_000;
const MAX_VISITS: usize = 1_000_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 8 * 1024 * 1024;
const MAX_PATH: usize = 4096;
const READ_CHUNK: usize = 64 * 1024;
const EXCLUDED: [&str; 4] = ["target", ".git", ".gitignore", "Cargo.lock"];
// The permit lives inside the blocking worker, including after caller cancellation.
// DFS retains at most 129 tree descriptors plus one CWD and one enumeration
// descriptor. Source-component traversal retains two descriptors at a time.
// Both bounds are below the 256-descriptor capture limit.
static WORKER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// A source explicitly selected by the caller. Root mounts apply the legacy
/// exact-name exclusions at every depth; targeted sources include every entry.
/// Relative paths use one CWD descriptor acquired when the worker starts. A
/// caller requiring invocation-time CWD semantics must supply absolute paths.
#[derive(Clone, Debug)]
pub struct CaptureRequest {
    pub path: PathBuf,
    pub root_mount: bool,
}

/// Cooperative cancellation. An in-flight syscall cannot be forcibly stopped.
#[derive(Clone, Debug, Default)]
pub struct CaptureCancellation(Arc<AtomicBool>);
impl CaptureCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    fn check(&self) -> Result<()> {
        ensure!(!self.0.load(Ordering::Acquire), "local capture cancelled");
        Ok(())
    }
}

struct CancelOnDrop(Option<CaptureCancellation>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.0 {
            cancellation.cancel();
        }
    }
}

/// Aggregate counters remain charged after individual source buffers are freed.
/// `operations` counts local requests only. The caller must include authoritative
/// heads and immutable sources when enforcing the 128-operation resolution limit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptureStats {
    pub operations: usize,
    pub manifest_nodes: usize,
    pub entry_visits: usize,
    pub file_bytes: usize,
    pub path_bytes: usize,
}

#[derive(Debug)]
pub struct CapturedSource {
    entries: Vec<CapturedEntry>,
    digests: BTreeMap<String, [u8; 32]>,
}
impl CapturedSource {
    pub fn entries(&self) -> &[CapturedEntry] {
        &self.entries
    }
    pub fn digests(&self) -> &BTreeMap<String, [u8; 32]> {
        &self.digests
    }
}

#[derive(Debug)]
pub struct CaptureBatch {
    sources: Vec<CapturedSource>,
    stats: CaptureStats,
}
impl CaptureBatch {
    /// Captures the entire batch before any source can be published. Admission
    /// and worker completion use the existing filesystem transaction lifecycle.
    /// Dropping the future requests cancellation; scope completion still waits
    /// for the worker to close its descriptors. Calling `cancel` while awaiting
    /// this future returns only after that worker has stopped.
    pub async fn capture(
        requests: Vec<CaptureRequest>,
        transaction: &cache::ForegroundTransaction,
        cancellation: CaptureCancellation,
    ) -> Result<Self> {
        ensure!(
            requests.len() <= MAX_OPERATIONS,
            "capture source operation limit exceeded"
        );
        run_capture(transaction, cancellation, move |cancellation| {
            capture_sync(requests, cancellation, Limits::default())
        })
        .await
    }

    pub fn sources(&self) -> &[CapturedSource] {
        &self.sources
    }
    pub fn stats(&self) -> CaptureStats {
        self.stats
    }
    pub(super) fn into_sources(self) -> Vec<CapturedSource> {
        self.sources
    }
}

async fn run_capture(
    transaction: &cache::ForegroundTransaction,
    cancellation: CaptureCancellation,
    capture: impl FnOnce(CaptureCancellation) -> Result<CaptureBatch> + Send + 'static,
) -> Result<CaptureBatch> {
    let mut cancel_on_drop = CancelOnDrop(Some(cancellation.clone()));
    cancellation.check()?;
    let permit = WORKER
        .try_acquire()
        .context("another local capture worker is active")?;
    let result = transaction
        .run_blocking(cache::FilesystemOperation::FileStage, move || {
            let _permit = permit;
            capture(cancellation)
        })
        .await?;
    cancel_on_drop.0 = None;
    result.into_inner()
}

#[derive(Clone, Copy)]
struct Limits {
    nodes: usize,
    visits: usize,
    bytes: usize,
    paths: usize,
    path: usize,
    depth: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            nodes: MAX_NODES,
            visits: MAX_VISITS,
            bytes: MAX_BYTES,
            paths: MAX_PATH_BYTES,
            path: MAX_PATH,
            depth: MAX_DEPTH,
        }
    }
}

#[cfg(test)]
type ReadHook = Box<dyn FnMut(&str)>;

struct Capture {
    cwd: OwnedFd,
    stats: CaptureStats,
    limits: Limits,
    cancel: CaptureCancellation,
    #[cfg(test)]
    hook: Option<ReadHook>,
}

// Keep only fields relevant to identity and common mutation detection. In
// particular, atime can change because capture itself reads the file.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Stamp {
    dev: u64,
    ino: u64,
    mode: u32,
    size: i128,
    mtime: i128,
    mtime_ns: i128,
    ctime: i128,
    ctime_ns: i128,
}
// rustix stat field widths differ between Unix targets.
#[allow(clippy::unnecessary_cast)]
impl From<Stat> for Stamp {
    fn from(s: Stat) -> Self {
        Self {
            dev: s.st_dev as u64,
            ino: s.st_ino as u64,
            mode: s.st_mode as u32,
            size: s.st_size as i128,
            mtime: s.st_mtime as i128,
            mtime_ns: s.st_mtime_nsec as i128,
            ctime: s.st_ctime as i128,
            ctime_ns: s.st_ctime_nsec as i128,
        }
    }
}
struct Observed {
    stamp: Stamp,
    children: Option<Vec<String>>,
}
struct Pending {
    request: CaptureRequest,
    source: CapturedSource,
    observed: BTreeMap<String, Observed>,
    ancestors: Vec<(u64, u64, FileType)>,
}

fn charge(value: &mut usize, amount: usize, limit: usize, label: &str) -> Result<()> {
    let next = value
        .checked_add(amount)
        .context("capture budget overflow")?;
    ensure!(next <= limit, "capture {label} limit exceeded");
    *value = next;
    Ok(())
}

fn capture_sync(
    requests: Vec<CaptureRequest>,
    cancel: CaptureCancellation,
    limits: Limits,
) -> Result<CaptureBatch> {
    ensure!(
        requests.len() <= MAX_OPERATIONS,
        "capture source operation limit exceeded"
    );
    let mut capture = Capture {
        cwd: fs::openat(CWD, ".", directory_flags(), Mode::empty())?,
        stats: CaptureStats {
            operations: requests.len(),
            ..Default::default()
        },
        limits,
        cancel,
        #[cfg(test)]
        hook: None,
    };
    let mut pending = Vec::new();
    for request in requests {
        capture.cancel.check()?;
        let (fd, ancestors) = open_source(&request.path, &capture.cwd, &capture.cancel)?;
        let mut item = Pending {
            request,
            source: CapturedSource {
                entries: Vec::new(),
                digests: BTreeMap::new(),
            },
            observed: BTreeMap::new(),
            ancestors,
        };
        capture.walk(&fd, "", 0, &mut item)?;
        pending.push(item);
    }
    // Re-open with the same no-follow policy and compare every entry after all
    // source reads. Nothing from this pass becomes deployment state.
    for item in &pending {
        capture.cancel.check()?;
        let (fd, ancestors) = open_source(&item.request.path, &capture.cwd, &capture.cancel)?;
        ensure!(
            ancestors == item.ancestors,
            "source path changed during capture"
        );
        capture.recheck(&fd, "", item)?;
    }
    capture.cancel.check()?;
    Ok(CaptureBatch {
        sources: pending.into_iter().map(|item| item.source).collect(),
        stats: capture.stats,
    })
}

fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.bytes().any(|b| b == b'/' || b.is_ascii_control()),
        "invalid local capture name"
    );
    Ok(())
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}
fn leaf_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC
}

type AncestorIdentity = (u64, u64, FileType);

#[allow(clippy::unnecessary_cast)]
fn identity(stat: Stat) -> AncestorIdentity {
    (
        stat.st_dev as u64,
        stat.st_ino as u64,
        FileType::from_raw_mode(stat.st_mode),
    )
}

fn open_source(
    path: &Path,
    cwd: &OwnedFd,
    cancellation: &CaptureCancellation,
) -> Result<(OwnedFd, Vec<AncestorIdentity>)> {
    cancellation.check()?;
    let path = path.to_str().context("local source path is not UTF-8")?;
    ensure!(
        !path.is_empty() && path.len() <= 16 * 1024,
        "invalid local source path length"
    );
    ensure!(path != "./", "noncanonical local source path");
    let absolute = path.starts_with('/');
    let tail = if absolute {
        &path[1..]
    } else if path == "." {
        ""
    } else {
        path.strip_prefix("./").unwrap_or(path)
    };
    let names: Vec<_> = if tail.is_empty() {
        Vec::new()
    } else {
        tail.split('/').collect()
    };
    ensure!(
        names.len() <= MAX_DEPTH,
        "local source path depth limit exceeded"
    );
    for name in &names {
        valid_name(name)?;
    }
    let mut fd = fs::openat(
        cwd,
        if absolute { "/" } else { "." },
        directory_flags(),
        Mode::empty(),
    )?;
    let mut ancestors = vec![identity(fs::fstat(&fd)?)];
    for (index, name) in names.iter().enumerate() {
        cancellation.check()?;
        let before = fs::statat(&fd, *name, AtFlags::SYMLINK_NOFOLLOW)?;
        let kind = FileType::from_raw_mode(before.st_mode);
        ensure!(
            matches!(kind, FileType::Directory | FileType::RegularFile),
            "unsupported source component {name:?}; symlinks are not followed"
        );
        let flags = if index + 1 == names.len() && kind == FileType::RegularFile {
            leaf_flags()
        } else {
            directory_flags()
        };
        fd = fs::openat(&fd, *name, flags, Mode::empty()).with_context(|| {
            format!("cannot open source component {name:?} without following symlinks")
        })?;
        let after = fs::fstat(&fd)?;
        if index + 1 < names.len() {
            ensure!(
                identity(after) == identity(before),
                "source ancestor changed before open: {name:?}"
            );
            ancestors.push(identity(after));
        } else {
            ensure!(
                Stamp::from(after) == Stamp::from(before),
                "source changed before open: {name:?}"
            );
        }
    }
    Ok((fd, ancestors))
}

impl Capture {
    fn names(&mut self, fd: &OwnedFd) -> Result<Vec<String>> {
        // A fresh directory file description avoids shared readdir offsets.
        let scan = fs::openat(fd, ".", directory_flags(), Mode::empty())?;
        let mut dir = fs::Dir::new(scan)?;
        let mut names = Vec::new();
        let mut name_bytes = 0usize;
        for entry in &mut dir {
            self.cancel.check()?;
            let entry = entry?;
            let raw = entry.file_name().to_bytes();
            if raw == b"." || raw == b".." {
                continue;
            }
            charge(
                &mut self.stats.entry_visits,
                1,
                self.limits.visits,
                "entry visits",
            )?;
            let name = std::str::from_utf8(raw).context("local entry name is not UTF-8")?;
            valid_name(name)?;
            // Bound transient enumeration before copying names or sorting.
            charge(
                &mut name_bytes,
                name.len(),
                self.limits.paths,
                "enumeration bytes",
            )?;
            names.push(name.to_owned());
        }
        names.sort_unstable();
        Ok(names)
    }

    fn walk(&mut self, fd: &OwnedFd, path: &str, depth: usize, item: &mut Pending) -> Result<()> {
        self.cancel.check()?;
        ensure!(
            depth <= self.limits.depth,
            "capture entry depth limit exceeded"
        );
        ensure!(
            path.len() <= self.limits.path,
            "capture relative path limit exceeded"
        );
        charge(
            &mut self.stats.manifest_nodes,
            1,
            self.limits.nodes,
            "manifest nodes",
        )?;
        let before = fs::fstat(fd)?;
        // Retain one manifest key, one mutation-check key, and, for files,
        // one digest key. Charge all copies before allocating any of them.
        let copies = if FileType::from_raw_mode(before.st_mode) == FileType::RegularFile {
            3
        } else {
            2
        };
        charge(
            &mut self.stats.path_bytes,
            path.len() * copies,
            self.limits.paths,
            "stored path bytes",
        )?;
        let stamp = Stamp::from(before);
        match FileType::from_raw_mode(before.st_mode) {
            FileType::Directory => {
                let names = self.names(fd)?;
                item.source.entries.push(CapturedEntry {
                    path: path.to_owned(),
                    data: None,
                });
                for name in &names {
                    self.cancel.check()?;
                    if item.request.root_mount && EXCLUDED.contains(&name.as_str()) {
                        continue;
                    }
                    let child_len = path.len() + usize::from(!path.is_empty()) + name.len();
                    ensure!(
                        child_len <= self.limits.path,
                        "capture relative path limit exceeded"
                    );
                    ensure!(
                        depth < self.limits.depth,
                        "capture entry depth limit exceeded"
                    );
                    let child_path = if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{path}/{name}")
                    };
                    ensure!(
                        child_path.len() <= self.limits.path,
                        "capture relative path limit exceeded"
                    );
                    let entry_stat = fs::statat(fd, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)?;
                    let kind = FileType::from_raw_mode(entry_stat.st_mode);
                    ensure!(
                        matches!(kind, FileType::Directory | FileType::RegularFile),
                        "unsupported local entry {child_path:?}"
                    );
                    let flags = if kind == FileType::Directory {
                        directory_flags()
                    } else {
                        leaf_flags()
                    };
                    #[cfg(test)]
                    if let Some(hook) = &mut self.hook {
                        hook(&format!("before-open:{child_path}"));
                    }
                    let child =
                        fs::openat(fd, name.as_str(), flags, Mode::empty()).with_context(|| {
                            format!(
                                "cannot open local entry {child_path:?} without following symlinks"
                            )
                        })?;
                    ensure!(
                        Stamp::from(fs::fstat(&child)?) == Stamp::from(entry_stat),
                        "local entry changed before open: {child_path:?}"
                    );
                    self.walk(&child, &child_path, depth + 1, item)?;
                }
                ensure!(
                    self.names(fd)? == names,
                    "directory entries changed during capture: {path:?}"
                );
                ensure!(
                    Stamp::from(fs::fstat(fd)?) == stamp,
                    "directory changed during capture: {path:?}"
                );
                item.observed.insert(
                    path.to_owned(),
                    Observed {
                        stamp,
                        children: Some(names),
                    },
                );
            }
            FileType::RegularFile => {
                ensure!(
                    stamp.size >= 0
                        && stamp.size
                            <= self.limits.bytes.saturating_sub(self.stats.file_bytes) as i128,
                    "capture file bytes limit exceeded"
                );
                let mut bytes = Vec::new();
                let mut chunk = vec![0u8; READ_CHUNK];
                loop {
                    self.cancel.check()?;
                    let available = self.limits.bytes - self.stats.file_bytes;
                    // At the inclusive byte limit, the final metadata/length
                    // check detects growth without an over-budget read.
                    if available == 0 {
                        break;
                    }
                    let count = rustix::io::read(fd, &mut chunk[..READ_CHUNK.min(available)])?;
                    if count == 0 {
                        break;
                    }
                    charge(
                        &mut self.stats.file_bytes,
                        count,
                        self.limits.bytes,
                        "file bytes",
                    )?;
                    bytes
                        .try_reserve_exact(count)
                        .context("cannot allocate captured file bytes")?;
                    bytes.extend_from_slice(&chunk[..count]);
                    #[cfg(test)]
                    if let Some(hook) = &mut self.hook {
                        hook(path);
                    }
                }
                ensure!(
                    Stamp::from(fs::fstat(fd)?) == stamp && bytes.len() as i128 == stamp.size,
                    "file changed during capture: {path:?}"
                );
                item.source
                    .digests
                    .insert(path.to_owned(), Sha256::digest(&bytes).into());
                item.source.entries.push(CapturedEntry {
                    path: path.to_owned(),
                    data: Some(Bytes::from(bytes)),
                });
                item.observed.insert(
                    path.to_owned(),
                    Observed {
                        stamp,
                        children: None,
                    },
                );
            }
            _ => bail!("local source is neither a regular file nor a directory"),
        }
        Ok(())
    }

    fn recheck(&mut self, fd: &OwnedFd, path: &str, item: &Pending) -> Result<()> {
        self.cancel.check()?;
        charge(
            &mut self.stats.entry_visits,
            1,
            self.limits.visits,
            "entry visits",
        )?;
        let observed = &item.observed[path];
        ensure!(
            Stamp::from(fs::fstat(fd)?) == observed.stamp,
            "local entry changed before sealing: {path:?}"
        );
        if let Some(expected) = &observed.children {
            ensure!(
                &self.names(fd)? == expected,
                "directory entries changed before sealing: {path:?}"
            );
            for name in expected {
                if item.request.root_mount && EXCLUDED.contains(&name.as_str()) {
                    continue;
                }
                let child_path = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}/{name}")
                };
                let expected = &item.observed[&child_path];
                let flags = if expected.children.is_some() {
                    directory_flags()
                } else {
                    leaf_flags()
                };
                let child = fs::openat(fd, name.as_str(), flags, Mode::empty())?;
                self.recheck(&child, &child_path, item)?;
            }
            ensure!(
                &self.names(fd)? == expected,
                "directory entries changed while sealing: {path:?}"
            );
        }
        ensure!(
            Stamp::from(fs::fstat(fd)?) == observed.stamp,
            "local entry changed while sealing: {path:?}"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::os::unix::{ffi::OsStringExt, fs::symlink};

    pub(in crate::image) static ASYNC_CAPTURE_TEST: tokio::sync::Mutex<()> =
        tokio::sync::Mutex::const_new(());

    fn fixture() -> tempfile::TempDir {
        // macOS /var and /tmp aliases are deliberately rejected by capture.
        let base = std::env::temp_dir().canonicalize().unwrap();
        tempfile::tempdir_in(base).unwrap()
    }
    fn request(path: &Path, root_mount: bool) -> CaptureRequest {
        CaptureRequest {
            path: path.to_owned(),
            root_mount,
        }
    }
    fn capture(path: &Path, root_mount: bool) -> Result<CaptureBatch> {
        capture_sync(
            vec![request(path, root_mount)],
            CaptureCancellation::default(),
            Limits::default(),
        )
    }
    fn small_capture(path: &Path, limits: Limits) -> Result<CaptureBatch> {
        capture_sync(
            vec![request(path, false)],
            CaptureCancellation::default(),
            limits,
        )
    }

    #[test]
    fn exact_names_empty_directories_and_immutable_bytes() {
        let dir = fixture();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        std::fs::write(dir.path().join("a\\b é"), b"frozen").unwrap();
        let batch = capture(dir.path(), false).unwrap();
        std::fs::remove_file(dir.path().join("a\\b é")).unwrap();
        let source = &batch.sources()[0];
        assert_eq!(
            source
                .entries()
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["", "a\\b é", "empty"]
        );
        assert_eq!(
            source.entries()[1].data.as_deref(),
            Some(b"frozen".as_slice())
        );
        assert_eq!(source.entries()[2].data, None);
        assert_eq!(
            source.digests()["a\\b é"],
            <[u8; 32]>::from(Sha256::digest(b"frozen"))
        );
        assert_eq!(batch.stats().file_bytes, 6);
    }

    #[test]
    fn empty_root_and_standalone_file_are_manifest_roots() {
        let dir = fixture();
        let empty = capture(dir.path(), false).unwrap();
        assert_eq!(empty.sources()[0].entries().len(), 1);
        assert_eq!(empty.sources()[0].entries()[0].path, "");
        assert_eq!(empty.sources()[0].entries()[0].data, None);
        let file = dir.path().join("file");
        std::fs::write(&file, vec![7u8; READ_CHUNK + 3]).unwrap();
        let batch = capture(&file, false).unwrap();
        assert_eq!(batch.sources()[0].entries()[0].path, "");
        assert_eq!(
            batch.sources()[0].entries()[0].data.as_ref().unwrap().len(),
            READ_CHUNK + 3
        );
    }

    #[test]
    fn root_exclusions_apply_at_every_depth_before_open() {
        let dir = fixture();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        for name in EXCLUDED {
            symlink("absent", dir.path().join(name)).unwrap();
            std::fs::write(dir.path().join("nested").join(name), b"include").unwrap();
        }
        let root = capture(dir.path(), true).unwrap();
        assert_eq!(
            root.sources()[0]
                .entries()
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["", "nested"]
        );
        assert!(capture(dir.path(), false).is_err());
        for name in EXCLUDED {
            std::fs::remove_file(dir.path().join(name)).unwrap();
        }
        let targeted = capture(dir.path(), false).unwrap();
        assert_eq!(targeted.sources()[0].entries().len(), 6);
        assert!(targeted.sources()[0]
            .entries()
            .iter()
            .any(|e| e.path == "nested/Cargo.lock"));
    }

    #[test]
    fn rejects_source_intermediate_and_descendant_symlinks() {
        let dir = fixture();
        std::fs::create_dir(dir.path().join("actual")).unwrap();
        std::fs::write(dir.path().join("actual/file"), b"data").unwrap();
        symlink("actual", dir.path().join("alias")).unwrap();
        assert!(capture(&dir.path().join("alias"), false).is_err());
        assert!(capture(&dir.path().join("alias/file"), false).is_err());
        assert!(capture(dir.path(), false).is_err());
        std::fs::remove_file(dir.path().join("alias")).unwrap();
        symlink("missing", dir.path().join("dangling")).unwrap();
        assert!(capture(dir.path(), false).is_err());
    }

    #[test]
    fn rejects_fifo_socket_and_invalid_names() {
        let dir = fixture();
        let fifo = dir.path().join("fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        assert!(capture(&fifo, false).is_err());
        assert!(capture(dir.path(), false).is_err());
        std::fs::remove_file(&fifo).unwrap();
        let socket_path = dir.path().join("socket");
        let socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        assert!(capture(dir.path(), false).is_err());
        drop(socket);
        std::fs::remove_file(socket_path).unwrap();
        for name in [
            std::ffi::OsString::from("newline\n"),
            std::ffi::OsString::from_vec(vec![0xff]),
        ] {
            let path = dir.path().join(name);
            match std::fs::write(&path, []) {
                Ok(()) => {
                    assert!(capture(dir.path(), false).is_err());
                    std::fs::remove_file(path).unwrap();
                }
                Err(error) => {
                    // APFS rejects invalid UTF-8 before capture can enumerate it.
                    assert_eq!(error.raw_os_error(), Some(92));
                    assert!(capture(&path, false).is_err());
                }
            }
        }
    }

    #[test]
    fn byte_node_path_depth_and_visit_limits_are_inclusive() {
        let dir = fixture();
        let file = dir.path().join("a");
        std::fs::write(&file, b"1234").unwrap();
        let baseline = capture(dir.path(), false).unwrap().stats();
        let exact = Limits {
            nodes: 2,
            bytes: 4,
            paths: 3,
            path: 1,
            depth: 1,
            visits: baseline.entry_visits,
        };
        assert!(small_capture(dir.path(), exact).is_ok());
        for limits in [
            Limits { nodes: 1, ..exact },
            Limits { bytes: 3, ..exact },
            Limits { paths: 2, ..exact },
            Limits { path: 0, ..exact },
            Limits { depth: 0, ..exact },
            Limits {
                visits: exact.visits - 1,
                ..exact
            },
        ] {
            assert!(small_capture(dir.path(), limits).is_err());
        }
        let requests = vec![request(&file, false), request(&file, false)];
        assert!(capture_sync(
            requests.clone(),
            CaptureCancellation::default(),
            Limits {
                bytes: 8,
                ..Default::default()
            }
        )
        .is_ok());
        assert!(capture_sync(
            requests,
            CaptureCancellation::default(),
            Limits {
                bytes: 7,
                ..Default::default()
            }
        )
        .is_err());
        assert!(capture_sync(
            vec![request(&file, false); 128],
            CaptureCancellation::default(),
            Limits::default()
        )
        .is_ok());
        assert!(capture_sync(
            vec![request(&file, false); 129],
            CaptureCancellation::default(),
            Limits::default()
        )
        .is_err());
    }

    #[test]
    fn rejects_noncanonical_paths_and_allows_hardlinks() {
        let dir = fixture();
        let file = dir.path().join("file");
        std::fs::write(&file, b"data").unwrap();
        for suffix in ["/", "/./file", "//file", "/../file"] {
            assert!(capture(
                Path::new(&format!("{}{suffix}", dir.path().display())),
                false
            )
            .is_err());
        }
        std::fs::hard_link(&file, dir.path().join("hardlink")).unwrap();
        assert_eq!(capture(dir.path(), false).unwrap().stats().file_bytes, 8);
    }

    fn hooked_capture(
        path: &Path,
        limits: Limits,
        cancel: CaptureCancellation,
        hook: impl FnMut(&str) + 'static,
    ) -> Result<()> {
        let mut capture = Capture {
            cwd: fs::openat(CWD, ".", directory_flags(), Mode::empty())?,
            stats: CaptureStats::default(),
            limits,
            cancel,
            hook: Some(Box::new(hook)),
        };
        let (fd, ancestors) = open_source(path, &capture.cwd, &capture.cancel)?;
        let mut item = Pending {
            request: request(path, false),
            source: CapturedSource {
                entries: Vec::new(),
                digests: BTreeMap::new(),
            },
            observed: BTreeMap::new(),
            ancestors,
        };
        capture.walk(&fd, "", 0, &mut item)?;
        capture.recheck(&fd, "", &item)
    }

    #[test]
    fn rejects_growth_after_initial_stat_and_cancellation_between_chunks() {
        use std::io::Write;
        let dir = fixture();
        let file = dir.path().join("file");
        std::fs::write(&file, b"1234").unwrap();
        let mutation_path = file.clone();
        let result = hooked_capture(
            &file,
            Limits {
                bytes: 4,
                ..Default::default()
            },
            CaptureCancellation::default(),
            move |_| {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&mutation_path)
                    .unwrap()
                    .write_all(b"5")
                    .unwrap();
            },
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("changed"));
        std::fs::write(&file, vec![0u8; READ_CHUNK + 1]).unwrap();
        let cancellation = CaptureCancellation::default();
        let worker_cancel = cancellation.clone();
        let result = hooked_capture(&file, Limits::default(), cancellation, move |_| {
            worker_cancel.cancel()
        });
        assert!(format!("{:#}", result.unwrap_err()).contains("cancelled"));
    }

    #[test]
    fn rejects_entry_substitution_and_directory_mutation() {
        let dir = fixture();
        std::fs::write(dir.path().join("a"), b"a").unwrap();
        std::fs::write(dir.path().join("b"), b"b").unwrap();
        let target = dir.path().join("b");
        let result = hooked_capture(
            dir.path(),
            Limits::default(),
            CaptureCancellation::default(),
            move |name| {
                if name == "a" {
                    std::fs::remove_file(&target).unwrap();
                    symlink("a", &target).unwrap();
                }
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_regular_file_replaced_by_fifo_between_stat_and_open() {
        let dir = fixture();
        let target = dir.path().join("file");
        std::fs::write(&target, b"captured").unwrap();
        let result = hooked_capture(
            dir.path(),
            Limits::default(),
            CaptureCancellation::default(),
            move |stage| {
                if stage == "before-open:file" {
                    std::fs::remove_file(&target).unwrap();
                    assert!(std::process::Command::new("mkfifo")
                        .arg(&target)
                        .status()
                        .unwrap()
                        .success());
                }
            },
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("changed before open"));
    }

    #[test]
    fn rejects_same_size_rewrite_after_file_read() {
        let dir = fixture();
        let file = dir.path().join("file");
        std::fs::write(&file, b"before").unwrap();
        let changed = file.clone();
        let result = hooked_capture(
            &file,
            Limits::default(),
            CaptureCancellation::default(),
            move |_| {
                std::fs::write(&changed, b"after!").unwrap();
            },
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("changed"));
    }

    #[tokio::test]
    async fn existing_filesystem_lifecycle_runs_capture_and_releases_admission() {
        let _serial = ASYNC_CAPTURE_TEST.lock().await;
        let dir = fixture();
        std::fs::write(dir.path().join("file"), b"captured").unwrap();
        let (owner, host) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::default(),
        )
        .unwrap();
        let (scope, scope_owner, finalizer, _completion) = host.try_filesystem_scope().unwrap();
        let transaction = host
            .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
            .unwrap();
        let batch = CaptureBatch::capture(
            vec![request(dir.path(), false)],
            &transaction,
            CaptureCancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(batch.stats().file_bytes, 8);
        assert!(WORKER.try_acquire().is_ok());
        drop(transaction);
        drop(scope_owner);
        drop(finalizer);
        owner.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn cancelled_waiter_retains_worker_and_scope_until_descriptors_close() {
        let _serial = ASYNC_CAPTURE_TEST.lock().await;
        let dir = fixture();
        let (owner, host) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::default(),
        )
        .unwrap();
        let (scope, scope_owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
        let transaction = host
            .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
            .unwrap();
        let cancellation = CaptureCancellation::default();
        let worker_cancel = cancellation.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            run_capture(&transaction, worker_cancel, move |cancel| {
                let fd = fs::openat(CWD, dir.path(), directory_flags(), Mode::empty()).unwrap();
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                drop(fd);
                cancel.check()?;
                unreachable!("cancelled capture must fail")
            })
            .await
        });
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(cancellation.check().is_err());
        assert!(WORKER.try_acquire().is_err());
        drop(scope_owner);
        drop(finalizer);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), completion.wait())
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), completion.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(WORKER.try_acquire().is_ok());
        owner.shutdown().await.unwrap();
    }

    #[test]
    fn ancestor_sibling_mutations_do_not_change_selected_source_identity() {
        let dir = fixture();
        let selected = dir.path().join("selected");
        std::fs::create_dir(&selected).unwrap();
        let cwd = fs::openat(CWD, ".", directory_flags(), Mode::empty()).unwrap();
        let (_, before) = open_source(&selected, &cwd, &CaptureCancellation::default()).unwrap();
        std::fs::write(dir.path().join("unrelated"), b"other").unwrap();
        let (_, after) = open_source(&selected, &cwd, &CaptureCancellation::default()).unwrap();
        assert_eq!(before, after);
    }
    #[test]
    fn maximum_directory_depth_uses_bounded_descriptor_stack() {
        let dir = fixture();
        let mut deepest = dir.path().to_owned();
        for _ in 0..MAX_DEPTH {
            deepest.push("d");
            std::fs::create_dir(&deepest).unwrap();
        }
        assert_eq!(
            capture(dir.path(), false).unwrap().stats().manifest_nodes,
            MAX_DEPTH + 1
        );
        std::fs::write(deepest.join("too-deep"), b"x").unwrap();
        assert!(capture(dir.path(), false)
            .unwrap_err()
            .to_string()
            .contains("depth"));
    }
}
