//! WASI filesystem interceptor for `/ipfs/` paths and CidTree-backed virtual FS.
//!
//! When a `CidTree` is present (virtual mode), `open-at` resolves paths lazily
//! through the content-addressed tree. File content is materialized to a staging
//! directory on demand, then opened as a real `cap-std` file descriptor so all
//! subsequent descriptor operations delegate to wasmtime-wasi's standard impl.
//!
//! When no CidTree is present, falls back to the original behavior: intercepts
//! only explicit `/ipfs/<CID>/…` paths via the pinset cache.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::proc::ComponentRunStates;
use crate::vfs::{CidTree, ResolveError, ResolvedNode};
use anyhow::Result;
use wasmtime::component::{HasData, Linker, Resource};
use wasmtime_wasi::filesystem::{WasiFilesystemCtx, WasiFilesystemCtxView};

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
    pub descriptors: &'a mut DescriptorContexts,
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
        descriptors: &mut state.fs_descriptors,
    }
}

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
///
/// The CID spelling occupies one path segment. Bare-CID boundaries can accept
/// multibase spellings that contain `/`, but this path grammar does not add
/// escaping, percent decoding, or longest-prefix framing for those spellings.
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

    let cid = ipfs::cid_identity::parse_cid(cid_str).ok()?;
    Some(IpfsCidPath {
        cid,
        subpath: subpath.to_string(),
    })
}

/// Virtual identity is independent of host materialization paths and current roots.
#[derive(Clone)]
pub(crate) struct CidDirectoryContext {
    tree: Arc<CidTree>,
    root: Arc<cid::Cid>,
    path: String,
}

/// Metadata follows resource-table ownership. Only preopened `/` descriptors
/// can select the global IPFS route; returned directories stay in their namespace.
#[derive(Default)]
pub(crate) struct DescriptorContexts {
    directories: HashMap<u32, CidDirectoryContext>,
    roots: HashSet<u32>,
    writable: HashSet<u32>,
}

impl DescriptorContexts {
    fn remove(&mut self, id: u32) {
        self.directories.remove(&id);
        self.roots.remove(&id);
        self.writable.remove(&id);
    }
}

enum OpenRoute {
    CidTree(CidDirectoryContext, String),
    Ipfs(IpfsCidPath),
    Wasi { writable: bool },
}

impl IpfsFilesystemView<'_> {
    fn route_open(
        &self,
        descriptor: &Resource<wasmtime_wasi::filesystem::Descriptor>,
        path: &str,
    ) -> P3FilesystemResult<OpenRoute> {
        // Validate the actual resource before consulting metadata or flags.
        if !matches!(
            self.table.get(descriptor)?,
            wasmtime_wasi::filesystem::Descriptor::Dir(_)
        ) {
            return Err(p3_types::ErrorCode::NotDirectory.into());
        }
        let id = descriptor.rep();
        if self.descriptors.writable.contains(&id) {
            return Ok(OpenRoute::Wasi { writable: true });
        }
        let root = self.descriptors.roots.contains(&id);
        if let Some(context) = self.descriptors.directories.get(&id) {
            if root {
                if let Some(parsed) = parse_ipfs_path(path) {
                    if parsed.cid != *context.root {
                        return Ok(OpenRoute::Ipfs(parsed));
                    }
                    return Ok(OpenRoute::CidTree(context.clone(), parsed.subpath));
                }
            }
            return Ok(OpenRoute::CidTree(context.clone(), path.to_string()));
        }
        if root {
            if let Some(parsed) = parse_ipfs_path(path) {
                return Ok(OpenRoute::Ipfs(parsed));
            }
        }
        Ok(OpenRoute::Wasi { writable: false })
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
    NotDirectory,
}

impl From<MaterializeError> for p3_types::ErrorCode {
    fn from(error: MaterializeError) -> Self {
        match error {
            MaterializeError::Invalid => Self::Invalid,
            MaterializeError::Io => Self::Io,
            MaterializeError::NoEntry => Self::NoEntry,
            MaterializeError::NotPermitted => Self::NotPermitted,
            MaterializeError::NotDirectory => Self::NotDirectory,
        }
    }
}

#[cfg(test)]
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
        resolution_error(error)
    })?;

    materialize_resolved_descriptor(cache, cid_tree, resolved).await
}

fn resolution_error(error: anyhow::Error) -> MaterializeError {
    match error.downcast_ref::<ResolveError>() {
        Some(ResolveError::NoEntry) => MaterializeError::NoEntry,
        Some(ResolveError::NotDirectory) => MaterializeError::NotDirectory,
        Some(ResolveError::InvalidPath | ResolveError::InvalidCid) => MaterializeError::Invalid,
        None => MaterializeError::Io,
    }
}

async fn materialize_resolved_descriptor(
    cache: Option<&cache::CacheMode>,
    cid_tree: &CidTree,
    resolved: ResolvedNode,
) -> Result<wasmtime_wasi::filesystem::Descriptor, MaterializeError> {
    match resolved {
        ResolvedNode::CidFile { cid, .. } => {
            let cache = cache.ok_or(MaterializeError::Io)?;
            let canonical_cid = cid.to_string();
            cache.ensure(&cid).await.map_err(|error| {
                tracing::warn!(%canonical_cid, %error, "CidTree cache ensure failed");
                MaterializeError::Io
            })?;
            let staging_path = cache.staging_dir().join(&canonical_cid);
            if !staging_path.exists() {
                cache
                    .fetch_to_path(&cid, &staging_path)
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
            let canonical_cid = cid.to_string();
            let staging_dir = cid_tree.staging_dir().join(format!("dir-{canonical_cid}"));
            if !staging_dir.exists() {
                let entries = cid_tree.ls_dir(&cid).await.map_err(|error| {
                    tracing::warn!(%canonical_cid, %error, "CidTree directory listing failed");
                    MaterializeError::Io
                })?;
                // The reusable path must never be the construction path. A unique
                // sibling keeps rename on one filesystem and isolates other builders.
                std::fs::create_dir_all(cid_tree.staging_dir())
                    .map_err(|_| MaterializeError::Io)?;
                let temporary = tempfile::Builder::new()
                    .prefix(".cid-dir-")
                    .tempdir_in(cid_tree.staging_dir())
                    .map_err(|_| MaterializeError::Io)?;
                for entry in entries {
                    if !is_confined_relative_path(&entry.name)
                        || std::path::Path::new(&entry.name).components().count() != 1
                    {
                        tracing::warn!(name = %entry.name, "rejected unconfined CidTree entry");
                        return Err(MaterializeError::Invalid);
                    }
                    let entry_path = temporary.path().join(&entry.name);
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
                    #[cfg(test)]
                    tests::directory_build_hook(temporary.path(), tests::BuildStage::Entry)?;
                }
                #[cfg(test)]
                tests::directory_build_hook(temporary.path(), tests::BuildStage::BeforePublish)?;

                // Linux RENAME_NOREPLACE / macOS RENAME_EXCL preserve the first
                // completed directory, including an empty one. `exists()` above
                // is only a fast path; the filesystem arbitrates publication
                // across CidTree instances and processes. Unsupported filesystems
                // fail closed rather than fall back to a replacing rename.
                use rustix::fs::{renameat_with, RenameFlags, CWD};
                match renameat_with(
                    CWD,
                    temporary.path(),
                    CWD,
                    &staging_dir,
                    RenameFlags::NOREPLACE,
                ) {
                    Ok(()) => {
                        // Rename freed the old name. Disarm cleanup before another
                        // builder can reuse it; this builder no longer owns it.
                        let _former_path = temporary.keep();
                        #[cfg(test)]
                        tests::directory_build_hook(&_former_path, tests::BuildStage::Published)?;
                    }
                    Err(rustix::io::Errno::EXIST) if staging_dir.is_dir() => {}
                    Err(_) => return Err(MaterializeError::Io),
                }
                // On failure or a lost race, TempDir removes only this builder's
                // unpublished state. Success disarms cleanup of the freed name.
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
        let id = descriptor.rep();
        p3_types::HostDescriptor::drop(&mut self.as_wasi_view(), descriptor)?;
        self.descriptors.remove(id);
        Ok(())
    }
}

impl p3_preopens::Host for IpfsFilesystemView<'_> {
    fn get_directories(
        &mut self,
    ) -> wasmtime::Result<Vec<(Resource<wasmtime_wasi::filesystem::Descriptor>, String)>> {
        let directories = p3_preopens::Host::get_directories(&mut self.as_wasi_view())?;
        for (descriptor, path) in &directories {
            let id = descriptor.rep();
            self.descriptors.remove(id);
            if path == "/tmp" {
                self.descriptors.writable.insert(id);
            } else if path == "/" {
                self.descriptors.roots.insert(id);
                if let Some(tree) = self.cid_tree {
                    self.descriptors.directories.insert(
                        id,
                        CidDirectoryContext {
                            tree: Arc::clone(tree),
                            root: tree.root_cid(),
                            path: String::new(),
                        },
                    );
                }
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
        let (cache, route) = store.with(|mut access| {
            let view = access.get();
            Ok::<_, P3FilesystemError>((
                view.cache_mode.clone(),
                view.route_open(&descriptor, &path)?,
            ))
        })?;
        if let OpenRoute::Wasi { writable } = route {
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
                let view = access.get();
                view.descriptors.remove(opened.rep());
                if writable {
                    view.descriptors.writable.insert(opened.rep());
                }
            });
            return Ok(opened);
        }

        // Immutable intent is rejected before resolving any target or changing staging.
        if flags.intersects(
            p3_types::DescriptorFlags::WRITE | p3_types::DescriptorFlags::MUTATE_DIRECTORY,
        ) || open_flags.intersects(
            p3_types::OpenFlags::CREATE
                | p3_types::OpenFlags::EXCLUSIVE
                | p3_types::OpenFlags::TRUNCATE,
        ) {
            return Err(p3_types::ErrorCode::NotPermitted.into());
        }
        let (opened, context) = match route {
            OpenRoute::CidTree(mut context, target) => {
                let resolved = context
                    .tree
                    .resolve_at(&context.root, &context.path, &target)
                    .await
                    .map_err(resolution_error)
                    .map_err(p3_types::ErrorCode::from)?;
                let directory = matches!(resolved.node, ResolvedNode::CidDir { .. });
                if !directory && open_flags.contains(p3_types::OpenFlags::DIRECTORY) {
                    return Err(p3_types::ErrorCode::NotDirectory.into());
                }
                let opened =
                    materialize_resolved_descriptor(cache.as_deref(), &context.tree, resolved.node)
                        .await
                        .map_err(p3_types::ErrorCode::from)?;
                context.path = resolved.path;
                (opened, directory.then_some(context))
            }
            OpenRoute::Ipfs(ipfs_path) => {
                let opened = materialize_ipfs_descriptor(cache.as_deref(), &ipfs_path, false)
                    .await
                    .map_err(p3_types::ErrorCode::from)?;
                if (open_flags.contains(p3_types::OpenFlags::DIRECTORY) || path.ends_with('/'))
                    && !matches!(opened, wasmtime_wasi::filesystem::Descriptor::Dir(_))
                {
                    return Err(p3_types::ErrorCode::NotDirectory.into());
                }
                (opened, None)
            }
            OpenRoute::Wasi { .. } => unreachable!(),
        };
        store.with(|mut access| {
            let view = access.get();
            let opened = view.table.push(opened)?;
            view.descriptors.remove(opened.rep());
            if let Some(context) = context {
                view.descriptors.directories.insert(opened.rep(), context);
            }
            Ok(opened)
        })
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum BuildStage {
        Entry,
        BeforePublish,
        Published,
    }

    type DirectoryBuildHook =
        Box<dyn FnMut(&std::path::Path, BuildStage) -> Result<(), MaterializeError>>;
    thread_local! {
        static DIRECTORY_BUILD_HOOK: std::cell::RefCell<Option<DirectoryBuildHook>> =
            const { std::cell::RefCell::new(None) };
    }

    // Materialization performs no awaits between tempdir creation and publish.
    // Tests install hooks only on dedicated threads or current-thread runtimes.
    pub(super) fn directory_build_hook(
        path: &std::path::Path,
        stage: BuildStage,
    ) -> Result<(), MaterializeError> {
        DIRECTORY_BUILD_HOOK.with_borrow_mut(|hook| match hook {
            Some(hook) => hook(path, stage),
            None => Ok(()),
        })
    }

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
    fn root_route_rejects_noncanonical_binary_cid_aliases() {
        let cid: cid::Cid = DIRECTORY_CID.parse().unwrap();
        let mut bytes = cid.to_bytes();
        bytes.push(0);
        let alias = cid::multibase::encode(cid::multibase::Base::Base32Lower, bytes);
        assert!(parse_ipfs_path(&format!("ipfs/{alias}/child")).is_none());
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
            ipfs::cid_identity::parse_cid(cid).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            staging.path().to_path_buf(),
        );

        let result = materialize_cid_tree_descriptor(None, &tree, "", false).await;
        assert!(matches!(result, Err(MaterializeError::Invalid)));
        assert!(!staging.path().join("escaped").exists());
    }

    #[tokio::test]
    async fn cid_tree_later_invalid_entry_leaves_no_published_directory() {
        let cid = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
        let staging = tempfile::TempDir::new().unwrap();
        let entries = vec![
            crate::vfs::DirEntry {
                name: "first".to_string(),
                cid: cid.to_string(),
                entry_type: crate::vfs::EntryType::File,
                size: 7,
            },
            crate::vfs::DirEntry {
                name: "../escaped".to_string(),
                cid: cid.to_string(),
                entry_type: crate::vfs::EntryType::File,
                size: 1,
            },
        ];
        std::fs::write(
            staging.path().join(format!("{cid}.dirlist.json")),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let tree = CidTree::new(
            ipfs::cid_identity::parse_cid(cid).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            staging.path().to_path_buf(),
        );
        let observed = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed_in_hook = observed.clone();
        let final_path = staging.path().join(format!("dir-{cid}"));
        let hook = BuildHookGuard::install(move |temporary, stage| {
            assert!(stage == BuildStage::Entry);
            assert_eq!(std::fs::metadata(temporary.join("first")).unwrap().len(), 7);
            assert!(!final_path.exists());
            observed_in_hook.set(true);
            Ok(())
        });
        let result = materialize_cid_tree_descriptor(None, &tree, "", false).await;
        drop(hook);
        assert!(observed.get(), "the earlier valid entry must be populated");
        assert!(matches!(result, Err(MaterializeError::Invalid)));
        assert!(!staging.path().join(format!("dir-{cid}")).exists());
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cid_tree_materialization_propagates_malformed_directory_listing() {
        let cid = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
        let staging = tempfile::TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
        });
        let tree = CidTree::new(
            ipfs::cid_identity::parse_cid(cid).unwrap(),
            ipfs::HttpClient::new(format!("http://{address}")),
            staging.path().to_path_buf(),
        );

        let result = materialize_cid_tree_descriptor(None, &tree, "", false).await;
        server.await.unwrap();

        assert!(matches!(result, Err(MaterializeError::Io)));
        assert!(!staging.path().join(format!("dir-{cid}")).exists());
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
            ipfs::cid_identity::parse_cid(root_cid).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            staging.clone(),
        );

        let result = materialize_cid_tree_descriptor(None, &tree, "hostile", false).await;
        assert!(matches!(result, Err(MaterializeError::Invalid)));
        assert!(!root.path().join("escaped").exists());
        assert_eq!(std::fs::read_dir(staging).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cid_tree_rejects_path_bearing_child_before_cache_effects() {
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
            cid,
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            tree_staging.path().to_path_buf(),
        );
        let cache = cache::CacheMode::Isolated(cache::IsolatedPinset::new(pinner).unwrap());
        let canonical_path = cache.staging_dir().join(cid.to_string());

        let result =
            materialize_cid_tree_descriptor(Some(&cache), &tree, "hostile-file", false).await;

        assert!(
            matches!(result, Err(MaterializeError::Invalid)),
            "path-bearing child metadata must be rejected"
        );
        assert!(!canonical_path.exists());
        assert_eq!(std::fs::read_dir(cache.staging_dir()).unwrap().count(), 0);
        assert!(
            !escaped_path.exists(),
            "a parseable CID string must not escape the cache staging directory"
        );
    }

    const DIRECTORY_CID: &str = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";

    #[tokio::test]
    async fn valid_alternate_child_cid_stages_canonical_file() {
        let content = b"alias file";
        let cid = cid::Cid::new_v1(
            0x55,
            cid::multihash::Multihash::<64>::wrap(0x12, &[255; 32]).unwrap(),
        );
        let alias = cid.to_string_of_base(cid::multibase::Base::Base64).unwrap();
        assert!(alias.contains('/'));
        let staging = tempfile::tempdir().unwrap();
        let entries = [crate::vfs::DirEntry {
            name: "child".into(),
            cid: alias,
            entry_type: crate::vfs::EntryType::File,
            size: content.len() as u64,
        }];
        std::fs::write(
            staging.path().join(format!("{cid}.dirlist.json")),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let tree = CidTree::new(
            cid,
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
            staging.path().into(),
        );
        let pinner = Arc::new(MockPinner {
            data: HashMap::from([(cid, content.to_vec())]),
            path_data: HashMap::new(),
        });
        let cache = cache::CacheMode::Isolated(cache::IsolatedPinset::new(pinner).unwrap());
        materialize_cid_tree_descriptor(Some(&cache), &tree, "child", false)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(cache.staging_dir().join(cid.to_string())).unwrap(),
            content
        );
        assert_eq!(std::fs::read_dir(cache.staging_dir()).unwrap().count(), 1);
    }

    struct BuildHookGuard;

    impl BuildHookGuard {
        fn install(
            hook: impl FnMut(&std::path::Path, BuildStage) -> Result<(), MaterializeError> + 'static,
        ) -> Self {
            DIRECTORY_BUILD_HOOK.with_borrow_mut(|slot| {
                assert!(slot.is_none());
                *slot = Some(Box::new(hook));
            });
            Self
        }
    }

    impl Drop for BuildHookGuard {
        fn drop(&mut self) {
            DIRECTORY_BUILD_HOOK.with_borrow_mut(|slot| *slot = None);
        }
    }

    fn directory_entries() -> Vec<crate::vfs::DirEntry> {
        vec![
            crate::vfs::DirEntry {
                name: "first".into(),
                cid: DIRECTORY_CID.into(),
                entry_type: crate::vfs::EntryType::File,
                size: 7,
            },
            crate::vfs::DirEntry {
                name: "nested".into(),
                cid: DIRECTORY_CID.into(),
                entry_type: crate::vfs::EntryType::Dir,
                size: 0,
            },
        ]
    }

    fn cached_directory_tree(
        staging: &std::path::Path,
        entries: &[crate::vfs::DirEntry],
    ) -> CidTree {
        std::fs::write(
            staging.join(format!("{DIRECTORY_CID}.dirlist.json")),
            serde_json::to_vec(entries).unwrap(),
        )
        .unwrap();
        CidTree::new(
            ipfs::cid_identity::parse_cid(DIRECTORY_CID).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
            staging.to_path_buf(),
        )
    }

    fn assert_complete_directory(path: &std::path::Path, empty: bool) {
        if empty {
            assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
        } else {
            assert_eq!(std::fs::read_dir(path).unwrap().count(), 2);
            assert_eq!(std::fs::metadata(path.join("first")).unwrap().len(), 7);
            assert!(path.join("nested").is_dir());
        }
    }

    #[tokio::test]
    async fn cid_tree_population_failure_is_unpublished_and_retryable() {
        let staging = tempfile::TempDir::new().unwrap();
        let tree = cached_directory_tree(staging.path(), &directory_entries());
        let final_path = staging.path().join(format!("dir-{DIRECTORY_CID}"));
        let observed = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed_in_hook = observed.clone();
        let final_in_hook = final_path.clone();
        let hook = BuildHookGuard::install(move |temporary, stage| {
            assert!(stage == BuildStage::Entry);
            // A real stub has been populated. Obstruct the next mkdir to cause
            // a real filesystem error, without corrupting the cached listing.
            assert_eq!(std::fs::metadata(temporary.join("first")).unwrap().len(), 7);
            assert!(!final_in_hook.exists());
            std::fs::write(temporary.join("nested"), b"obstruction").unwrap();
            observed_in_hook.set(true);
            Ok(())
        });
        let result = materialize_cid_tree_descriptor(None, &tree, "", false).await;
        drop(hook);
        assert!(observed.get());
        assert!(matches!(result, Err(MaterializeError::Io)));
        assert!(!final_path.exists());
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 1);

        // The same tree retries from its valid cached listing, in new state.
        let final_in_hook = final_path.clone();
        let hook = BuildHookGuard::install(move |_, stage| {
            assert_eq!(final_in_hook.exists(), stage == BuildStage::Published);
            Ok(())
        });
        let descriptor = materialize_cid_tree_descriptor(None, &tree, "", false)
            .await
            .unwrap();
        drop(hook);
        let wasmtime_wasi::filesystem::Descriptor::Dir(dir) = descriptor else {
            panic!("expected directory descriptor");
        };
        assert_eq!(dir.perms, wasmtime_wasi::FsPerms::ReadOnly);
        assert_eq!(dir.open_mode, wasmtime_wasi::OpenMode::READ);
        assert_complete_directory(&final_path, false);
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 2);
    }

    // Both independent trees reach partial construction and completed private
    // construction before either may publish. Channels then select a winner.
    fn concurrent_directory_publication(empty: bool) {
        use std::os::unix::fs::MetadataExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let staging = tempfile::TempDir::new().unwrap();
        let entries = if empty { vec![] } else { directory_entries() };
        let first_tree = cached_directory_tree(staging.path(), &entries);
        let second_tree = CidTree::new(
            ipfs::cid_identity::parse_cid(DIRECTORY_CID).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
            staging.path().to_path_buf(),
        );
        let final_path = staging.path().join(format!("dir-{DIRECTORY_CID}"));
        let (events_tx, events_rx) = mpsc::channel();
        let mut gates = Vec::new();
        let mut workers = Vec::new();
        for (id, tree) in [first_tree, second_tree].into_iter().enumerate() {
            let (gate_tx, gate_rx) = mpsc::channel();
            gates.push(gate_tx);
            let events_tx = events_tx.clone();
            workers.push(std::thread::spawn(move || {
                let mut first_entry = true;
                let _hook = BuildHookGuard::install(move |temporary, stage| {
                    if stage == BuildStage::Published {
                        return Ok(());
                    }
                    let before_publish = stage == BuildStage::BeforePublish;
                    if first_entry || before_publish {
                        first_entry = false;
                        events_tx
                            .send((id, temporary.to_path_buf(), before_publish))
                            .unwrap();
                        // Timeout only bounds a broken test; channels establish ordering.
                        gate_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    }
                    Ok(())
                });
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let descriptor = runtime
                    .block_on(materialize_cid_tree_descriptor(None, &tree, "", false))
                    .unwrap();
                let wasmtime_wasi::filesystem::Descriptor::Dir(dir) = descriptor else {
                    panic!("expected directory descriptor");
                };
                dir.dir.metadata().unwrap().ino()
            }));
        }
        drop(events_tx);
        let mut temporary_paths = Vec::new();
        for _ in 0..2 {
            let (_, temporary, before_publish) =
                events_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(before_publish, empty);
            assert!(!final_path.exists());
            if !empty {
                assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 1);
                assert_eq!(std::fs::metadata(temporary.join("first")).unwrap().len(), 7);
            }
            temporary_paths.push(temporary);
        }
        assert_ne!(temporary_paths[0], temporary_paths[1]);
        if !empty {
            for gate in &gates {
                gate.send(()).unwrap();
            }
            for _ in 0..2 {
                let (_, temporary, before_publish) =
                    events_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                assert!(before_publish);
                assert!(!final_path.exists());
                assert_complete_directory(&temporary, false);
            }
        }
        gates[0].send(()).unwrap();
        let winner_inode = workers.remove(0).join().unwrap();
        assert_complete_directory(&final_path, empty);
        assert_eq!(std::fs::metadata(&final_path).unwrap().ino(), winner_inode);
        gates[1].send(()).unwrap();
        let loser_inode = workers.remove(0).join().unwrap();
        assert_eq!(
            winner_inode, loser_inode,
            "loser must open the winner, even for an empty directory"
        );
        assert_eq!(std::fs::metadata(&final_path).unwrap().ino(), winner_inode);
        assert_complete_directory(&final_path, empty);
        for temporary in temporary_paths {
            assert!(!temporary.exists());
        }
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 2);
    }

    #[test]
    fn cid_tree_concurrent_builders_publish_one_complete_directory() {
        concurrent_directory_publication(false);
    }

    #[test]
    fn cid_tree_concurrent_builders_preserve_empty_winner() {
        concurrent_directory_publication(true);
    }

    #[tokio::test]
    async fn cid_tree_reuses_published_directory_without_population() {
        use std::os::unix::fs::MetadataExt;
        let staging = tempfile::TempDir::new().unwrap();
        let tree = cached_directory_tree(staging.path(), &directory_entries());
        materialize_cid_tree_descriptor(None, &tree, "", false)
            .await
            .unwrap();
        let final_path = staging.path().join(format!("dir-{DIRECTORY_CID}"));
        let inode = std::fs::metadata(&final_path).unwrap().ino();
        std::fs::remove_file(staging.path().join(format!("{DIRECTORY_CID}.dirlist.json"))).unwrap();
        // A new tree has no LRU; its backend is unavailable. Final reuse must
        // not require listing or touch any published entry.
        let tree = CidTree::new(
            ipfs::cid_identity::parse_cid(DIRECTORY_CID).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
            staging.path().into(),
        );
        let _hook = BuildHookGuard::install(|_, _| panic!("published directory was reconstructed"));
        let descriptor = materialize_cid_tree_descriptor(None, &tree, "", false)
            .await
            .unwrap();
        let wasmtime_wasi::filesystem::Descriptor::Dir(dir) = descriptor else {
            panic!("expected directory");
        };
        assert_eq!(dir.dir.metadata().unwrap().ino(), inode);
        assert_complete_directory(&final_path, false);
        assert!(matches!(
            materialize_cid_tree_descriptor(None, &tree, "", true).await,
            Err(MaterializeError::NotPermitted)
        ));
    }

    #[tokio::test]
    async fn cid_tree_success_does_not_clean_reused_temporary_name() {
        let staging = tempfile::TempDir::new().unwrap();
        let tree = cached_directory_tree(staging.path(), &directory_entries());
        let reused_path = std::rc::Rc::new(std::cell::RefCell::new(None));
        let reused_in_hook = reused_path.clone();
        let _hook = BuildHookGuard::install(move |temporary, stage| {
            if stage == BuildStage::Published {
                // Publication frees this pathname. Simulate another builder
                // acquiring it before the successful publisher drops TempDir.
                std::fs::create_dir(temporary).unwrap();
                std::fs::write(temporary.join("other-builder"), b"in progress").unwrap();
                *reused_in_hook.borrow_mut() = Some(temporary.to_path_buf());
            }
            Ok(())
        });
        materialize_cid_tree_descriptor(None, &tree, "", false)
            .await
            .unwrap();
        let reused = reused_path.borrow().clone().expect("publication hook ran");
        assert_eq!(
            std::fs::read(reused.join("other-builder")).unwrap(),
            b"in progress"
        );
        assert_complete_directory(&staging.path().join(format!("dir-{DIRECTORY_CID}")), false);
    }

    #[tokio::test]
    async fn cid_tree_directory_materialization_uses_canonical_cid_path() {
        let canonical = "bafkreibm6jg3ux5quy7flfgn5gmxk5ubm6yur3apcu3to3d6tmjzptm2ye";
        let alternate = canonical
            .parse::<cid::Cid>()
            .unwrap()
            .to_string_of_base(cid::multibase::Base::Base58Btc)
            .unwrap();
        let staging = tempfile::TempDir::new().unwrap();
        std::fs::write(
            staging.path().join(format!("{canonical}.dirlist.json")),
            b"[]",
        )
        .unwrap();
        let tree = CidTree::new(
            ipfs::cid_identity::parse_cid(&alternate).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
            staging.path().into(),
        );
        materialize_cid_tree_descriptor(None, &tree, "", false)
            .await
            .unwrap();
        assert!(staging.path().join(format!("dir-{canonical}")).is_dir());
        assert!(!staging.path().join(format!("dir-{alternate}")).exists());
        use std::os::unix::fs::MetadataExt;
        let inode = std::fs::metadata(staging.path().join(format!("dir-{canonical}")))
            .unwrap()
            .ino();
        std::fs::remove_file(staging.path().join(format!("{canonical}.dirlist.json"))).unwrap();
        let tree = CidTree::new(
            ipfs::cid_identity::parse_cid(canonical).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
            staging.path().into(),
        );
        let _hook = BuildHookGuard::install(|_, _| panic!("alias caused duplicate publication"));
        let descriptor = materialize_cid_tree_descriptor(None, &tree, "", false)
            .await
            .unwrap();
        let wasmtime_wasi::filesystem::Descriptor::Dir(dir) = descriptor else {
            panic!("expected directory")
        };
        assert_eq!(dir.dir.metadata().unwrap().ino(), inode);
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cid_tree_listing_http_failure_can_retry() {
        let staging = tempfile::TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (status, body) in [
                ("500 Internal Server Error", "failure".to_string()),
                (
                    "200 OK",
                    format!(
                        r#"{{"Objects":[{{"Links":[{{"Name":"first","Hash":"{DIRECTORY_CID}","Size":7,"Type":2}},{{"Name":"nested","Hash":"{DIRECTORY_CID}","Size":0,"Type":1}}]}}]}}"#
                    ),
                ),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let tree = CidTree::new(
            ipfs::cid_identity::parse_cid(DIRECTORY_CID).unwrap(),
            ipfs::HttpClient::new(format!("http://{address}")),
            staging.path().into(),
        );
        let result = materialize_cid_tree_descriptor(None, &tree, "", false).await;
        assert!(matches!(result, Err(MaterializeError::Io)));
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
        materialize_cid_tree_descriptor(None, &tree, "", false)
            .await
            .unwrap();
        assert_complete_directory(&staging.path().join(format!("dir-{DIRECTORY_CID}")), false);
        server.await.unwrap();
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

    #[tokio::test]
    async fn materialize_ipfs_fetches_read_only_content() {
        let content = b"hello ipfs world";
        let (cid, pinner) = test_cid_and_pinner(content);
        let cache = cache::CacheMode::Isolated(cache::IsolatedPinset::new(pinner).unwrap());
        let path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };

        let descriptor = materialize_ipfs_descriptor(Some(&cache), &path, false)
            .await
            .expect("materialize IPFS content");
        drop(descriptor);
        assert_eq!(
            std::fs::read(cache.staging_dir().join(cid.to_string())).unwrap(),
            content
        );
    }

    #[tokio::test]
    async fn materialize_ipfs_rejects_writes_and_missing_cache() {
        let (cid, pinner) = test_cid_and_pinner(b"data");
        let cache = cache::CacheMode::Isolated(cache::IsolatedPinset::new(pinner).unwrap());
        let path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };

        assert!(matches!(
            materialize_ipfs_descriptor(Some(&cache), &path, true).await,
            Err(MaterializeError::NotPermitted)
        ));
        assert!(matches!(
            materialize_ipfs_descriptor(None, &path, false).await,
            Err(MaterializeError::NoEntry)
        ));
    }

    #[tokio::test]
    async fn materialize_ipfs_fetches_subpath_content() {
        let root_bytes = b"root cid blob bytes";
        let nested_bytes = b"nested file content";
        let (cid, pinner) =
            test_cid_and_pinner_with_subpath(root_bytes, "sub/dir/file.txt", nested_bytes);
        let cache = cache::CacheMode::Isolated(cache::IsolatedPinset::new(pinner).unwrap());
        let path = IpfsCidPath {
            cid,
            subpath: "sub/dir/file.txt".to_string(),
        };

        let descriptor = materialize_ipfs_descriptor(Some(&cache), &path, false)
            .await
            .expect("materialize IPFS subpath");
        drop(descriptor);
        assert_eq!(
            std::fs::read(
                cache
                    .staging_dir()
                    .join(cid.to_string())
                    .join("sub/dir/file.txt")
            )
            .unwrap(),
            nested_bytes
        );
    }
}

#[cfg(test)]
#[path = "fs_intercept_tests.rs"]
mod descriptor_tests;
