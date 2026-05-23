//! WASI filesystem interceptor for `/ipfs/` paths and CidTree-backed virtual FS.
//!
//! When a `CidTree` is present (virtual mode), ALL filesystem operations resolve
//! paths lazily through the content-addressed tree. File content is materialized
//! to a staging directory on demand, then opened as a real `cap-std` file
//! descriptor so all subsequent reads delegate to wasmtime-wasi's standard impl.
//!
//! When no CidTree is present, falls back to the original behavior: intercepts
//! only explicit `/ipfs/<CID>/…` paths via the pinset cache.

use std::sync::Arc;

use crate::proc::ComponentRunStates;
use crate::vfs::{CidTree, ResolvedNode};
use anyhow::Result;
use wasmtime::component::{HasData, Linker, Resource};
use wasmtime_wasi::filesystem::{WasiFilesystemCtx, WasiFilesystemCtxView};
use wasmtime_wasi::p2::bindings::filesystem::{preopens, types};
use wasmtime_wasi::p2::{FsError, FsResult};
use wasmtime_wasi_io::streams::{DynInputStream, DynOutputStream};

// ── Marker type for HasData ────────────────────────────────────────

pub(crate) struct IpfsFilesystem;

impl HasData for IpfsFilesystem {
    type Data<'a> = IpfsFilesystemView<'a>;
}

// ── View type: wraps WasiFilesystemCtxView + cache + CidTree ──────

pub(crate) struct IpfsFilesystemView<'a> {
    pub ctx: &'a mut WasiFilesystemCtx,
    pub table: &'a mut wasmtime::component::ResourceTable,
    pub cache_mode: &'a Option<cache::CacheMode>,
    pub cid_tree: &'a Option<Arc<CidTree>>,
}

impl IpfsFilesystemView<'_> {
    /// Construct a temporary `WasiFilesystemCtxView` for delegation.
    fn as_wasi_view(&mut self) -> WasiFilesystemCtxView<'_> {
        WasiFilesystemCtxView {
            ctx: &mut *self.ctx,
            table: &mut *self.table,
        }
    }
}

// ── Accessor function ──────────────────────────────────────────────

fn ipfs_filesystem(state: &mut ComponentRunStates) -> IpfsFilesystemView<'_> {
    // Split borrow across distinct fields of ComponentRunStates.
    IpfsFilesystemView {
        ctx: state.wasi_ctx.filesystem(),
        table: &mut state.resource_table,
        cache_mode: &state.cache_mode,
        cid_tree: &state.cid_tree,
    }
}

// ── CID path parsing ───────────────────────────────────────────────

/// Parsed IPFS path: CID + optional subpath.
pub(crate) struct IpfsCidPath {
    pub cid: cid::Cid,
    pub subpath: String,
}

/// Parse a relative path like `ipfs/QmHash/sub/file` into CID + subpath.
/// Returns None if the path doesn't start with `ipfs/` or contains path
/// traversal components (`..`).
pub(crate) fn parse_ipfs_path(path: &str) -> Option<IpfsCidPath> {
    let rest = path.strip_prefix("ipfs/")?;
    let (cid_str, subpath) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx + 1..]),
        None => (rest, ""),
    };

    // Reject path traversal: any ".." component could escape the staging directory.
    if subpath.split('/').any(|seg| seg == "..") {
        return None;
    }

    let cid = cid_str.parse::<cid::Cid>().ok()?;
    Some(IpfsCidPath {
        cid,
        subpath: subpath.to_string(),
    })
}

// ── CidTree-backed open ───────────────────────────────────────────

impl IpfsFilesystemView<'_> {
    /// Handle an `open_at` for a path resolved through the CidTree.
    ///
    /// Resolves the path to a CID (or local override), fetches the content
    /// to staging if needed, and opens a real cap-std file descriptor.
    async fn open_via_cid_tree(
        &mut self,
        cid_tree: &CidTree,
        path: &str,
        flags: types::DescriptorFlags,
    ) -> FsResult<Resource<types::Descriptor>> {
        use wasmtime_wasi::{DirPerms, FilePerms, OpenMode};

        // Reject writes — CidTree is immutable
        if flags.contains(types::DescriptorFlags::WRITE) {
            return Err(types::ErrorCode::NotPermitted.into());
        }

        let resolved = cid_tree.resolve_path(path).await.map_err(|e| {
            tracing::debug!(path = %path, err = %e, "CidTree path resolution failed");
            FsError::from(types::ErrorCode::NoEntry)
        })?;

        match resolved {
            ResolvedNode::LocalFile(host_path) => {
                let file = cap_std::fs::Dir::open_ambient_dir(
                    host_path.parent().unwrap_or(&host_path),
                    cap_std::ambient_authority(),
                )
                .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?
                .open(host_path.file_name().unwrap_or_default())
                .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;

                let wasi_file = wasmtime_wasi::filesystem::File::new(
                    file,
                    FilePerms::READ,
                    OpenMode::READ,
                    false,
                );
                let descriptor = wasmtime_wasi::filesystem::Descriptor::File(wasi_file);
                self.table
                    .push(descriptor)
                    .map_err(|_| -> FsError { types::ErrorCode::Io.into() })
            }
            ResolvedNode::LocalDir(host_path) => {
                let dir =
                    cap_std::fs::Dir::open_ambient_dir(&host_path, cap_std::ambient_authority())
                        .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;
                let wasi_dir = wasmtime_wasi::filesystem::Dir::new(
                    dir,
                    DirPerms::READ,
                    FilePerms::READ,
                    OpenMode::READ,
                    false,
                );
                let descriptor = wasmtime_wasi::filesystem::Descriptor::Dir(wasi_dir);
                self.table
                    .push(descriptor)
                    .map_err(|_| -> FsError { types::ErrorCode::Io.into() })
            }
            ResolvedNode::CidFile { cid, .. } => {
                // Materialize file content to staging via PinsetCache, then open real FD.
                let cache = self
                    .cache_mode
                    .as_ref()
                    .ok_or_else(|| -> FsError { types::ErrorCode::Io.into() })?;

                let cid_parsed: cid::Cid = cid
                    .parse()
                    .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;

                cache.ensure(&cid_parsed).await.map_err(|e| {
                    tracing::warn!(cid = %cid, err = %e, "CidTree cache ensure failed");
                    FsError::from(types::ErrorCode::Io)
                })?;

                let staging_path = cache.staging_dir().join(&cid);
                if !staging_path.exists() {
                    let bytes = cache.fetch(&cid_parsed).await.map_err(|e| {
                        tracing::warn!(cid = %cid, err = %e, "CidTree fetch failed");
                        FsError::from(types::ErrorCode::Io)
                    })?;

                    if let Some(parent) = staging_path.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;
                    }
                    std::fs::write(&staging_path, &bytes)
                        .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;
                }

                let file = cap_std::fs::Dir::open_ambient_dir(
                    staging_path.parent().unwrap_or(&staging_path),
                    cap_std::ambient_authority(),
                )
                .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?
                .open(staging_path.file_name().unwrap_or_default())
                .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;

                let wasi_file = wasmtime_wasi::filesystem::File::new(
                    file,
                    FilePerms::READ,
                    OpenMode::READ,
                    false,
                );
                let descriptor = wasmtime_wasi::filesystem::Descriptor::File(wasi_file);
                self.table
                    .push(descriptor)
                    .map_err(|_| -> FsError { types::ErrorCode::Io.into() })
            }
            ResolvedNode::CidDir { cid } => {
                // Create a staging directory populated with stub entries from
                // CidTree::ls_dir(). Directories are real subdirs. Files are
                // sparse stubs with correct size (truncate to reported size)
                // so stat() and readdir() return accurate metadata.
                // Actual file content is fetched lazily on open_at.
                let staging_dir = cid_tree.staging_dir().join(format!("dir-{cid}"));
                if !staging_dir.exists() {
                    std::fs::create_dir_all(&staging_dir)
                        .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;

                    if let Ok(entries) = cid_tree.ls_dir(&cid).await {
                        for entry in &entries {
                            let entry_path = staging_dir.join(&entry.name);
                            match &entry.entry_type {
                                crate::vfs::EntryType::Dir => {
                                    let _ = std::fs::create_dir_all(&entry_path);
                                }
                                _ => {
                                    // Create stub file with correct size via truncate.
                                    // The file is sparse (no disk blocks allocated for
                                    // zeroes on most filesystems).
                                    let f = std::fs::File::create(&entry_path)
                                        .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;
                                    f.set_len(entry.size)
                                        .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;
                                }
                            }
                        }
                    }
                }

                let dir =
                    cap_std::fs::Dir::open_ambient_dir(&staging_dir, cap_std::ambient_authority())
                        .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;
                let wasi_dir = wasmtime_wasi::filesystem::Dir::new(
                    dir,
                    DirPerms::READ,
                    FilePerms::READ,
                    OpenMode::READ,
                    false,
                );
                let descriptor = wasmtime_wasi::filesystem::Descriptor::Dir(wasi_dir);
                self.table
                    .push(descriptor)
                    .map_err(|_| -> FsError { types::ErrorCode::Io.into() })
            }
        }
    }
}

// ── open_at interception (original IPFS path handler) ─────────────

impl IpfsFilesystemView<'_> {
    /// Handle an `open_at` for an IPFS path.
    ///
    /// Ensures the CID is cached, materializes content to the staging dir,
    /// and opens it as a real file descriptor.
    async fn open_ipfs(
        &mut self,
        ipfs_path: IpfsCidPath,
        _oflags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> FsResult<Resource<types::Descriptor>> {
        use wasmtime_wasi::{DirPerms, FilePerms, OpenMode};

        // Reject writes — /ipfs/ is content-addressed and immutable
        if flags.contains(types::DescriptorFlags::WRITE) {
            return Err(types::ErrorCode::NotPermitted.into());
        }

        let cache = self
            .cache_mode
            .as_ref()
            .ok_or_else(|| -> FsError { types::ErrorCode::NoEntry.into() })?;

        // Ensure CID is pinned in IPFS
        cache.ensure(&ipfs_path.cid).await.map_err(|e| {
            tracing::warn!(cid = %ipfs_path.cid, err = %e, "IPFS cache ensure failed");
            FsError::from(types::ErrorCode::Io)
        })?;

        // Materialize to staging directory (local filesystem is our cache).
        // Staging dir is owned by the CacheMode: shared for Shared, per-proc for Isolated.
        let staging_path = cache.staging_dir().join(ipfs_path.cid.to_string());
        let target_path = if ipfs_path.subpath.is_empty() {
            staging_path.clone()
        } else {
            staging_path.join(&ipfs_path.subpath)
        };

        // Skip fetch if already staged (disk cache hit)
        if !target_path.exists() {
            let fetch_result = if ipfs_path.subpath.is_empty() {
                cache.fetch(&ipfs_path.cid).await
            } else {
                cache.fetch_path(&ipfs_path.cid, &ipfs_path.subpath).await
            };
            let bytes = fetch_result.map_err(|e| {
                tracing::warn!(
                    cid = %ipfs_path.cid,
                    subpath = %ipfs_path.subpath,
                    err = %e,
                    "IPFS fetch failed"
                );
                FsError::from(types::ErrorCode::Io)
            })?;

            if let Some(parent) = target_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;
            }
            std::fs::write(&target_path, &bytes)
                .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;
        }

        if !target_path.exists() {
            return Err(types::ErrorCode::NoEntry.into());
        }

        // Open as a real filesystem descriptor
        let meta = std::fs::metadata(&target_path)
            .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;

        let descriptor = if meta.is_dir() {
            let dir =
                cap_std::fs::Dir::open_ambient_dir(&target_path, cap_std::ambient_authority())
                    .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;
            let wasi_dir = wasmtime_wasi::filesystem::Dir::new(
                dir,
                DirPerms::READ,
                FilePerms::READ,
                OpenMode::READ,
                false,
            );
            wasmtime_wasi::filesystem::Descriptor::Dir(wasi_dir)
        } else {
            let file = cap_std::fs::Dir::open_ambient_dir(
                target_path.parent().unwrap_or(&target_path),
                cap_std::ambient_authority(),
            )
            .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?
            .open(target_path.file_name().unwrap_or_default())
            .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;

            let wasi_file =
                wasmtime_wasi::filesystem::File::new(file, FilePerms::READ, OpenMode::READ, false);
            wasmtime_wasi::filesystem::Descriptor::File(wasi_file)
        };

        let fd = self
            .table
            .push(descriptor)
            .map_err(|_| -> FsError { types::ErrorCode::Io.into() })?;
        Ok(fd)
    }
}

// ── HostDescriptor — delegate everything, intercept open_at ────────

impl types::HostDescriptor for IpfsFilesystemView<'_> {
    async fn advise(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
        len: types::Filesize,
        advice: types::Advice,
    ) -> FsResult<()> {
        self.as_wasi_view().advise(fd, offset, len, advice).await
    }

    async fn sync_data(&mut self, fd: Resource<types::Descriptor>) -> FsResult<()> {
        self.as_wasi_view().sync_data(fd).await
    }

    async fn get_flags(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::DescriptorFlags> {
        self.as_wasi_view().get_flags(fd).await
    }

    async fn get_type(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::DescriptorType> {
        self.as_wasi_view().get_type(fd).await
    }

    async fn set_size(
        &mut self,
        fd: Resource<types::Descriptor>,
        size: types::Filesize,
    ) -> FsResult<()> {
        self.as_wasi_view().set_size(fd, size).await
    }

    async fn set_times(
        &mut self,
        fd: Resource<types::Descriptor>,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> FsResult<()> {
        self.as_wasi_view().set_times(fd, atim, mtim).await
    }

    async fn read(
        &mut self,
        fd: Resource<types::Descriptor>,
        len: types::Filesize,
        offset: types::Filesize,
    ) -> FsResult<(Vec<u8>, bool)> {
        self.as_wasi_view().read(fd, len, offset).await
    }

    async fn write(
        &mut self,
        fd: Resource<types::Descriptor>,
        buf: Vec<u8>,
        offset: types::Filesize,
    ) -> FsResult<types::Filesize> {
        self.as_wasi_view().write(fd, buf, offset).await
    }

    async fn read_directory(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<Resource<types::DirectoryEntryStream>> {
        self.as_wasi_view().read_directory(fd).await
    }

    async fn sync(&mut self, fd: Resource<types::Descriptor>) -> FsResult<()> {
        self.as_wasi_view().sync(fd).await
    }

    async fn create_directory_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        self.as_wasi_view().create_directory_at(fd, path).await
    }

    async fn stat(&mut self, fd: Resource<types::Descriptor>) -> FsResult<types::DescriptorStat> {
        self.as_wasi_view().stat(fd).await
    }

    async fn stat_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
    ) -> FsResult<types::DescriptorStat> {
        self.as_wasi_view().stat_at(fd, path_flags, path).await
    }

    async fn set_times_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> FsResult<()> {
        self.as_wasi_view()
            .set_times_at(fd, path_flags, path, atim, mtim)
            .await
    }

    async fn link_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        old_path_flags: types::PathFlags,
        old_path: String,
        new_descriptor: Resource<types::Descriptor>,
        new_path: String,
    ) -> FsResult<()> {
        self.as_wasi_view()
            .link_at(fd, old_path_flags, old_path, new_descriptor, new_path)
            .await
    }

    async fn open_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
        oflags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> FsResult<Resource<types::Descriptor>> {
        // CidTree-rooted paths resolve through the virtual filesystem.
        // Guests build `$WW_ROOT/…` paths which wasi-libc turns into
        // relative `ipfs/<root_cid>/…`; route those through CidTree so
        // directory CIDs work (open_ipfs is file-only and fails loudly
        // for directories).
        if let Some(ref cid_tree) = self.cid_tree {
            let rooted_subpath = parse_ipfs_path(&path)
                .filter(|p| p.cid.to_string() == *cid_tree.root_cid())
                .map(|p| p.subpath);
            let target = rooted_subpath.unwrap_or_else(|| {
                // Non-ipfs path: resolve directly against the CidTree root.
                path.clone()
            });
            // Only skip CidTree for paths that explicitly reference a
            // different CID — those are content-addressed leaf fetches
            // and belong in open_ipfs.
            let is_other_ipfs = parse_ipfs_path(&path)
                .map(|p| p.cid.to_string() != *cid_tree.root_cid())
                .unwrap_or(false);
            if !is_other_ipfs {
                let cid_tree = Arc::clone(cid_tree);
                tracing::debug!(path = %target, "CidTree open_at");
                return self.open_via_cid_tree(&cid_tree, &target, flags).await;
            }
        }

        // Intercept explicit /ipfs/<leaf_cid>/… paths (no CidTree active,
        // or CidTree active but the cid doesn't match root).
        if let Some(ipfs_path) = parse_ipfs_path(&path) {
            tracing::debug!(cid = %ipfs_path.cid, subpath = %ipfs_path.subpath, "Intercepting IPFS open_at");
            return self.open_ipfs(ipfs_path, oflags, flags).await;
        }

        // Delegate to standard filesystem
        self.as_wasi_view()
            .open_at(fd, path_flags, path, oflags, flags)
            .await
    }

    fn drop(&mut self, fd: Resource<types::Descriptor>) -> anyhow::Result<()> {
        self.as_wasi_view().drop(fd)
    }

    async fn readlink_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<String> {
        self.as_wasi_view().readlink_at(fd, path).await
    }

    async fn remove_directory_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        self.as_wasi_view().remove_directory_at(fd, path).await
    }

    async fn rename_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        old_path: String,
        new_fd: Resource<types::Descriptor>,
        new_path: String,
    ) -> FsResult<()> {
        self.as_wasi_view()
            .rename_at(fd, old_path, new_fd, new_path)
            .await
    }

    async fn symlink_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        src_path: String,
        dest_path: String,
    ) -> FsResult<()> {
        self.as_wasi_view()
            .symlink_at(fd, src_path, dest_path)
            .await
    }

    async fn unlink_file_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        self.as_wasi_view().unlink_file_at(fd, path).await
    }

    fn read_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
    ) -> FsResult<Resource<DynInputStream>> {
        self.as_wasi_view().read_via_stream(fd, offset)
    }

    fn write_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
    ) -> FsResult<Resource<DynOutputStream>> {
        self.as_wasi_view().write_via_stream(fd, offset)
    }

    fn append_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<Resource<DynOutputStream>> {
        self.as_wasi_view().append_via_stream(fd)
    }

    async fn is_same_object(
        &mut self,
        a: Resource<types::Descriptor>,
        b: Resource<types::Descriptor>,
    ) -> anyhow::Result<bool> {
        self.as_wasi_view().is_same_object(a, b).await
    }

    async fn metadata_hash(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::MetadataHashValue> {
        self.as_wasi_view().metadata_hash(fd).await
    }

    async fn metadata_hash_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
    ) -> FsResult<types::MetadataHashValue> {
        self.as_wasi_view()
            .metadata_hash_at(fd, path_flags, path)
            .await
    }
}

// ── Host trait (error code conversion) ─────────────────────────────

impl types::Host for IpfsFilesystemView<'_> {
    fn convert_error_code(&mut self, err: FsError) -> anyhow::Result<types::ErrorCode> {
        self.as_wasi_view().convert_error_code(err)
    }

    fn filesystem_error_code(
        &mut self,
        err: Resource<anyhow::Error>,
    ) -> anyhow::Result<Option<types::ErrorCode>> {
        self.as_wasi_view().filesystem_error_code(err)
    }
}

// ── HostDirectoryEntryStream ───────────────────────────────────────

impl types::HostDirectoryEntryStream for IpfsFilesystemView<'_> {
    async fn read_directory_entry(
        &mut self,
        stream: Resource<types::DirectoryEntryStream>,
    ) -> FsResult<Option<types::DirectoryEntry>> {
        self.as_wasi_view().read_directory_entry(stream).await
    }

    fn drop(&mut self, stream: Resource<types::DirectoryEntryStream>) -> anyhow::Result<()> {
        types::HostDirectoryEntryStream::drop(&mut self.as_wasi_view(), stream)
    }
}

// ── Preopens ───────────────────────────────────────────────────────

impl preopens::Host for IpfsFilesystemView<'_> {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<types::Descriptor>, String)>> {
        self.as_wasi_view().get_directories()
    }
}

// ── Linker override ────────────────────────────────────────────────

/// Override the filesystem linker bindings with our IPFS interceptor.
///
/// Call this AFTER `add_to_linker_async` to replace the standard filesystem
/// implementation with one that intercepts `/ipfs/` paths.
pub(crate) fn override_filesystem_linker(linker: &mut Linker<ComponentRunStates>) -> Result<()> {
    // Enable shadowing so we can override the already-registered filesystem bindings
    linker.allow_shadowing(true);

    types::add_to_linker::<ComponentRunStates, IpfsFilesystem>(linker, ipfs_filesystem)?;
    preopens::add_to_linker::<ComponentRunStates, IpfsFilesystem>(linker, ipfs_filesystem)?;

    // Restore default (no shadowing) for safety
    linker.allow_shadowing(false);

    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    // ── CID path parsing tests ─────────────────────────────────────

    #[test]
    fn test_parse_ipfs_path_with_subpath() {
        let cid_str = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
        let path = format!("ipfs/{cid_str}/sub/file.txt");
        let parsed = parse_ipfs_path(&path).expect("should parse");
        assert_eq!(parsed.cid.to_string(), cid_str);
        assert_eq!(parsed.subpath, "sub/file.txt");
    }

    #[test]
    fn test_parse_ipfs_path_root() {
        let cid_str = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
        let path = format!("ipfs/{cid_str}");
        let parsed = parse_ipfs_path(&path).expect("should parse");
        assert_eq!(parsed.cid.to_string(), cid_str);
        assert_eq!(parsed.subpath, "");
    }

    #[test]
    fn test_parse_non_ipfs_path() {
        assert!(parse_ipfs_path("usr/local/bin").is_none());
        assert!(parse_ipfs_path("etc/config").is_none());
    }

    #[test]
    fn test_parse_ipfs_path_invalid_cid() {
        assert!(parse_ipfs_path("ipfs/not-a-valid-cid/file").is_none());
    }

    #[test]
    fn test_parse_ipfs_path_rejects_traversal() {
        let cid_str = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
        // Direct traversal
        assert!(parse_ipfs_path(&format!("ipfs/{cid_str}/../../etc/passwd")).is_none());
        // Mid-path traversal
        assert!(parse_ipfs_path(&format!("ipfs/{cid_str}/sub/../../../etc")).is_none());
        // Single dotdot
        assert!(parse_ipfs_path(&format!("ipfs/{cid_str}/..")).is_none());
        // Valid subpaths still work
        assert!(parse_ipfs_path(&format!("ipfs/{cid_str}/sub/file.txt")).is_some());
        assert!(parse_ipfs_path(&format!("ipfs/{cid_str}/file..name")).is_some());
    }

    // ── Mock pinner for integration tests ──────────────────────────

    struct MockPinner {
        data: HashMap<cid::Cid, Vec<u8>>,
        path_data: HashMap<String, Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl cache::Pinner for MockPinner {
        async fn pin(&self, _cid: &cid::Cid) -> anyhow::Result<()> {
            Ok(())
        }
        async fn unpin(&self, _cid: &cid::Cid) -> anyhow::Result<()> {
            Ok(())
        }
        async fn fetch(&self, cid: &cid::Cid) -> anyhow::Result<Vec<u8>> {
            self.data
                .get(cid)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("CID not found in mock"))
        }
        async fn fetch_path(&self, cid: &cid::Cid, subpath: &str) -> anyhow::Result<Vec<u8>> {
            if subpath.is_empty() {
                return self.fetch(cid).await;
            }
            let key = format!("{cid}/{subpath}");
            self.path_data
                .get(&key)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("CID subpath not found in mock: {key}"))
        }
        async fn size(&self, cid: &cid::Cid) -> anyhow::Result<u64> {
            self.data
                .get(cid)
                .map(|d| d.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("CID not found in mock"))
        }
    }

    /// Helper: construct a test CID + mock pinner with known content.
    fn test_cid_and_pinner(content: &[u8]) -> (cid::Cid, Arc<MockPinner>) {
        let cid_str = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
        let cid: cid::Cid = cid_str.parse().unwrap();
        let mut data = HashMap::new();
        data.insert(cid, content.to_vec());
        (
            cid,
            Arc::new(MockPinner {
                data,
                path_data: HashMap::new(),
            }),
        )
    }

    /// Helper: construct a test CID + mock pinner where subpath bytes differ
    /// from the root CID bytes.
    fn test_cid_and_pinner_with_subpath(
        root_content: &[u8],
        subpath: &str,
        subpath_content: &[u8],
    ) -> (cid::Cid, Arc<MockPinner>) {
        let (cid, _) = test_cid_and_pinner(root_content);
        let mut data = HashMap::new();
        data.insert(cid, root_content.to_vec());
        let mut path_data = HashMap::new();
        path_data.insert(format!("{cid}/{subpath}"), subpath_content.to_vec());
        (cid, Arc::new(MockPinner { data, path_data }))
    }

    /// Helper: build the view for testing open_ipfs.
    struct TestHarness {
        wasi_ctx: wasmtime_wasi::WasiCtx,
        resource_table: wasmtime::component::ResourceTable,
        cache_mode: Option<cache::CacheMode>,
        cid_tree: Option<Arc<CidTree>>,
    }

    impl TestHarness {
        fn new(cache_mode: Option<cache::CacheMode>) -> Self {
            Self {
                wasi_ctx: wasmtime_wasi::WasiCtxBuilder::new().build(),
                resource_table: wasmtime::component::ResourceTable::new(),
                cache_mode,
                cid_tree: None,
            }
        }

        fn view(&mut self) -> IpfsFilesystemView<'_> {
            IpfsFilesystemView {
                ctx: self.wasi_ctx.filesystem(),
                table: &mut self.resource_table,
                cache_mode: &self.cache_mode,
                cid_tree: &self.cid_tree,
            }
        }
    }

    // ── Integration tests ──────────────────────────────────────────

    #[tokio::test]
    async fn test_open_ipfs_file_materializes_and_returns_descriptor() {
        let content = b"hello ipfs world";
        let (cid, pinner) = test_cid_and_pinner(content);

        let isolated = cache::IsolatedPinset::new(pinner).unwrap();
        let mut harness = TestHarness::new(Some(cache::CacheMode::Isolated(isolated)));

        let ipfs_path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };
        let fd = harness
            .view()
            .open_ipfs(
                ipfs_path,
                types::OpenFlags::empty(),
                types::DescriptorFlags::READ,
            )
            .await
            .expect("open_ipfs should succeed");

        // Descriptor was pushed to the resource table
        let desc = harness.resource_table.get(&fd);
        assert!(desc.is_ok(), "descriptor should be in resource table");

        // Content was materialized to staging
        let staging_file = harness
            .cache_mode
            .as_ref()
            .unwrap()
            .staging_dir()
            .join(cid.to_string());
        assert!(staging_file.exists(), "staging file should exist");
        assert_eq!(
            std::fs::read(&staging_file).unwrap(),
            content,
            "staging file should contain the IPFS content"
        );
    }

    #[tokio::test]
    async fn test_open_ipfs_write_rejected() {
        let (cid, pinner) = test_cid_and_pinner(b"data");
        let isolated = cache::IsolatedPinset::new(pinner).unwrap();
        let mut harness = TestHarness::new(Some(cache::CacheMode::Isolated(isolated)));

        let ipfs_path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };
        let result = harness
            .view()
            .open_ipfs(
                ipfs_path,
                types::OpenFlags::empty(),
                types::DescriptorFlags::READ | types::DescriptorFlags::WRITE,
            )
            .await;

        assert!(result.is_err(), "write to /ipfs/ should be rejected");
    }

    #[tokio::test]
    async fn test_open_ipfs_no_cache_returns_error() {
        let cid: cid::Cid = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG"
            .parse()
            .unwrap();
        let mut harness = TestHarness::new(None); // no cache

        let ipfs_path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };
        let result = harness
            .view()
            .open_ipfs(
                ipfs_path,
                types::OpenFlags::empty(),
                types::DescriptorFlags::READ,
            )
            .await;

        assert!(result.is_err(), "open without cache should fail");
    }

    #[tokio::test]
    async fn test_open_ipfs_unknown_cid_returns_error() {
        // Pinner has no data for the CID we'll request
        let pinner = Arc::new(MockPinner {
            data: HashMap::new(),
            path_data: HashMap::new(),
        });
        let isolated = cache::IsolatedPinset::new(pinner).unwrap();
        let mut harness = TestHarness::new(Some(cache::CacheMode::Isolated(isolated)));

        let cid: cid::Cid = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG"
            .parse()
            .unwrap();
        let ipfs_path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };
        let result = harness
            .view()
            .open_ipfs(
                ipfs_path,
                types::OpenFlags::empty(),
                types::DescriptorFlags::READ,
            )
            .await;

        assert!(result.is_err(), "unknown CID should fail");
    }

    #[tokio::test]
    async fn test_open_ipfs_with_shared_cache() {
        let content = b"shared cache content";
        let (cid, pinner) = test_cid_and_pinner(content);

        let pinset = Arc::new(cache::PinsetCache::new(pinner, 10 * 1024 * 1024).unwrap());
        let mut harness = TestHarness::new(Some(cache::CacheMode::Shared(pinset)));

        let ipfs_path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };
        let fd = harness
            .view()
            .open_ipfs(
                ipfs_path,
                types::OpenFlags::empty(),
                types::DescriptorFlags::READ,
            )
            .await
            .expect("open_ipfs with shared cache should succeed");

        assert!(harness.resource_table.get(&fd).is_ok());

        let staging_file = harness
            .cache_mode
            .as_ref()
            .unwrap()
            .staging_dir()
            .join(cid.to_string());
        assert_eq!(std::fs::read(&staging_file).unwrap(), content);
    }

    #[tokio::test]
    async fn test_open_ipfs_with_subpath() {
        // Root CID bytes and subpath bytes intentionally differ.
        // Regression: open_ipfs must fetch /ipfs/<cid>/<subpath>, not /ipfs/<cid>.
        let root_bytes = b"root cid blob bytes";
        let nested_bytes = b"nested file content";
        let (cid, pinner) =
            test_cid_and_pinner_with_subpath(root_bytes, "sub/dir/file.txt", nested_bytes);

        let isolated = cache::IsolatedPinset::new(pinner).unwrap();
        let mut harness = TestHarness::new(Some(cache::CacheMode::Isolated(isolated)));

        let ipfs_path = IpfsCidPath {
            cid,
            subpath: "sub/dir/file.txt".to_string(),
        };
        let fd = harness
            .view()
            .open_ipfs(
                ipfs_path,
                types::OpenFlags::empty(),
                types::DescriptorFlags::READ,
            )
            .await
            .expect("open_ipfs with subpath should succeed");

        assert!(harness.resource_table.get(&fd).is_ok());

        // Verify nested path was created in staging
        let nested_file = harness
            .cache_mode
            .as_ref()
            .unwrap()
            .staging_dir()
            .join(cid.to_string())
            .join("sub/dir/file.txt");
        assert!(nested_file.exists(), "nested staging file should exist");
        assert_eq!(std::fs::read(&nested_file).unwrap(), nested_bytes);
    }

    #[tokio::test]
    async fn test_open_ipfs_skips_fetch_on_staging_hit() {
        let content = b"cached on disk";
        let (cid, pinner) = test_cid_and_pinner(content);

        let isolated = cache::IsolatedPinset::new(pinner).unwrap();
        let mut harness = TestHarness::new(Some(cache::CacheMode::Isolated(isolated)));

        // First open: fetches and stages
        let ipfs_path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };
        harness
            .view()
            .open_ipfs(
                ipfs_path,
                types::OpenFlags::empty(),
                types::DescriptorFlags::READ,
            )
            .await
            .expect("first open should succeed");

        // Second open: should hit staging (file already exists)
        let ipfs_path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };
        let fd = harness
            .view()
            .open_ipfs(
                ipfs_path,
                types::OpenFlags::empty(),
                types::DescriptorFlags::READ,
            )
            .await
            .expect("second open should hit staging cache");

        assert!(harness.resource_table.get(&fd).is_ok());
    }
}
