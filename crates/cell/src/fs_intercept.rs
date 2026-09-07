//! WASI filesystem interceptor for `/ipfs/` paths and CidTree-backed virtual FS.
//!
//! When a `CidTree` is present (virtual mode), `open-at` resolves paths lazily
//! through the content-addressed tree. File content is materialized to a staging
//! directory on demand, then opened as a real `cap-std` file descriptor so all
//! subsequent descriptor operations delegate to wasmtime-wasi's standard impl.
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
    pub cache_mode: &'a Option<Arc<cache::CacheMode>>,
    pub cid_tree: &'a Option<Arc<CidTree>>,
    pub writable_descriptors: &'a mut std::collections::HashSet<u32>,
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
    state.mark_host_call();
    // Split borrow across distinct fields of ComponentRunStates.
    IpfsFilesystemView {
        ctx: state.wasi_ctx.filesystem(),
        table: &mut state.resource_table,
        cache_mode: &state.cache_mode,
        cid_tree: &state.cid_tree,
        writable_descriptors: &mut state.writable_fs_descriptors,
    }
}

#[allow(
    dead_code,
    reason = "used by the dormant P3 linker and its artifact test lane"
)]
pub(crate) trait FilesystemHostState: Send + Sized + 'static {
    fn intercepted_filesystem(&mut self) -> IpfsFilesystemView<'_>;
    fn wasi_filesystem_getter(
    ) -> for<'a> fn(&'a mut Self) -> <wasmtime_wasi::filesystem::WasiFilesystem as HasData>::Data<'a>;
    fn wasi_filesystem_access<'a>(
        store: wasmtime::StoreContextMut<'a, Self>,
    ) -> Access<'a, Self, wasmtime_wasi::filesystem::WasiFilesystem>;
}

fn component_wasi_filesystem(
    state: &mut ComponentRunStates,
) -> <WasiFilesystem as HasData>::Data<'_> {
    state.mark_host_call();
    WasiFilesystemCtxView {
        ctx: state.wasi_ctx.filesystem(),
        table: &mut state.resource_table,
    }
}

impl FilesystemHostState for ComponentRunStates {
    fn intercepted_filesystem(&mut self) -> IpfsFilesystemView<'_> {
        ipfs_filesystem(self)
    }

    fn wasi_filesystem_getter() -> for<'a> fn(&'a mut Self) -> <WasiFilesystem as HasData>::Data<'a>
    {
        component_wasi_filesystem
    }

    fn wasi_filesystem_access<'a>(
        store: wasmtime::StoreContextMut<'a, Self>,
    ) -> Access<'a, Self, WasiFilesystem> {
        Access::new(store, |state| component_wasi_filesystem(state))
    }
}

fn p3_wasi_access<'a, T: FilesystemHostState>(
    store: wasmtime::StoreContextMut<'a, T>,
) -> Access<'a, T, WasiFilesystem> {
    T::wasi_filesystem_access(store)
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

    // Reject every absolute or non-normal component before joining this path
    // to a host staging directory.
    if !subpath.is_empty() && !is_confined_relative_path(subpath) {
        return None;
    }

    let cid = cid_str.parse::<cid::Cid>().ok()?;
    Some(IpfsCidPath {
        cid,
        subpath: subpath.to_string(),
    })
}

enum OpenRoute {
    CidTree(Arc<CidTree>, String),
    Ipfs(IpfsCidPath),
    Wasi,
}

fn route_open(cid_tree: Option<&Arc<CidTree>>, path: &str) -> OpenRoute {
    if let Some(cid_tree) = cid_tree {
        let rooted_subpath = parse_ipfs_path(path)
            .filter(|parsed| parsed.cid.to_string() == *cid_tree.root_cid())
            .map(|parsed| parsed.subpath);
        let is_other_ipfs = parse_ipfs_path(path)
            .map(|parsed| parsed.cid.to_string() != *cid_tree.root_cid())
            .unwrap_or(false);
        if !is_other_ipfs {
            return OpenRoute::CidTree(
                Arc::clone(cid_tree),
                rooted_subpath.unwrap_or_else(|| path.to_string()),
            );
        }
    }

    match parse_ipfs_path(path) {
        Some(path) => OpenRoute::Ipfs(path),
        None => OpenRoute::Wasi,
    }
}

fn is_confined_relative_path(path: &str) -> bool {
    use std::path::Component;

    !std::path::Path::new(path).is_absolute()
        && std::path::Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn open_read_only_path(
    path: &std::path::Path,
) -> Result<wasmtime_wasi::filesystem::Descriptor, MaterializeError> {
    use wasmtime_wasi::filesystem::{Descriptor, Dir, File};
    use wasmtime_wasi::{FsPerms, OpenMode};

    let metadata = std::fs::metadata(path).map_err(|_| MaterializeError::Io)?;
    if metadata.is_dir() {
        let dir = cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
            .map_err(|_| MaterializeError::Io)?;
        Ok(Descriptor::Dir(Dir::new(
            dir.into_std_file(),
            FsPerms::ReadOnly,
            OpenMode::READ,
            false,
        )))
    } else {
        let file = cap_std::fs::Dir::open_ambient_dir(
            path.parent().unwrap_or(path),
            cap_std::ambient_authority(),
        )
        .map_err(|_| MaterializeError::Io)?
        .open(path.file_name().unwrap_or_default())
        .map_err(|_| MaterializeError::Io)?;
        Ok(Descriptor::File(File::new(
            file.into_std(),
            FsPerms::ReadOnly,
            OpenMode::READ,
            false,
        )))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MaterializeError {
    Invalid,
    Io,
    NoEntry,
    NotPermitted,
}

impl From<MaterializeError> for types::ErrorCode {
    fn from(error: MaterializeError) -> Self {
        match error {
            MaterializeError::Invalid => Self::Invalid,
            MaterializeError::Io => Self::Io,
            MaterializeError::NoEntry => Self::NoEntry,
            MaterializeError::NotPermitted => Self::NotPermitted,
        }
    }
}

impl From<MaterializeError> for p3_types::ErrorCode {
    fn from(error: MaterializeError) -> Self {
        match error {
            MaterializeError::Invalid => Self::Invalid,
            MaterializeError::Io => Self::Io,
            MaterializeError::NoEntry => Self::NoEntry,
            MaterializeError::NotPermitted => Self::NotPermitted,
        }
    }
}

pub(crate) async fn materialize_cid_tree_descriptor(
    cache: Option<&cache::CacheMode>,
    cid_tree: &CidTree,
    path: &str,
    write_requested: bool,
) -> Result<wasmtime_wasi::filesystem::Descriptor, MaterializeError> {
    if write_requested {
        return Err(MaterializeError::NotPermitted);
    }

    let resolved = cid_tree.resolve_path(path).await.map_err(|error| {
        tracing::debug!(path, %error, "CidTree path resolution failed");
        MaterializeError::NoEntry
    })?;

    match resolved {
        ResolvedNode::CidFile { cid, .. } => {
            let cache = cache.ok_or(MaterializeError::Io)?;
            let parsed = cid.parse::<cid::Cid>().map_err(|_| MaterializeError::Io)?;
            let canonical_cid = parsed.to_string();
            cache.ensure(&parsed).await.map_err(|error| {
                tracing::warn!(%canonical_cid, %error, "CidTree cache ensure failed");
                MaterializeError::Io
            })?;
            let staging_path = cache.staging_dir().join(&canonical_cid);
            if !staging_path.exists() {
                cache
                    .fetch_to_path(&parsed, &staging_path)
                    .await
                    .map_err(|error| {
                        tracing::warn!(%canonical_cid, %error, "CidTree stream fetch failed");
                        MaterializeError::Io
                    })?;
            }
            if !staging_path.exists() {
                return Err(MaterializeError::Io);
            }
            open_read_only_path(&staging_path)
        }
        ResolvedNode::CidDir { cid } => {
            let parsed = cid.parse::<cid::Cid>().map_err(|error| {
                tracing::warn!(%cid, %error, "CidTree directory has an invalid CID");
                MaterializeError::Invalid
            })?;
            let canonical_cid = parsed.to_string();
            let staging_dir = cid_tree.staging_dir().join(format!("dir-{canonical_cid}"));
            if !staging_dir.exists() {
                std::fs::create_dir_all(&staging_dir).map_err(|_| MaterializeError::Io)?;
                if let Ok(entries) = cid_tree.ls_dir(&canonical_cid).await {
                    for entry in entries {
                        if !is_confined_relative_path(&entry.name)
                            || std::path::Path::new(&entry.name).components().count() != 1
                        {
                            tracing::warn!(name = %entry.name, "rejected unconfined CidTree entry");
                            return Err(MaterializeError::Invalid);
                        }
                        let entry_path = staging_dir.join(&entry.name);
                        match entry.entry_type {
                            crate::vfs::EntryType::Dir => {
                                std::fs::create_dir_all(entry_path)
                                    .map_err(|_| MaterializeError::Io)?;
                            }
                            _ => {
                                let file = std::fs::File::create(entry_path)
                                    .map_err(|_| MaterializeError::Io)?;
                                file.set_len(entry.size).map_err(|_| MaterializeError::Io)?;
                            }
                        }
                    }
                }
            }
            open_read_only_path(&staging_dir)
        }
    }
}

pub(crate) async fn materialize_ipfs_descriptor(
    cache: Option<&cache::CacheMode>,
    ipfs_path: &IpfsCidPath,
    write_requested: bool,
) -> Result<wasmtime_wasi::filesystem::Descriptor, MaterializeError> {
    if write_requested {
        return Err(MaterializeError::NotPermitted);
    }
    if !ipfs_path.subpath.is_empty() && !is_confined_relative_path(&ipfs_path.subpath) {
        return Err(MaterializeError::Invalid);
    }

    let cache = cache.ok_or(MaterializeError::NoEntry)?;
    cache.ensure(&ipfs_path.cid).await.map_err(|error| {
        tracing::warn!(cid = %ipfs_path.cid, %error, "IPFS cache ensure failed");
        MaterializeError::Io
    })?;
    let staging_path = cache.staging_dir().join(ipfs_path.cid.to_string());
    let target_path = if ipfs_path.subpath.is_empty() {
        staging_path
    } else {
        staging_path.join(&ipfs_path.subpath)
    };
    if !target_path.exists() {
        let result = if ipfs_path.subpath.is_empty() {
            cache.fetch_to_path(&ipfs_path.cid, &target_path).await
        } else {
            cache
                .fetch_path_to_path(&ipfs_path.cid, &ipfs_path.subpath, &target_path)
                .await
        };
        result.map_err(|error| {
            tracing::warn!(cid = %ipfs_path.cid, subpath = %ipfs_path.subpath, %error, "IPFS stream fetch failed");
            MaterializeError::Io
        })?;
    }
    if !target_path.exists() {
        return Err(MaterializeError::NoEntry);
    }
    open_read_only_path(&target_path)
}

impl IpfsFilesystemView<'_> {
    async fn open_via_cid_tree(
        &mut self,
        cid_tree: &CidTree,
        path: &str,
        flags: types::DescriptorFlags,
    ) -> FsResult<Resource<types::Descriptor>> {
        let descriptor = materialize_cid_tree_descriptor(
            self.cache_mode.as_deref(),
            cid_tree,
            path,
            flags.contains(types::DescriptorFlags::WRITE),
        )
        .await
        .map_err(|error| FsError::from(types::ErrorCode::from(error)))?;
        self.table
            .push(descriptor)
            .map_err(|_| types::ErrorCode::Io.into())
    }

    async fn open_ipfs(
        &mut self,
        ipfs_path: IpfsCidPath,
        _open_flags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> FsResult<Resource<types::Descriptor>> {
        let descriptor = materialize_ipfs_descriptor(
            self.cache_mode.as_deref(),
            &ipfs_path,
            flags.contains(types::DescriptorFlags::WRITE),
        )
        .await
        .map_err(|error| FsError::from(types::ErrorCode::from(error)))?;
        self.table
            .push(descriptor)
            .map_err(|_| types::ErrorCode::Io.into())
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
        // `/tmp` and every descriptor opened beneath it belong to the
        // process-private scratch preopen, not to the immutable image tree.
        // Descriptor identity keeps this routing decision out of guest paths
        // and prevents a child from widening it.
        if self.writable_descriptors.contains(&fd.rep()) {
            let opened = self
                .as_wasi_view()
                .open_at(fd, path_flags, path, oflags, flags)
                .await?;
            self.writable_descriptors.insert(opened.rep());
            return Ok(opened);
        }

        match route_open(self.cid_tree.as_ref(), &path) {
            OpenRoute::CidTree(cid_tree, target) => {
                tracing::debug!(path = %target, "CidTree open_at");
                return self.open_via_cid_tree(&cid_tree, &target, flags).await;
            }
            OpenRoute::Ipfs(ipfs_path) => {
                tracing::debug!(cid = %ipfs_path.cid, subpath = %ipfs_path.subpath, "Intercepting IPFS open_at");
                return self.open_ipfs(ipfs_path, oflags, flags).await;
            }
            OpenRoute::Wasi => {}
        }

        // Delegate to standard filesystem
        self.as_wasi_view()
            .open_at(fd, path_flags, path, oflags, flags)
            .await
    }

    fn drop(&mut self, fd: Resource<types::Descriptor>) -> wasmtime::Result<()> {
        self.writable_descriptors.remove(&fd.rep());
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
    ) -> wasmtime::Result<bool> {
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
    fn convert_error_code(&mut self, err: FsError) -> wasmtime::Result<types::ErrorCode> {
        self.as_wasi_view().convert_error_code(err)
    }

    fn filesystem_error_code(
        &mut self,
        err: Resource<wasmtime::Error>,
    ) -> wasmtime::Result<Option<types::ErrorCode>> {
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

    fn drop(&mut self, stream: Resource<types::DirectoryEntryStream>) -> wasmtime::Result<()> {
        types::HostDirectoryEntryStream::drop(&mut self.as_wasi_view(), stream)
    }
}

// ── Preopens ───────────────────────────────────────────────────────

impl preopens::Host for IpfsFilesystemView<'_> {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<types::Descriptor>, String)>> {
        let directories = self.as_wasi_view().get_directories()?;
        for (descriptor, path) in &directories {
            if path == "/tmp" {
                self.writable_descriptors.insert(descriptor.rep());
            }
        }
        Ok(directories)
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

// ── WASI P3 adapter ───────────────────────────────────────────────

use wasmtime::component::{Access, Accessor, FutureReader, StreamReader};
use wasmtime::AsContextMut as _;
use wasmtime_wasi::filesystem::WasiFilesystem;
use wasmtime_wasi::p3::bindings::filesystem::{preopens as p3_preopens, types as p3_types};
use wasmtime_wasi::p3::filesystem::{
    FilesystemError as P3FilesystemError, FilesystemResult as P3FilesystemResult,
};

fn p3_wasi_accessor<T: FilesystemHostState>(
    store: &Accessor<T, IpfsFilesystem>,
) -> Accessor<T, WasiFilesystem> {
    store.with_getter::<WasiFilesystem>(T::wasi_filesystem_getter())
}

impl p3_types::Host for IpfsFilesystemView<'_> {
    fn convert_error_code(
        &mut self,
        error: P3FilesystemError,
    ) -> wasmtime::Result<p3_types::ErrorCode> {
        error.downcast()
    }
}

impl p3_types::HostDescriptor for IpfsFilesystemView<'_> {
    fn drop(
        &mut self,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
    ) -> wasmtime::Result<()> {
        self.writable_descriptors.remove(&descriptor.rep());
        p3_types::HostDescriptor::drop(&mut self.as_wasi_view(), descriptor)
    }
}

impl p3_preopens::Host for IpfsFilesystemView<'_> {
    fn get_directories(
        &mut self,
    ) -> wasmtime::Result<Vec<(Resource<wasmtime_wasi::filesystem::Descriptor>, String)>> {
        let directories = p3_preopens::Host::get_directories(&mut self.as_wasi_view())?;
        for (descriptor, path) in &directories {
            if path == "/tmp" {
                self.writable_descriptors.insert(descriptor.rep());
            }
        }
        Ok(directories)
    }
}

impl<T: FilesystemHostState> p3_types::HostDescriptorWithStore<T> for IpfsFilesystem {
    fn read_via_stream(
        mut store: Access<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        offset: p3_types::Filesize,
    ) -> wasmtime::Result<(
        StreamReader<u8>,
        FutureReader<Result<(), p3_types::ErrorCode>>,
    )> {
        let wasi = p3_wasi_access(store.as_context_mut());
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::read_via_stream(
            wasi, descriptor, offset,
        )
    }

    fn write_via_stream(
        mut store: Access<'_, T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        data: StreamReader<u8>,
        offset: p3_types::Filesize,
    ) -> wasmtime::Result<FutureReader<Result<(), p3_types::ErrorCode>>> {
        let wasi = p3_wasi_access(store.as_context_mut());
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::write_via_stream(
            wasi, descriptor, data, offset,
        )
    }

    fn append_via_stream(
        mut store: Access<'_, T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        data: StreamReader<u8>,
    ) -> wasmtime::Result<FutureReader<Result<(), p3_types::ErrorCode>>> {
        let wasi = p3_wasi_access(store.as_context_mut());
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::append_via_stream(
            wasi, descriptor, data,
        )
    }

    async fn advise(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        offset: p3_types::Filesize,
        length: p3_types::Filesize,
        advice: p3_types::Advice,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::advise(
            &p3_wasi_accessor(store),
            descriptor,
            offset,
            length,
            advice,
        )
        .await
    }

    async fn sync_data(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::sync_data(
            &p3_wasi_accessor(store),
            descriptor,
        )
        .await
    }

    async fn get_flags(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
    ) -> P3FilesystemResult<p3_types::DescriptorFlags> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::get_flags(
            &p3_wasi_accessor(store),
            descriptor,
        )
        .await
    }

    async fn get_type(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
    ) -> P3FilesystemResult<p3_types::DescriptorType> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::get_type(
            &p3_wasi_accessor(store),
            descriptor,
        )
        .await
    }

    async fn set_size(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        size: p3_types::Filesize,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::set_size(
            &p3_wasi_accessor(store),
            descriptor,
            size,
        )
        .await
    }

    async fn set_times(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        data_access_timestamp: p3_types::NewTimestamp,
        data_modification_timestamp: p3_types::NewTimestamp,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::set_times(
            &p3_wasi_accessor(store),
            descriptor,
            data_access_timestamp,
            data_modification_timestamp,
        )
        .await
    }

    fn read_directory(
        mut store: Access<'_, T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
    ) -> wasmtime::Result<(
        StreamReader<p3_types::DirectoryEntry>,
        FutureReader<Result<(), p3_types::ErrorCode>>,
    )> {
        let wasi = p3_wasi_access(store.as_context_mut());
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::read_directory(wasi, descriptor)
    }

    async fn sync(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::sync(
            &p3_wasi_accessor(store),
            descriptor,
        )
        .await
    }

    async fn create_directory_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        path: String,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::create_directory_at(
            &p3_wasi_accessor(store),
            descriptor,
            path,
        )
        .await
    }

    async fn stat(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
    ) -> P3FilesystemResult<p3_types::DescriptorStat> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::stat(
            &p3_wasi_accessor(store),
            descriptor,
        )
        .await
    }

    async fn stat_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        path_flags: p3_types::PathFlags,
        path: String,
    ) -> P3FilesystemResult<p3_types::DescriptorStat> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::stat_at(
            &p3_wasi_accessor(store),
            descriptor,
            path_flags,
            path,
        )
        .await
    }

    async fn set_times_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        path_flags: p3_types::PathFlags,
        path: String,
        data_access_timestamp: p3_types::NewTimestamp,
        data_modification_timestamp: p3_types::NewTimestamp,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::set_times_at(
            &p3_wasi_accessor(store),
            descriptor,
            path_flags,
            path,
            data_access_timestamp,
            data_modification_timestamp,
        )
        .await
    }

    async fn link_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        old_path_flags: p3_types::PathFlags,
        old_path: String,
        new_descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        new_path: String,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::link_at(
            &p3_wasi_accessor(store),
            descriptor,
            old_path_flags,
            old_path,
            new_descriptor,
            new_path,
        )
        .await
    }

    async fn open_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        path_flags: p3_types::PathFlags,
        path: String,
        open_flags: p3_types::OpenFlags,
        flags: p3_types::DescriptorFlags,
    ) -> P3FilesystemResult<Resource<wasmtime_wasi::filesystem::Descriptor>> {
        let writable = store.with(|mut access| {
            access
                .get()
                .writable_descriptors
                .contains(&descriptor.rep())
        });
        if writable {
            let opened = <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::open_at(
                &p3_wasi_accessor(store),
                descriptor,
                path_flags,
                path,
                open_flags,
                flags,
            )
            .await?;
            store.with(|mut access| {
                access.get().writable_descriptors.insert(opened.rep());
            });
            return Ok(opened);
        }

        let (cache, route) = store.with(|mut access| {
            let view = access.get();
            (
                view.cache_mode.clone(),
                route_open(view.cid_tree.as_ref(), &path),
            )
        });
        let write_requested = flags.contains(p3_types::DescriptorFlags::WRITE);
        let descriptor = match route {
            OpenRoute::CidTree(cid_tree, target) => {
                materialize_cid_tree_descriptor(
                    cache.as_deref(),
                    &cid_tree,
                    &target,
                    write_requested,
                )
                .await
            }
            OpenRoute::Ipfs(ipfs_path) => {
                materialize_ipfs_descriptor(cache.as_deref(), &ipfs_path, write_requested).await
            }
            OpenRoute::Wasi => {
                return <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::open_at(
                    &p3_wasi_accessor(store),
                    descriptor,
                    path_flags,
                    path,
                    open_flags,
                    flags,
                )
                .await;
            }
        }
        .map_err(|error| P3FilesystemError::from(p3_types::ErrorCode::from(error)))?;

        store
            .with(|mut access| access.get().table.push(descriptor))
            .map_err(P3FilesystemError::from)
    }

    async fn readlink_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        path: String,
    ) -> P3FilesystemResult<String> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::readlink_at(
            &p3_wasi_accessor(store),
            descriptor,
            path,
        )
        .await
    }

    async fn remove_directory_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        path: String,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::remove_directory_at(
            &p3_wasi_accessor(store),
            descriptor,
            path,
        )
        .await
    }

    async fn rename_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        old_path: String,
        new_descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        new_path: String,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::rename_at(
            &p3_wasi_accessor(store),
            descriptor,
            old_path,
            new_descriptor,
            new_path,
        )
        .await
    }

    async fn symlink_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        old_path: String,
        new_path: String,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::symlink_at(
            &p3_wasi_accessor(store),
            descriptor,
            old_path,
            new_path,
        )
        .await
    }

    async fn unlink_file_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        path: String,
    ) -> P3FilesystemResult<()> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::unlink_file_at(
            &p3_wasi_accessor(store),
            descriptor,
            path,
        )
        .await
    }

    async fn is_same_object(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        other: Resource<wasmtime_wasi::filesystem::Descriptor>,
    ) -> wasmtime::Result<bool> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::is_same_object(
            &p3_wasi_accessor(store),
            descriptor,
            other,
        )
        .await
    }

    async fn metadata_hash(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
    ) -> P3FilesystemResult<p3_types::MetadataHashValue> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::metadata_hash(
            &p3_wasi_accessor(store),
            descriptor,
        )
        .await
    }

    async fn metadata_hash_at(
        store: &Accessor<T, Self>,
        descriptor: Resource<wasmtime_wasi::filesystem::Descriptor>,
        path_flags: p3_types::PathFlags,
        path: String,
    ) -> P3FilesystemResult<p3_types::MetadataHashValue> {
        <WasiFilesystem as p3_types::HostDescriptorWithStore<T>>::metadata_hash_at(
            &p3_wasi_accessor(store),
            descriptor,
            path_flags,
            path,
        )
        .await
    }
}

#[allow(dead_code, reason = "used by the dormant P3 artifact test lane")]
pub(crate) fn override_p3_filesystem_linker<T: FilesystemHostState>(
    linker: &mut Linker<T>,
) -> Result<()> {
    linker.allow_shadowing(true);
    p3_types::add_to_linker::<T, IpfsFilesystem>(linker, T::intercepted_filesystem)?;
    p3_preopens::add_to_linker::<T, IpfsFilesystem>(linker, T::intercepted_filesystem)?;
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

    #[tokio::test]
    async fn cid_tree_rejects_unconfined_directory_entry_names() {
        let cid = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
        let staging = tempfile::TempDir::new().unwrap();
        let entries = vec![crate::vfs::DirEntry {
            name: "../escaped".to_string(),
            cid: cid.to_string(),
            entry_type: crate::vfs::EntryType::File,
            size: 1,
        }];
        std::fs::write(
            staging.path().join(format!("{cid}.dirlist.json")),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let tree = CidTree::new(
            cid.to_string(),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            staging.path().to_path_buf(),
        );

        let result = materialize_cid_tree_descriptor(None, &tree, "", false).await;
        assert!(matches!(result, Err(MaterializeError::Invalid)));
        assert!(!staging.path().join("escaped").exists());
    }

    #[tokio::test]
    async fn cid_tree_rejects_unconfined_directory_cid() {
        let root_cid = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
        let root = tempfile::TempDir::new().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let entries = vec![crate::vfs::DirEntry {
            name: "hostile".to_string(),
            cid: "segment/../../escaped".to_string(),
            entry_type: crate::vfs::EntryType::Dir,
            size: 0,
        }];
        std::fs::write(
            staging.join(format!("{root_cid}.dirlist.json")),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let tree = CidTree::new(
            root_cid.to_string(),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            staging.clone(),
        );

        let result = materialize_cid_tree_descriptor(None, &tree, "hostile", false).await;
        assert!(matches!(result, Err(MaterializeError::Invalid)));
        assert!(!root.path().join("escaped").exists());
        assert_eq!(std::fs::read_dir(staging).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cid_tree_file_materialization_uses_canonical_cid_path() {
        let content = b"confined file content";
        let (cid, pinner) = test_cid_and_pinner(content);
        let escape_root = tempfile::TempDir::new().unwrap();
        let escaped_parent = escape_root.path().join("escaped");
        let escaped_path = escaped_parent.join("ipfs").join(cid.to_string());
        let hostile_cid = format!("{}/ipfs/{cid}", escaped_parent.display());

        let tree_staging = tempfile::TempDir::new().unwrap();
        let entries = vec![crate::vfs::DirEntry {
            name: "hostile-file".to_string(),
            cid: hostile_cid,
            entry_type: crate::vfs::EntryType::File,
            size: content.len() as u64,
        }];
        std::fs::write(
            tree_staging.path().join(format!("{cid}.dirlist.json")),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let tree = CidTree::new(
            cid.to_string(),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            tree_staging.path().to_path_buf(),
        );
        let cache = cache::CacheMode::Isolated(cache::IsolatedPinset::new(pinner).unwrap());
        let canonical_path = cache.staging_dir().join(cid.to_string());

        let descriptor =
            materialize_cid_tree_descriptor(Some(&cache), &tree, "hostile-file", false)
                .await
                .unwrap();
        drop(descriptor);

        assert_eq!(std::fs::read(&canonical_path).unwrap(), content);
        assert!(
            !escaped_path.exists(),
            "a parseable CID string must not escape the cache staging directory"
        );
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
        cache_mode: Option<Arc<cache::CacheMode>>,
        cid_tree: Option<Arc<CidTree>>,
        writable_descriptors: std::collections::HashSet<u32>,
    }

    impl TestHarness {
        fn new(cache_mode: Option<cache::CacheMode>) -> Self {
            Self {
                wasi_ctx: wasmtime_wasi::WasiCtxBuilder::new().build(),
                resource_table: wasmtime::component::ResourceTable::new(),
                cache_mode: cache_mode.map(Arc::new),
                cid_tree: None,
                writable_descriptors: std::collections::HashSet::new(),
            }
        }

        fn view(&mut self) -> IpfsFilesystemView<'_> {
            IpfsFilesystemView {
                ctx: self.wasi_ctx.filesystem(),
                table: &mut self.resource_table,
                cache_mode: &self.cache_mode,
                cid_tree: &self.cid_tree,
                writable_descriptors: &mut self.writable_descriptors,
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
