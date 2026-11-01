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
use wasmtime_wasi::filesystem::{
    FilesystemHandle, FilesystemOperation as WasiFilesystemOperation, FilesystemOperationError,
    WasiFilesystemCtx, WasiFilesystemCtxView,
};

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
    pub filesystem_resources: &'a Arc<cache::CellResources>,
    pub filesystem_scope: &'a cache::FsScope,
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
        filesystem_resources: &state.filesystem_resources,
        filesystem_scope: &state.filesystem_scope,
        descriptors: &mut state.fs_descriptors,
    }
}

/// Ownership carried by Wasmtime for exactly one native filesystem operation
/// or P3 stream session. Field order is deliberate: L and F are released
/// before the final actual-work receipt can make the root cleanup-ready.
struct NativeFsLease {
    _logical: cache::LogicalIoPermit,
    _producer: cache::ForegroundProducerPermit,
    _work: cache::FsWorkToken,
}

fn admit_native_filesystem_operation(
    resources: &Arc<cache::CellResources>,
    filesystem_scope: &cache::FsScope,
) -> Result<WasiFilesystemOperation, FilesystemOperationError> {
    let logical = resources
        .try_logical_io()
        .map_err(|_| FilesystemOperationError::Rejected)?;
    let work = filesystem_scope
        .try_work()
        .map_err(|_| FilesystemOperationError::Rejected)?;
    let producer = resources
        .host()
        .try_foreground()
        .map_err(|_| FilesystemOperationError::Rejected)?;
    Ok(WasiFilesystemOperation::new(NativeFsLease {
        _logical: logical,
        _producer: producer,
        _work: work,
    }))
}

/// Install the zero-argument Wasmtime lifecycle provider without capturing a
/// Store, root owner, or Cell-local state.
pub(crate) fn install_native_filesystem_hook(
    builder: &mut wasmtime_wasi::WasiCtxBuilder,
    resources: Arc<cache::CellResources>,
    filesystem_scope: cache::FsScope,
) {
    builder.filesystem_operation_hook(move || {
        admit_native_filesystem_operation(&resources, &filesystem_scope)
    });
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
    lease: Option<cache::StagedContentLease>,
) -> Result<wasmtime_wasi::filesystem::Descriptor, MaterializeError> {
    use wasmtime_wasi::filesystem::{Descriptor, Dir, File};
    use wasmtime_wasi::{FsPerms, OpenMode};

    let metadata = std::fs::metadata(path).map_err(|_| MaterializeError::Io)?;
    if metadata.is_dir() {
        let dir = cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
            .map_err(|_| MaterializeError::Io)?;
        let native = dir.into_std_file();
        let native = match lease {
            Some(lease) => FilesystemHandle::new(native, lease),
            None => native.into(),
        };
        Ok(Descriptor::Dir(Dir::new(
            native,
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
        let native = file.into_std();
        let native = match lease {
            Some(lease) => FilesystemHandle::new(native, lease),
            None => native.into(),
        };
        Ok(Descriptor::File(File::new(
            native,
            FsPerms::ReadOnly,
            OpenMode::READ,
            false,
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MaterializeError {
    Busy(cache::AdmissionError),
    Cancelled,
    Invalid,
    Io,
    NoEntry,
    NotPermitted,
    NotDirectory,
}

impl From<MaterializeError> for p3_types::ErrorCode {
    fn from(error: MaterializeError) -> Self {
        match error {
            MaterializeError::Busy(_) => Self::Busy,
            MaterializeError::Cancelled => Self::Interrupted,
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
    let resources = cid_tree.test_cell_resources();
    let filesystem_scope = cid_tree.test_filesystem_scope();
    let logical = resources.try_logical_io().map_err(MaterializeError::Busy)?;
    let mut traversal = cid_tree
        .traversal_in(&filesystem_scope, logical)
        .map_err(MaterializeError::Busy)?;
    let resolved = cid_tree
        .resolve_path_in(&mut traversal, path)
        .await
        .map_err(resolution_error)?;
    let logical = traversal.into_logical().map_err(resolution_error)?;
    materialize_cid_tree_descriptor_in(
        &filesystem_scope,
        &resources,
        logical,
        cache,
        cid_tree,
        resolved,
    )
    .await?
    .install(|result| result)
}

async fn materialize_cid_tree_descriptor_in(
    filesystem_scope: &cache::FsScope,
    resources: &Arc<cache::CellResources>,
    logical: cache::LogicalIoPermit,
    cache: Option<&cache::CacheMode>,
    cid_tree: &CidTree,
    resolved: ResolvedNode,
) -> Result<
    cache::FsResultEnvelope<Result<wasmtime_wasi::filesystem::Descriptor, MaterializeError>>,
    MaterializeError,
> {
    materialize_resolved_descriptor(
        filesystem_scope,
        resources,
        logical,
        cache,
        cid_tree,
        resolved,
    )
    .await
}

fn resolution_error(error: anyhow::Error) -> MaterializeError {
    if let Some(error) = error.downcast_ref::<cache::AdmissionError>() {
        return MaterializeError::Busy(error.clone());
    }
    match error.downcast_ref::<ResolveError>() {
        Some(ResolveError::NoEntry) => MaterializeError::NoEntry,
        Some(ResolveError::NotDirectory) => MaterializeError::NotDirectory,
        Some(ResolveError::InvalidPath | ResolveError::InvalidCid) => MaterializeError::Invalid,
        None => MaterializeError::Io,
    }
}

fn materialization_error(error: anyhow::Error) -> MaterializeError {
    if let Some(error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<cache::AdmissionError>())
    {
        MaterializeError::Busy(error.clone())
    } else {
        MaterializeError::Io
    }
}

async fn materialize_cache_path(
    filesystem_scope: &cache::FsScope,
    logical: cache::LogicalIoPermit,
    cache: &cache::CacheMode,
    cid: &cid::Cid,
    subpath: Option<&str>,
) -> Result<
    (
        cache::ForegroundTransaction,
        std::path::PathBuf,
        Option<cache::StagedContentLease>,
    ),
    MaterializeError,
> {
    cache
        .ensure_in(filesystem_scope, &logical, cid)
        .await
        .map_err(materialization_error)?;
    match cache {
        cache::CacheMode::Shared(shared) => {
            let staged = shared
                .staged_content_in(filesystem_scope, logical, cid)
                .await
                .map_err(materialization_error)?;
            let path = shared
                .materialize_staged(&staged, subpath)
                .await
                .map_err(materialization_error)?;
            Ok((
                staged.transaction().continuation(),
                path,
                Some(staged.lease()),
            ))
        }
        cache::CacheMode::Isolated(pinset) => {
            let transaction = pinset
                .try_materialization_transaction(filesystem_scope, logical)
                .map_err(materialization_error)?;
            let root = pinset.staging_dir().join(cid.to_string());
            let path = subpath.map_or_else(|| root.clone(), |subpath| root.join(subpath));
            let check_path = path.clone();
            let exists = transaction
                .run_blocking(cache::FilesystemOperation::DescriptorOpen, move || {
                    check_path.exists()
                })
                .await
                .map_err(|_| MaterializeError::Io)?
                .into_inner();
            if !exists {
                match subpath {
                    Some(subpath) => {
                        pinset
                            .fetch_path_to_path_in(
                                filesystem_scope,
                                &transaction,
                                cid,
                                subpath,
                                &path,
                            )
                            .await
                    }
                    None => {
                        pinset
                            .fetch_to_path_in(filesystem_scope, &transaction, cid, &path)
                            .await
                    }
                }
                .map_err(materialization_error)?;
            }
            Ok((transaction, path, None))
        }
    }
}

async fn materialize_resolved_descriptor(
    filesystem_scope: &cache::FsScope,
    resources: &Arc<cache::CellResources>,
    logical: cache::LogicalIoPermit,
    cache: Option<&cache::CacheMode>,
    cid_tree: &CidTree,
    resolved: ResolvedNode,
) -> Result<
    cache::FsResultEnvelope<Result<wasmtime_wasi::filesystem::Descriptor, MaterializeError>>,
    MaterializeError,
> {
    match resolved {
        ResolvedNode::CidFile { cid, .. } => {
            let cache = cache.ok_or(MaterializeError::Io)?;
            let canonical_cid = cid.to_string();
            let (transaction, staging_path, lease) = materialize_cache_path(
                filesystem_scope,
                logical,
                cache,
                &cid,
                None,
            )
            .await
            .map_err(|error| {
                tracing::warn!(%canonical_cid, ?error, "CidTree cache materialization failed");
                error
            })?;
            transaction
                .run_blocking(cache::FilesystemOperation::DescriptorOpen, move || {
                    if !staging_path.exists() {
                        return Err(MaterializeError::Io);
                    }
                    open_read_only_path(&staging_path, lease)
                })
                .await
                .map_err(|_| MaterializeError::Io)
        }
        ResolvedNode::CidDir { cid } => {
            let cancellation = filesystem_scope.cancellation();
            let transaction = resources
                .host()
                .try_cleanup_transaction(logical, &[filesystem_scope])
                .map_err(MaterializeError::Busy)?;
            let canonical_cid = cid.to_string();
            let staging_dir = cid_tree.staging_dir().join(format!("dir-{canonical_cid}"));
            let check_path = staging_dir.clone();
            let exists = transaction
                .run_blocking(cache::FilesystemOperation::DescriptorOpen, move || {
                    check_path.exists()
                })
                .await
                .map_err(|_| MaterializeError::Io)?
                .into_inner();
            if !exists {
                let entries = cid_tree
                    .ls_dir_under(&transaction, &cid)
                    .await
                    .map_err(|error| {
                        tracing::warn!(%canonical_cid, %error, "CidTree directory listing failed");
                        materialization_error(error)
                    })?;
                let staging_root = cid_tree.staging_dir().to_owned();
                let publish_path = staging_dir.clone();
                #[cfg(test)]
                let build_hook = tests::take_directory_build_hook();
                #[cfg(test)]
                let cleanup_hook = tests::take_directory_cleanup_hook();
                let build_scope = filesystem_scope.clone();
                let build_cancellation = cancellation.clone();
                submit_directory_build(&transaction, filesystem_scope, &cancellation, move || {
                    build_directory(
                        staging_root,
                        publish_path,
                        entries,
                        build_scope,
                        build_cancellation,
                        #[cfg(test)]
                        build_hook,
                        #[cfg(test)]
                        cleanup_hook,
                    )
                })
                .await?;
            }
            check_directory_cancellation(&cancellation)?;
            #[cfg(any(test, feature = "test-support"))]
            let descriptor_scope = filesystem_scope.clone();
            transaction
                .run_blocking(cache::FilesystemOperation::DescriptorOpen, move || {
                    check_directory_cancellation(&cancellation)?;
                    #[cfg(any(test, feature = "test-support"))]
                    descriptor_scope.test_event(
                        cache::FilesystemOperation::DescriptorOpen,
                        cache::FsTestStage::Started,
                        None,
                    );
                    open_read_only_path(&staging_dir, None)
                })
                .await
                .map_err(|_| MaterializeError::Io)
        }
    }
}

async fn submit_directory_build(
    transaction: &cache::ForegroundTransaction,
    scope: &cache::FsScope,
    cancellation: &cache::FsCancellation,
    build: impl FnOnce() -> Result<(), MaterializeError> + Send + 'static,
) -> Result<(), MaterializeError> {
    check_directory_cancellation(cancellation)?;
    transaction
        .run_directory_build(scope, build)
        .await
        .map_err(|_| MaterializeError::Io)?
        .into_inner()
}

fn check_directory_cancellation(
    cancellation: &cache::FsCancellation,
) -> Result<(), MaterializeError> {
    if cancellation.is_cancelled() {
        Err(MaterializeError::Cancelled)
    } else {
        Ok(())
    }
}

// Explicit rollback also runs on unwind. Published content is disarmed before
// any later work so a reused private pathname or another winner is never removed.
struct DirectoryTemporary<'a> {
    directory: Option<tempfile::TempDir>,
    scope: &'a cache::FsScope,
    #[cfg(test)]
    cleanup_hook: Option<tests::DirectoryCleanupHook>,
}

impl DirectoryTemporary<'_> {
    fn path(&self) -> &std::path::Path {
        self.directory
            .as_ref()
            .expect("private directory published")
            .path()
    }
}

impl Drop for DirectoryTemporary<'_> {
    fn drop(&mut self) {
        if let Some(temporary) = self.directory.take() {
            cleanup_temporary_directory(
                self.scope,
                temporary,
                #[cfg(test)]
                &mut self.cleanup_hook,
            );
        }
    }
}

fn build_directory(
    staging_root: std::path::PathBuf,
    staging_dir: std::path::PathBuf,
    entries: Vec<crate::vfs::DirEntry>,
    filesystem_scope: cache::FsScope,
    cancellation: cache::FsCancellation,
    #[cfg(test)] mut build_hook: Option<tests::DirectoryBuildHook>,
    #[cfg(test)] cleanup_hook: Option<tests::DirectoryCleanupHook>,
) -> Result<(), MaterializeError> {
    check_directory_cancellation(&cancellation)?;
    std::fs::create_dir_all(&staging_root).map_err(|_| MaterializeError::Io)?;
    check_directory_cancellation(&cancellation)?;
    let temporary = tempfile::Builder::new()
        .prefix(".cid-dir-")
        .tempdir_in(&staging_root)
        .map_err(|_| MaterializeError::Io)?;
    let mut temporary = DirectoryTemporary {
        directory: Some(temporary),
        scope: &filesystem_scope,
        #[cfg(test)]
        cleanup_hook,
    };
    for entry in entries {
        check_directory_cancellation(&cancellation)?;
        if !is_confined_relative_path(&entry.name)
            || std::path::Path::new(&entry.name).components().count() != 1
        {
            tracing::warn!(name = %entry.name, "rejected unconfined CidTree entry");
            return Err(MaterializeError::Invalid);
        }
        let entry_path = temporary.path().join(&entry.name);
        match entry.entry_type {
            crate::vfs::EntryType::Dir => {
                std::fs::create_dir_all(entry_path).map_err(|_| MaterializeError::Io)?;
            }
            _ => {
                let file = std::fs::File::create(entry_path).map_err(|_| MaterializeError::Io)?;
                check_directory_cancellation(&cancellation)?;
                file.set_len(entry.size).map_err(|_| MaterializeError::Io)?;
            }
        }
        #[cfg(test)]
        tests::directory_build_hook(&mut build_hook, temporary.path(), tests::BuildStage::Entry)?;
        #[cfg(any(test, feature = "test-support"))]
        filesystem_scope.test_event(
            cache::FilesystemOperation::DirectoryBuild,
            cache::FsTestStage::DirectoryEntry,
            Some(temporary.path().to_owned()),
        );
    }
    #[cfg(test)]
    tests::directory_build_hook(
        &mut build_hook,
        temporary.path(),
        tests::BuildStage::BeforePublish,
    )?;
    #[cfg(any(test, feature = "test-support"))]
    filesystem_scope.test_event(
        cache::FilesystemOperation::DirectoryBuild,
        cache::FsTestStage::BeforePublish,
        Some(temporary.path().to_owned()),
    );
    if !cancellation.try_accept_commit() {
        return Err(MaterializeError::Cancelled);
    }
    #[cfg(any(test, feature = "test-support"))]
    filesystem_scope.test_event(
        cache::FilesystemOperation::DirectoryBuild,
        cache::FsTestStage::CommitAccepted,
        Some(temporary.path().to_owned()),
    );

    use rustix::fs::{renameat_with, RenameFlags, CWD};
    match renameat_with(
        CWD,
        temporary.path(),
        CWD,
        &staging_dir,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {
            let _former_path = temporary.directory.take().unwrap().keep();
            #[cfg(test)]
            tests::directory_build_hook(
                &mut build_hook,
                &_former_path,
                tests::BuildStage::Published,
            )?;
            #[cfg(any(test, feature = "test-support"))]
            filesystem_scope.test_event(
                cache::FilesystemOperation::DirectoryBuild,
                cache::FsTestStage::Published,
                Some(_former_path.clone()),
            );
        }
        Err(rustix::io::Errno::EXIST) if staging_dir.is_dir() => {
            // The guard removes only this builder's private staging.
        }
        Err(_) => {
            return Err(MaterializeError::Io);
        }
    }
    Ok(())
}

fn cleanup_temporary_directory(
    filesystem_scope: &cache::FsScope,
    temporary: tempfile::TempDir,
    #[cfg(test)] cleanup_hook: &mut Option<tests::DirectoryCleanupHook>,
) {
    let temporary_path = temporary.path().to_owned();
    #[cfg(test)]
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tests::directory_cleanup_hook(cleanup_hook, &temporary_path)
    })) {
        Ok(Ok(())) => {}
        result => filesystem_scope.record_cleanup_failure(format!(
            "temporary CidTree directory rollback hook failed or panicked: {result:?}"
        )),
    }
    if let Err(error) = temporary.close() {
        filesystem_scope.record_cleanup_failure(format!(
            "failed to remove temporary CidTree directory {}: {error}",
            temporary_path.display()
        ));
    }
}

async fn materialize_ipfs_descriptor_in(
    filesystem_scope: &cache::FsScope,
    logical: cache::LogicalIoPermit,
    cache: Option<&cache::CacheMode>,
    ipfs_path: &IpfsCidPath,
    write_requested: bool,
) -> Result<
    cache::FsResultEnvelope<Result<wasmtime_wasi::filesystem::Descriptor, MaterializeError>>,
    MaterializeError,
> {
    if write_requested {
        return Err(MaterializeError::NotPermitted);
    }
    if !ipfs_path.subpath.is_empty() && !is_confined_relative_path(&ipfs_path.subpath) {
        return Err(MaterializeError::Invalid);
    }

    let cache = cache.ok_or(MaterializeError::NoEntry)?;
    let subpath = (!ipfs_path.subpath.is_empty()).then_some(ipfs_path.subpath.as_str());
    let (transaction, target_path, lease) =
        materialize_cache_path(filesystem_scope, logical, cache, &ipfs_path.cid, subpath)
            .await
            .map_err(|error| {
                tracing::warn!(cid = %ipfs_path.cid, subpath = %ipfs_path.subpath, ?error, "IPFS cache materialization failed");
                error
            })?;
    transaction
        .run_blocking(cache::FilesystemOperation::DescriptorOpen, move || {
            if !target_path.exists() {
                return Err(MaterializeError::NoEntry);
            }
            open_read_only_path(&target_path, lease)
        })
        .await
        .map_err(|_| MaterializeError::Io)
}

#[cfg(test)]
pub(crate) async fn materialize_ipfs_descriptor(
    resources: &Arc<cache::CellResources>,
    filesystem_scope: &cache::FsScope,
    cache: Option<&cache::CacheMode>,
    ipfs_path: &IpfsCidPath,
    write_requested: bool,
) -> Result<wasmtime_wasi::filesystem::Descriptor, MaterializeError> {
    let logical = resources.try_logical_io().map_err(MaterializeError::Busy)?;
    materialize_ipfs_descriptor_in(filesystem_scope, logical, cache, ipfs_path, write_requested)
        .await?
        .install(|result| result)
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
        let directory = store.with(|mut access| {
            Ok::<_, P3FilesystemError>(matches!(
                access.get().table.get(&descriptor)?,
                wasmtime_wasi::filesystem::Descriptor::Dir(_)
            ))
        })?;
        if directory {
            return Ok(p3_types::DescriptorType::Directory);
        }
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
        let (cache, resources, filesystem_scope, route) = store.with(|mut access| {
            let view = access.get();
            Ok::<_, P3FilesystemError>((
                view.cache_mode.clone(),
                Arc::clone(view.filesystem_resources),
                view.filesystem_scope.clone(),
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
        if let OpenRoute::CidTree(context, target) = &route {
            CidTree::validate_relative_path(&context.path, target)
                .map_err(resolution_error)
                .map_err(p3_types::ErrorCode::from)?;
        }
        let logical = resources
            .try_logical_io()
            .map_err(|_| p3_types::ErrorCode::Busy)?;
        let (operation, context) = match route {
            OpenRoute::CidTree(mut context, target) => {
                let mut traversal = context
                    .tree
                    .traversal_in(&filesystem_scope, logical)
                    .map_err(|error| p3_types::ErrorCode::from(MaterializeError::Busy(error)))?;
                let resolved = context
                    .tree
                    .resolve_at_in(&mut traversal, &context.root, &context.path, &target)
                    .await
                    .map_err(resolution_error)
                    .map_err(p3_types::ErrorCode::from)?;
                // Traversal F ends before independent ensure/materialization,
                // while its one logical permit transfers to the next phase.
                let logical = traversal
                    .into_logical()
                    .map_err(resolution_error)
                    .map_err(p3_types::ErrorCode::from)?;
                let directory = matches!(resolved.node, ResolvedNode::CidDir { .. });
                if !directory && open_flags.contains(p3_types::OpenFlags::DIRECTORY) {
                    return Err(p3_types::ErrorCode::NotDirectory.into());
                }
                let operation = materialize_cid_tree_descriptor_in(
                    &filesystem_scope,
                    &resources,
                    logical,
                    cache.as_deref(),
                    &context.tree,
                    resolved.node,
                )
                .await
                .map_err(p3_types::ErrorCode::from)?;
                context.path = resolved.path;
                (operation, directory.then_some(context))
            }
            OpenRoute::Ipfs(ipfs_path) => {
                let operation = materialize_ipfs_descriptor_in(
                    &filesystem_scope,
                    logical,
                    cache.as_deref(),
                    &ipfs_path,
                    false,
                )
                .await
                .map_err(p3_types::ErrorCode::from)?;
                (operation, None)
            }
            OpenRoute::Wasi { .. } => unreachable!(),
        };
        store.with(|mut access| {
            let view = access.get();
            operation.install(|opened| {
                let opened = opened.map_err(p3_types::ErrorCode::from)?;
                if (open_flags.contains(p3_types::OpenFlags::DIRECTORY) || path.ends_with('/'))
                    && !matches!(opened, wasmtime_wasi::filesystem::Descriptor::Dir(_))
                {
                    return Err(p3_types::ErrorCode::NotDirectory.into());
                }
                let opened = view.table.push(opened)?;
                view.descriptors.remove(opened.rep());
                if let Some(context) = context {
                    view.descriptors.directories.insert(opened.rep(), context);
                }
                Ok(opened)
            })
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

    struct TestFilesystem {
        resources: Arc<cache::CellResources>,
        scope: cache::FsScope,
        _scope_owner: cache::FsOwner,
        _scope_finalizer: cache::FsFinalizer,
        completion: cache::FsCompletion,
        _host_owner: cache::HostFsOwner,
    }

    struct TestTree {
        tree: CidTree,
        filesystem: TestFilesystem,
    }

    impl std::ops::Deref for TestTree {
        type Target = CidTree;

        fn deref(&self) -> &Self::Target {
            &self.tree
        }
    }

    fn test_tree(root: cid::Cid, ipfs: ipfs::HttpClient, staging: std::path::PathBuf) -> TestTree {
        let filesystem = TestFilesystem::new();
        let tree = filesystem.tree(root, ipfs, staging);
        TestTree { tree, filesystem }
    }

    impl TestFilesystem {
        fn new() -> Self {
            Self::with_limits(cache::HostLimits::default(), cache::CellLimits::default())
        }

        fn with_limits(host_limits: cache::HostLimits, cell_limits: cache::CellLimits) -> Self {
            let (host_owner, host) =
                cache::HostFsOwner::start(tokio::runtime::Handle::current(), host_limits).unwrap();
            let resources = cache::CellResources::new(Arc::clone(&host), cell_limits);
            let (scope, scope_owner, scope_finalizer, completion) =
                host.try_filesystem_scope().unwrap();
            Self {
                resources,
                scope,
                _scope_owner: scope_owner,
                _scope_finalizer: scope_finalizer,
                completion,
                _host_owner: host_owner,
            }
        }

        fn tree(
            &self,
            root: cid::Cid,
            ipfs: ipfs::HttpClient,
            staging: std::path::PathBuf,
        ) -> CidTree {
            CidTree::new(
                root,
                ipfs,
                staging,
                Arc::clone(self.resources.host()),
                self.scope.clone(),
            )
        }

        async fn isolated_cache(&self, pinner: Arc<dyn cache::Pinner>) -> cache::CacheMode {
            let logical = self.resources.try_logical_io().unwrap();
            let transaction = self
                .resources
                .host()
                .try_cleanup_transaction(logical, &[&self.scope])
                .unwrap();
            cache::CacheMode::Isolated(
                cache::IsolatedPinset::new(pinner, self.scope.clone(), &transaction)
                    .await
                    .unwrap(),
            )
        }

        async fn shared_cache(
            &self,
            pinner: Arc<dyn cache::Pinner>,
            budget: usize,
        ) -> (Arc<cache::PinsetCache>, cache::CacheMode) {
            let shared = Arc::new(
                cache::PinsetCache::new(pinner, budget, Arc::clone(self.resources.host()))
                    .await
                    .unwrap(),
            );
            (Arc::clone(&shared), cache::CacheMode::Shared(shared))
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum BuildStage {
        Entry,
        BeforePublish,
        Published,
    }

    pub(super) type DirectoryBuildHook =
        Box<dyn FnMut(&std::path::Path, BuildStage) -> Result<(), MaterializeError> + Send>;
    thread_local! {
        static DIRECTORY_BUILD_HOOK: std::cell::RefCell<Option<DirectoryBuildHook>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) type DirectoryCleanupHook =
        Box<dyn FnMut(&std::path::Path) -> Result<(), MaterializeError> + Send>;
    thread_local! {
        static DIRECTORY_CLEANUP_HOOK: std::cell::RefCell<Option<DirectoryCleanupHook>> =
            const { std::cell::RefCell::new(None) };
    }

    // Materialization performs no awaits between tempdir creation and publish.
    // Tests install hooks only on dedicated threads or current-thread runtimes.
    pub(super) fn take_directory_build_hook() -> Option<DirectoryBuildHook> {
        DIRECTORY_BUILD_HOOK.with_borrow_mut(Option::take)
    }

    pub(super) fn take_directory_cleanup_hook() -> Option<DirectoryCleanupHook> {
        DIRECTORY_CLEANUP_HOOK.with_borrow_mut(Option::take)
    }

    pub(super) fn directory_build_hook(
        hook: &mut Option<DirectoryBuildHook>,
        path: &std::path::Path,
        stage: BuildStage,
    ) -> Result<(), MaterializeError> {
        match hook {
            Some(hook) => hook(path, stage),
            None => Ok(()),
        }
    }

    pub(super) fn directory_cleanup_hook(
        hook: &mut Option<DirectoryCleanupHook>,
        path: &std::path::Path,
    ) -> Result<(), MaterializeError> {
        match hook {
            Some(hook) => hook(path),
            None => Ok(()),
        }
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
    async fn native_lease_clones_retain_one_admission_until_actual_work_releases() {
        let (host_owner, host) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::new(1, 2, 1).unwrap(),
        )
        .unwrap();
        let resources =
            cache::CellResources::new(Arc::clone(&host), cache::CellLimits::new(1).unwrap());
        let (scope, scope_owner, scope_finalizer, completion) =
            host.try_filesystem_scope().unwrap();

        let operation = admit_native_filesystem_operation(&resources, &scope).unwrap();
        let native_worker = operation.clone();
        drop(operation);
        assert!(matches!(
            admit_native_filesystem_operation(&resources, &scope),
            Err(FilesystemOperationError::Rejected)
        ));

        drop(native_worker);
        let replacement = admit_native_filesystem_operation(&resources, &scope).unwrap();
        drop(replacement);
        drop(scope_owner);
        drop(scope_finalizer);
        completion.wait().await.unwrap();
        host_owner.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cid_tree_rejects_unconfined_directory_entry_names() {
        let fs = TestFilesystem::new();
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
        let tree = fs.tree(
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
        let fs = TestFilesystem::new();
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
        let tree = fs.tree(
            ipfs::cid_identity::parse_cid(cid).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            staging.path().to_path_buf(),
        );
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed_in_hook = observed.clone();
        let final_path = staging.path().join(format!("dir-{cid}"));
        let hook = BuildHookGuard::install(move |temporary, stage| {
            assert!(stage == BuildStage::Entry);
            assert_eq!(std::fs::metadata(temporary.join("first")).unwrap().len(), 7);
            assert!(!final_path.exists());
            observed_in_hook.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let result = materialize_cid_tree_descriptor(None, &tree, "", false).await;
        drop(hook);
        assert!(
            observed.load(std::sync::atomic::Ordering::SeqCst),
            "the earlier valid entry must be populated"
        );
        assert!(matches!(result, Err(MaterializeError::Invalid)));
        assert!(!staging.path().join(format!("dir-{cid}")).exists());
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cid_tree_materialization_propagates_malformed_directory_listing() {
        let fs = TestFilesystem::new();
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
        let tree = fs.tree(
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
        let fs = TestFilesystem::new();
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
        let tree = fs.tree(
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
        let fs = TestFilesystem::new();
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
        let tree = fs.tree(
            cid,
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            tree_staging.path().to_path_buf(),
        );
        let cache = fs.isolated_cache(pinner).await;
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
        let fs = TestFilesystem::new();
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
        let tree = fs.tree(
            cid,
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
            staging.path().into(),
        );
        let pinner = Arc::new(MockPinner {
            data: HashMap::from([(cid, content.to_vec())]),
            path_data: HashMap::new(),
        });
        let cache = fs.isolated_cache(pinner).await;
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
            hook: impl FnMut(&std::path::Path, BuildStage) -> Result<(), MaterializeError>
                + Send
                + 'static,
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

    struct CleanupHookGuard;

    impl CleanupHookGuard {
        fn install(
            hook: impl FnMut(&std::path::Path) -> Result<(), MaterializeError> + Send + 'static,
        ) -> Self {
            DIRECTORY_CLEANUP_HOOK.with_borrow_mut(|slot| {
                assert!(slot.is_none());
                *slot = Some(Box::new(hook));
            });
            Self
        }
    }

    impl Drop for CleanupHookGuard {
        fn drop(&mut self) {
            DIRECTORY_CLEANUP_HOOK.with_borrow_mut(|slot| *slot = None);
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
    ) -> TestTree {
        std::fs::write(
            staging.join(format!("{DIRECTORY_CID}.dirlist.json")),
            serde_json::to_vec(entries).unwrap(),
        )
        .unwrap();
        test_tree(
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
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed_in_hook = observed.clone();
        let final_in_hook = final_path.clone();
        let hook = BuildHookGuard::install(move |temporary, stage| {
            assert!(stage == BuildStage::Entry);
            // A real stub has been populated. Obstruct the next mkdir to cause
            // a real filesystem error, without corrupting the cached listing.
            assert_eq!(std::fs::metadata(temporary.join("first")).unwrap().len(), 7);
            assert!(!final_in_hook.exists());
            std::fs::write(temporary.join("nested"), b"obstruction").unwrap();
            observed_in_hook.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let result = materialize_cid_tree_descriptor(None, &tree, "", false).await;
        drop(hook);
        assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
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

    // Regressions for the distinct admission/driver/blocking execution boundaries.
    #[test]
    fn directory_cancellation_before_execution_has_no_filesystem_effects() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::Poll;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            for boundary in [
                "never-polled",
                "before-submit",
                "driver-queued",
                "blocking-queued",
            ] {
                let fs = TestFilesystem::new();
                let parent = tempfile::tempdir().unwrap();
                let root = parent.path().join("untouched");
                let destination = root.join("published");
                let transaction = fs
                    .resources
                    .host()
                    .try_cleanup_transaction(fs.resources.try_logical_io().unwrap(), &[&fs.scope])
                    .unwrap();
                let entered = Arc::new(AtomicBool::new(false));
                let witness = Arc::clone(&entered);
                let scope = fs.scope.clone();
                let cancellation = scope.cancellation();
                let work_root = root.clone();
                let (submitted_tx, submitted_rx) = tokio::sync::oneshot::channel();
                let submitted_tx = std::sync::Mutex::new(Some(submitted_tx));
                fs.scope.set_test_hook(Arc::new(move |event| {
                    if event.stage == cache::FsTestStage::BlockingSubmitted {
                        let _ = submitted_tx.lock().unwrap().take().unwrap().send(());
                    }
                }));
                let (blocker_release_tx, blocker_release_rx) = std::sync::mpsc::channel();
                let blocker = if boundary == "blocking-queued" {
                    let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
                    let blocker = tokio::task::spawn_blocking(move || {
                        blocked_tx.send(()).unwrap();
                        blocker_release_rx
                            .recv_timeout(std::time::Duration::from_secs(10))
                            .unwrap();
                    });
                    blocked_rx.await.unwrap();
                    Some(blocker)
                } else {
                    None
                };
                let build_cancellation = cancellation.clone();
                let mut call = Box::pin(submit_directory_build(
                    &transaction,
                    &fs.scope,
                    &cancellation,
                    move || {
                        witness.store(true, Ordering::SeqCst);
                        build_directory(
                            work_root,
                            destination,
                            directory_entries(),
                            scope,
                            build_cancellation,
                            None,
                            None,
                        )
                    },
                ));
                if matches!(boundary, "driver-queued" | "blocking-queued") {
                    std::future::poll_fn(|cx| {
                        assert!(call.as_mut().poll(cx).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                    if boundary == "blocking-queued" {
                        // Driver has submitted the job, but the sole native worker is gated.
                        submitted_rx.await.unwrap();
                    }
                    assert!(!entered.load(Ordering::SeqCst));
                }
                drop(fs._scope_owner);
                if let Some(blocker) = blocker {
                    blocker_release_tx.send(()).unwrap();
                    blocker.await.unwrap();
                }
                if boundary == "never-polled" {
                    drop(call);
                } else {
                    assert_eq!(call.await, Err(MaterializeError::Cancelled), "{boundary}");
                }
                assert_eq!(
                    entered.load(Ordering::SeqCst),
                    matches!(boundary, "driver-queued" | "blocking-queued"),
                    "{boundary}"
                );
                assert!(!root.exists(), "{boundary} must cause no native effects");
                drop(transaction);
                fs.completion.wait().await.unwrap();
            }
        });
    }

    #[tokio::test]
    async fn directory_rollback_error_and_panics_fail_completion_after_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};
        for failure in ["rollback-error", "rollback-panic", "populate-panic"] {
            let fs = TestFilesystem::new();
            let staging = tempfile::tempdir().unwrap();
            let root = staging.path().to_owned();
            let final_path = root.join("published");
            let later_cleanup = Arc::new(AtomicBool::new(false));
            let later_witness = Arc::clone(&later_cleanup);
            fs.scope
                .own_cleanup(cache::FilesystemOperation::ProcessCleanup, move || {
                    later_witness.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            let transaction = fs
                .resources
                .host()
                .try_cleanup_transaction(fs.resources.try_logical_io().unwrap(), &[&fs.scope])
                .unwrap();
            let scope = fs.scope.clone();
            let mut owner = Some(fs._scope_owner);
            let parent = root.clone();
            let result = transaction
                .run_directory_build(&fs.scope, move || {
                    build_directory(
                        root,
                        final_path,
                        directory_entries(),
                        scope.clone(),
                        scope.cancellation(),
                        Some(Box::new(move |_, stage| {
                            if stage == BuildStage::Entry {
                                drop(owner.take());
                                assert_ne!(failure, "populate-panic", "injected population panic");
                            }
                            Ok(())
                        })),
                        Some(Box::new(move |_| {
                            assert_ne!(failure, "rollback-panic", "injected rollback panic");
                            if failure == "rollback-error" {
                                // A real unlink/rmdir failure, not just a synthetic error result.
                                std::fs::set_permissions(
                                    &parent,
                                    std::fs::Permissions::from_mode(0o555),
                                )
                                .unwrap();
                            }
                            Ok(())
                        })),
                    )
                })
                .await;
            if failure == "populate-panic" {
                assert!(result.is_err());
            } else {
                assert_eq!(
                    result.unwrap().into_inner(),
                    Err(MaterializeError::Cancelled)
                );
            }
            drop(transaction);
            assert!(fs.completion.wait().await.is_err(), "{failure}");
            assert!(later_cleanup.load(Ordering::SeqCst));
            assert!(!staging.path().join("published").exists());
            if failure == "rollback-error" {
                std::fs::set_permissions(staging.path(), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
                assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 1);
            } else {
                assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
            }
        }
    }

    #[tokio::test]
    async fn accepted_directory_commit_survives_close_and_skips_descriptor_open() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for winner_exists in [false, true] {
            let staging = tempfile::tempdir().unwrap();
            let tree = cached_directory_tree(staging.path(), &directory_entries());
            let final_path = staging.path().join(format!("dir-{DIRECTORY_CID}"));
            let destination = final_path.clone();
            let TestTree {
                tree,
                filesystem: fs,
            } = tree;
            let owner = std::sync::Mutex::new(Some(fs._scope_owner));
            let descriptor_opened = Arc::new(AtomicBool::new(false));
            let descriptor_witness = Arc::clone(&descriptor_opened);
            fs.scope.set_test_hook(Arc::new(move |event| {
                if event.operation == cache::FilesystemOperation::DescriptorOpen {
                    descriptor_witness.store(true, Ordering::SeqCst);
                }
                if event.stage == cache::FsTestStage::CommitAccepted {
                    if winner_exists {
                        std::fs::create_dir(&destination).unwrap();
                        std::fs::write(destination.join("winner"), b"untouched").unwrap();
                    }
                    drop(owner.lock().unwrap().take());
                }
            }));
            let result = materialize_cid_tree_descriptor(None, &tree, "", false).await;
            assert!(
                matches!(result, Err(MaterializeError::Cancelled)),
                "closed root must skip the descriptor tail after accepted publication"
            );
            fs.completion.wait().await.unwrap();
            assert!(!descriptor_opened.load(Ordering::SeqCst));
            if winner_exists {
                assert_eq!(
                    std::fs::read(final_path.join("winner")).unwrap(),
                    b"untouched"
                );
                assert_eq!(std::fs::read_dir(&final_path).unwrap().count(), 1);
            } else {
                assert_complete_directory(&final_path, false);
            }
            assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 2);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closed_root_stops_directory_entries_and_retains_rollback() {
        let fs = TestFilesystem::with_limits(
            cache::HostLimits::new(1, 2, 1).unwrap(),
            cache::CellLimits::new(1).unwrap(),
        );
        let staging = tempfile::tempdir().unwrap();
        let final_path = staging.path().join("published");
        let transaction = fs
            .resources
            .host()
            .try_cleanup_transaction(fs.resources.try_logical_io().unwrap(), &[&fs.scope])
            .unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (rollback_tx, rollback_rx) = tokio::sync::oneshot::channel();
        let (rollback_release_tx, rollback_release_rx) = std::sync::mpsc::channel();
        let mut entered_tx = Some(entered_tx);
        let mut rollback_tx = Some(rollback_tx);
        let scope = fs.scope.clone();
        let root = staging.path().to_owned();
        let destination = final_path.clone();
        let waiter = tokio::spawn(async move {
            transaction
                .run_directory_build(&scope, {
                    let scope = scope.clone();
                    move || {
                        build_directory(
                            root,
                            destination,
                            directory_entries(),
                            scope.clone(),
                            scope.cancellation(),
                            Some(Box::new(move |path, stage| {
                                if stage == BuildStage::Entry {
                                    if let Some(entered) = entered_tx.take() {
                                        entered.send(path.to_owned()).unwrap();
                                        release_rx
                                            .recv_timeout(std::time::Duration::from_secs(10))
                                            .unwrap();
                                    }
                                }
                                Ok(())
                            })),
                            Some(Box::new(move |path| {
                                rollback_tx.take().unwrap().send(path.to_owned()).unwrap();
                                rollback_release_rx
                                    .recv_timeout(std::time::Duration::from_secs(10))
                                    .unwrap();
                                Ok(())
                            })),
                        )
                    }
                })
                .await
                .unwrap()
                .into_inner()
        });
        let temporary = entered_rx.await.unwrap();
        assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 1);
        drop(fs._scope_owner);
        assert!(fs.resources.try_logical_io().is_err());
        assert!(fs.completion.try_snapshot().unwrap().outstanding_work() > 0);
        release_tx.send(()).unwrap();
        let rollback = rollback_rx
            .await
            .expect("root close must roll back, not publish");
        assert_eq!(rollback, temporary);
        assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 1);
        assert!(!final_path.exists());
        assert!(fs.resources.try_logical_io().is_err());
        assert!(fs.completion.try_snapshot().unwrap().outstanding_work() > 0);
        rollback_release_tx.send(()).unwrap();
        assert!(waiter.await.unwrap().is_err());
        fs.completion.wait().await.unwrap();
        assert!(!temporary.exists());
        assert!(!final_path.exists());
        assert!(fs.resources.try_logical_io().is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_waiter_with_live_root_retains_complete_publication_and_retry() {
        let staging = tempfile::TempDir::new().unwrap();
        let tree = cached_directory_tree(staging.path(), &directory_entries());
        let final_path = staging.path().join(format!("dir-{DIRECTORY_CID}"));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered_tx = std::sync::Mutex::new(Some(entered_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (published_tx, published_rx) = tokio::sync::oneshot::channel();
        let published_tx = std::sync::Mutex::new(Some(published_tx));
        let release_rx = std::sync::Mutex::new(Some(release_rx));
        let first_entry = std::sync::atomic::AtomicBool::new(true);
        let _hook = BuildHookGuard::install(move |_, stage| {
            if stage == BuildStage::Entry
                && first_entry.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                entered_tx.lock().unwrap().take().unwrap().send(()).unwrap();
                let receiver = release_rx.lock().unwrap().take().unwrap();
                let _ = receiver.recv();
            }
            if stage == BuildStage::Published {
                published_tx
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
            }
            Ok(())
        });
        let tree = Arc::new(tree);
        let waiter_tree = Arc::clone(&tree);
        let waiter = tokio::spawn(async move {
            materialize_cid_tree_descriptor(None, &waiter_tree, "", false).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx)
            .await
            .unwrap()
            .unwrap();
        waiter.abort();
        match waiter.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("materialization waiter completed before cancellation"),
        }
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), published_rx)
            .await
            .unwrap()
            .unwrap();

        assert_complete_directory(&final_path, false);
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 2);
        let retry_tree = cached_directory_tree(staging.path(), &directory_entries());
        materialize_cid_tree_descriptor(None, &retry_tree, "", false)
            .await
            .unwrap();
        assert_complete_directory(&final_path, false);
    }

    #[tokio::test]
    async fn cid_tree_loser_cleanup_failure_poisons_completion_and_preserves_winner() {
        let staging = tempfile::TempDir::new().unwrap();
        let tree = cached_directory_tree(staging.path(), &directory_entries());
        let final_path = staging.path().join(format!("dir-{DIRECTORY_CID}"));
        let temporary_path = Arc::new(std::sync::Mutex::new(None));
        let temporary_in_hook = Arc::clone(&temporary_path);
        let winner_in_hook = final_path.clone();
        let _build_hook = BuildHookGuard::install(move |temporary, stage| {
            if stage == BuildStage::BeforePublish {
                *temporary_in_hook.lock().unwrap() = Some(temporary.to_owned());
                std::fs::create_dir(&winner_in_hook).unwrap();
                std::fs::write(winner_in_hook.join("winner"), b"winner").unwrap();
            }
            Ok(())
        });
        let _cleanup_hook = CleanupHookGuard::install(|_| Err(MaterializeError::Io));
        let completion = tree.filesystem.completion.clone();
        let descriptor = materialize_cid_tree_descriptor(None, &tree, "", false)
            .await
            .unwrap();
        let wasmtime_wasi::filesystem::Descriptor::Dir(_) = descriptor else {
            panic!("expected the competing winner directory");
        };
        assert_eq!(std::fs::read(final_path.join("winner")).unwrap(), b"winner");

        drop(tree);
        let error = completion
            .wait()
            .await
            .expect_err("loser cleanup failure must poison filesystem completion");
        assert!(error.to_string().contains("cleanup failed"));
        assert_eq!(std::fs::read(final_path.join("winner")).unwrap(), b"winner");
        assert!(!temporary_path.lock().unwrap().as_ref().unwrap().exists());
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
        let second_tree = test_tree(
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

    #[tokio::test(flavor = "multi_thread")]
    async fn cid_tree_concurrent_builders_publish_one_complete_directory() {
        concurrent_directory_publication(false);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cid_tree_concurrent_builders_preserve_empty_winner() {
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
        let tree = test_tree(
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
        let reused_path = Arc::new(std::sync::Mutex::new(None));
        let reused_in_hook = reused_path.clone();
        let _hook = BuildHookGuard::install(move |temporary, stage| {
            if stage == BuildStage::Published {
                // Publication frees this pathname. Simulate another builder
                // acquiring it before the successful publisher drops TempDir.
                std::fs::create_dir(temporary).unwrap();
                std::fs::write(temporary.join("other-builder"), b"in progress").unwrap();
                *reused_in_hook.lock().unwrap() = Some(temporary.to_path_buf());
            }
            Ok(())
        });
        materialize_cid_tree_descriptor(None, &tree, "", false)
            .await
            .unwrap();
        let reused = reused_path
            .lock()
            .unwrap()
            .clone()
            .expect("publication hook ran");
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
        let tree = test_tree(
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
        let tree = test_tree(
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
        let tree = test_tree(
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

    struct GatedColdPinner {
        contents: Vec<u8>,
        gate_pin: bool,
        gate_fetch: bool,
        fail_fetch: bool,
        pins: std::sync::atomic::AtomicUsize,
        fetches: std::sync::atomic::AtomicUsize,
        finished: std::sync::atomic::AtomicBool,
        pin_started: tokio::sync::Notify,
        release_pin: tokio::sync::Notify,
        fetch_started: tokio::sync::Notify,
        release_fetch: tokio::sync::Notify,
    }

    impl GatedColdPinner {
        fn new(gate_pin: bool, gate_fetch: bool, fail_fetch: bool) -> Arc<Self> {
            Arc::new(Self {
                contents: b"retained cold materialization".to_vec(),
                gate_pin,
                gate_fetch,
                fail_fetch,
                pins: std::sync::atomic::AtomicUsize::new(0),
                fetches: std::sync::atomic::AtomicUsize::new(0),
                finished: std::sync::atomic::AtomicBool::new(false),
                pin_started: tokio::sync::Notify::new(),
                release_pin: tokio::sync::Notify::new(),
                fetch_started: tokio::sync::Notify::new(),
                release_fetch: tokio::sync::Notify::new(),
            })
        }
    }

    #[async_trait::async_trait]
    impl cache::Pinner for GatedColdPinner {
        async fn pin(&self, _cid: &cid::Cid) -> anyhow::Result<()> {
            self.pins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.gate_pin {
                let released = self.release_pin.notified();
                self.pin_started.notify_waiters();
                released.await;
            }
            Ok(())
        }

        async fn unpin(&self, _cid: &cid::Cid) -> anyhow::Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &cid::Cid) -> anyhow::Result<Vec<u8>> {
            Ok(self.contents.clone())
        }

        async fn fetch_to_path(
            &self,
            _cid: &cid::Cid,
            destination: &std::path::Path,
        ) -> anyhow::Result<()> {
            use std::io::Write;

            let fetch_number = self
                .fetches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            file.write_all(&self.contents[..self.contents.len() / 2])?;
            if self.gate_fetch && fetch_number == 1 {
                let released = self.release_fetch.notified();
                self.fetch_started.notify_waiters();
                released.await;
            }
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if self.fail_fetch {
                anyhow::bail!("injected retained fetch failure");
            }
            file.write_all(&self.contents[self.contents.len() / 2..])?;
            Ok(())
        }

        async fn size(&self, _cid: &cid::Cid) -> anyhow::Result<u64> {
            Ok(self.contents.len() as u64)
        }
    }

    struct MockPinner {
        data: HashMap<cid::Cid, Vec<u8>>,
        path_data: HashMap<String, Vec<u8>>,
    }

    struct DirectoryRootPinner;

    impl DirectoryRootPinner {
        fn write_exclusive(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
            use std::io::Write as _;

            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            file.write_all(bytes)?;
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl cache::Pinner for DirectoryRootPinner {
        async fn pin(&self, _cid: &cid::Cid) -> anyhow::Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &cid::Cid) -> anyhow::Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &cid::Cid) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("directory root requires path materialization")
        }

        async fn fetch_to_path(
            &self,
            _cid: &cid::Cid,
            destination: &std::path::Path,
        ) -> anyhow::Result<()> {
            std::fs::create_dir(destination)?;
            Self::write_exclusive(&destination.join("marker"), b"directory-root")
        }

        async fn fetch_path_to_path(
            &self,
            _cid: &cid::Cid,
            subpath: &str,
            destination: &std::path::Path,
        ) -> anyhow::Result<()> {
            let bytes = match subpath {
                "root" => b"literal-root-child".as_slice(),
                "root/nested" => b"nested-under-root-child".as_slice(),
                _ => anyhow::bail!("unexpected test subpath: {subpath}"),
            };
            Self::write_exclusive(destination, bytes)
        }

        async fn size(&self, _cid: &cid::Cid) -> anyhow::Result<u64> {
            Ok(1)
        }
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

    fn spawn_materialization(
        resources: Arc<cache::CellResources>,
        scope: cache::FsScope,
        cache: Arc<cache::CacheMode>,
        path: IpfsCidPath,
    ) -> tokio::task::JoinHandle<Result<wasmtime_wasi::filesystem::Descriptor, MaterializeError>>
    {
        tokio::spawn(async move {
            materialize_ipfs_descriptor(&resources, &scope, Some(&cache), &path, false).await
        })
    }

    #[tokio::test]
    async fn same_cid_cold_materializations_each_require_foreground_admission() {
        let fs = TestFilesystem::with_limits(
            cache::HostLimits::new(4, 2, 4).unwrap(),
            cache::CellLimits::new(4).unwrap(),
        );
        let cid: cid::Cid = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG"
            .parse()
            .unwrap();
        let pinner = GatedColdPinner::new(true, true, false);
        let cache = Arc::new(fs.isolated_cache(pinner.clone()).await);
        let path = move || IpfsCidPath {
            cid,
            subpath: String::new(),
        };
        let pin_started = pinner.pin_started.notified();
        let mut first = spawn_materialization(
            Arc::clone(&fs.resources),
            fs.scope.clone(),
            Arc::clone(&cache),
            path(),
        );
        pin_started.await;

        let (second_entered_tx, second_entered_rx) = tokio::sync::oneshot::channel();
        let second_resources = Arc::clone(&fs.resources);
        let second_scope = fs.scope.clone();
        let second_cache = Arc::clone(&cache);
        let mut second = tokio::spawn(async move {
            second_entered_tx.send(()).unwrap();
            materialize_ipfs_descriptor(
                &second_resources,
                &second_scope,
                Some(&second_cache),
                &path(),
                false,
            )
            .await
        });
        second_entered_rx.await.unwrap();
        assert!(
            !second.is_finished(),
            "same-CID waiter must join the retained ensure owner"
        );

        let fetch_started = pinner.fetch_started.notified();
        pinner.release_pin.notify_one();
        fetch_started.await;
        let (loser_was_first, loser) = tokio::select! {
            result = &mut first => (true, result.unwrap()),
            result = &mut second => (false, result.unwrap()),
        };
        let error = match loser {
            Err(error) => error,
            Ok(_) => panic!("the second independent cold transfer must be rejected"),
        };
        assert!(matches!(
            error,
            MaterializeError::Busy(ref admission)
                if admission.kind() == cache::AdmissionKind::ForegroundProducer
        ));
        assert_eq!(
            pinner.pins.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "same-CID ensure must stay deduplicated"
        );
        assert_eq!(
            pinner.fetches.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "whole materialization must not be coalesced or duplicated after F rejection"
        );

        pinner.release_fetch.notify_one();
        let descriptor = if loser_was_first {
            second.await.unwrap().unwrap()
        } else {
            first.await.unwrap().unwrap()
        };
        drop(descriptor);
        assert_eq!(
            std::fs::read(cache.staging_dir().join(cid.to_string())).unwrap(),
            pinner.contents
        );
    }

    #[tokio::test]
    async fn cold_materialization_reuses_one_foreground_lease_through_descriptor_open() {
        let fs = TestFilesystem::with_limits(
            cache::HostLimits::new(2, 2, 4).unwrap(),
            cache::CellLimits::new(2).unwrap(),
        );
        let cid: cid::Cid = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG"
            .parse()
            .unwrap();
        let pinner = GatedColdPinner::new(false, false, false);
        let cache = fs.isolated_cache(pinner.clone()).await;
        let descriptor = materialize_ipfs_descriptor(
            &fs.resources,
            &fs.scope,
            Some(&cache),
            &IpfsCidPath {
                cid,
                subpath: String::new(),
            },
            false,
        )
        .await
        .expect("one F must span staging, fetch, commit, and descriptor open");
        drop(descriptor);
        assert_eq!(pinner.fetches.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retained_materialization_outlives_cell_runtime_until_rollback_and_completion() {
        use futures::FutureExt as _;

        let fs = TestFilesystem::with_limits(
            cache::HostLimits::new(1, 2, 4).unwrap(),
            cache::CellLimits::new(1).unwrap(),
        );
        let cid: cid::Cid = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG"
            .parse()
            .unwrap();
        let pinner = GatedColdPinner::new(false, true, true);
        let cache = Arc::new(fs.isolated_cache(pinner.clone()).await);
        let staging = cache.staging_dir().to_path_buf();
        let task_resources = Arc::clone(&fs.resources);
        let task_scope = fs.scope.clone();
        let task_cache = Arc::clone(&cache);
        let fetch_started = pinner.fetch_started.notified();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let cell_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.spawn(async move {
                let _ = materialize_ipfs_descriptor(
                    &task_resources,
                    &task_scope,
                    Some(&task_cache),
                    &IpfsCidPath {
                        cid,
                        subpath: String::new(),
                    },
                    false,
                )
                .await;
            });
            runtime.block_on(async { shutdown_rx.await.unwrap() });
            drop(runtime);
        });

        fetch_started.await;
        shutdown_tx.send(()).unwrap();
        tokio::task::spawn_blocking(move || cell_thread.join().unwrap())
            .await
            .unwrap();
        assert!(matches!(
            fs.resources.try_logical_io(),
            Err(ref error) if error.kind() == cache::AdmissionKind::CellLogical
        ));

        let TestFilesystem {
            resources: _,
            scope: _,
            _scope_owner: owner,
            _scope_finalizer: finalizer,
            completion,
            _host_owner: host_owner,
        } = fs;
        drop(owner);
        let finalizer_task = tokio::spawn(finalizer.run());
        assert!(
            completion.wait().now_or_never().is_none(),
            "origin completion must remain pending while retained materialization is active"
        );

        pinner.release_fetch.notify_one();
        finalizer_task.await.unwrap().unwrap();
        completion.wait().await.unwrap();
        assert!(pinner.finished.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            !staging.exists(),
            "failed retained materialization and root cleanup must remove partial staging"
        );
        drop(cache);
        drop(host_owner);
    }

    #[tokio::test]
    async fn materialize_ipfs_fetches_read_only_content() {
        let fs = TestFilesystem::new();
        let content = b"hello ipfs world";
        let (cid, pinner) = test_cid_and_pinner(content);
        let cache = fs.isolated_cache(pinner).await;
        let path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };

        let descriptor =
            materialize_ipfs_descriptor(&fs.resources, &fs.scope, Some(&cache), &path, false)
                .await
                .expect("materialize IPFS content");
        drop(descriptor);
        assert_eq!(
            std::fs::read(cache.staging_dir().join(cid.to_string())).unwrap(),
            content
        );
    }

    #[tokio::test]
    async fn preserving_native_handle_retains_generation_after_descriptor_drop_and_eviction() {
        use futures::FutureExt as _;
        use std::io::Read as _;

        let fs = TestFilesystem::with_limits(
            cache::HostLimits::new(8, 3, 8).unwrap(),
            cache::CellLimits::new(1).unwrap(),
        );
        let first_hash = cid::multihash::Multihash::wrap(0x00, &[71]).unwrap();
        let second_hash = cid::multihash::Multihash::wrap(0x00, &[72]).unwrap();
        let first = cid::Cid::new_v1(0x55, first_hash);
        let second = cid::Cid::new_v1(0x55, second_hash);
        let mut data = HashMap::new();
        data.insert(first, b"first".to_vec());
        data.insert(second, b"other".to_vec());
        let mut path_data = HashMap::new();
        path_data.insert(format!("{first}/a"), b"first-a".to_vec());
        path_data.insert(format!("{first}/b"), b"first-b".to_vec());
        let pinner = Arc::new(MockPinner { data, path_data });
        let (shared, cache) = fs.shared_cache(pinner, 5).await;
        let descriptor_a = materialize_ipfs_descriptor(
            &fs.resources,
            &fs.scope,
            Some(&cache),
            &IpfsCidPath {
                cid: first,
                subpath: "a".to_owned(),
            },
            false,
        )
        .await
        .unwrap();
        let descriptor_b = materialize_ipfs_descriptor(
            &fs.resources,
            &fs.scope,
            Some(&cache),
            &IpfsCidPath {
                cid: first,
                subpath: "b".to_owned(),
            },
            false,
        )
        .await
        .unwrap();
        let operation = admit_native_filesystem_operation(&fs.resources, &fs.scope).unwrap();
        assert!(matches!(
            admit_native_filesystem_operation(&fs.resources, &fs.scope),
            Err(FilesystemOperationError::Rejected)
        ));
        drop(operation);
        let operation = admit_native_filesystem_operation(&fs.resources, &fs.scope).unwrap();
        drop(operation);
        let (generation_path, completion) = shared.test_generation(&first).unwrap();
        let cache_completion = shared.completion();
        let wasmtime_wasi::filesystem::Descriptor::File(file) = &descriptor_a else {
            panic!("expected staged file descriptor")
        };
        let native = file.file.try_clone().unwrap();
        drop(descriptor_a);
        drop(descriptor_b);

        shared.ensure(&second).await.unwrap();
        assert!(shared.test_generation(&first).is_some());
        assert!(generation_path.exists());
        assert_eq!(
            std::fs::read(generation_path.join("content/subpaths/b")).unwrap(),
            b"first-b"
        );
        drop(cache);
        drop(shared);
        assert!(cache_completion.wait().now_or_never().is_none());
        let mut contents = String::new();
        let mut native_file = &*native;
        native_file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "first-a");

        drop(native);
        completion.wait().await.unwrap();
        cache_completion.wait().await.unwrap();
        assert!(!generation_path.exists());
    }

    #[tokio::test]
    async fn shared_generation_root_and_literal_root_subpaths_are_disjoint_in_both_orders() {
        use std::io::Read as _;

        async fn materialize(
            fs: &TestFilesystem,
            cache: &cache::CacheMode,
            cid: cid::Cid,
            subpath: &str,
        ) -> wasmtime_wasi::filesystem::Descriptor {
            materialize_ipfs_descriptor(
                &fs.resources,
                &fs.scope,
                Some(cache),
                &IpfsCidPath {
                    cid,
                    subpath: subpath.to_owned(),
                },
                false,
            )
            .await
            .unwrap()
        }

        fn read_file(descriptor: &wasmtime_wasi::filesystem::Descriptor) -> Vec<u8> {
            let wasmtime_wasi::filesystem::Descriptor::File(file) = descriptor else {
                panic!("expected a staged file descriptor")
            };
            let native = file.file.try_clone().unwrap();
            let mut reader = &*native;
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            bytes
        }

        for root_first in [true, false] {
            let fs = TestFilesystem::new();
            let cid: cid::Cid = DIRECTORY_CID.parse().unwrap();
            let (shared, cache) = fs.shared_cache(Arc::new(DirectoryRootPinner), 8).await;

            let (root, child) = if root_first {
                (
                    materialize(&fs, &cache, cid, "").await,
                    materialize(&fs, &cache, cid, "root").await,
                )
            } else {
                let child = materialize(&fs, &cache, cid, "root").await;
                let root = materialize(&fs, &cache, cid, "").await;
                (root, child)
            };

            assert!(matches!(
                root,
                wasmtime_wasi::filesystem::Descriptor::Dir(_)
            ));
            assert_eq!(read_file(&child), b"literal-root-child");

            let (generation, _) = shared.test_generation(&cid).unwrap();
            let root_path = generation.join("content/root");
            let child_path = generation.join("content/subpaths/root");
            assert_ne!(root_path, child_path);
            assert!(root_path.is_dir());
            assert_eq!(
                std::fs::read(root_path.join("marker")).unwrap(),
                b"directory-root"
            );
            assert_eq!(std::fs::read(child_path).unwrap(), b"literal-root-child");
        }

        for root_first in [true, false] {
            let fs = TestFilesystem::new();
            let cid: cid::Cid = DIRECTORY_CID.parse().unwrap();
            let (shared, cache) = fs.shared_cache(Arc::new(DirectoryRootPinner), 8).await;

            let (root, nested) = if root_first {
                (
                    materialize(&fs, &cache, cid, "").await,
                    materialize(&fs, &cache, cid, "root/nested").await,
                )
            } else {
                let nested = materialize(&fs, &cache, cid, "root/nested").await;
                let root = materialize(&fs, &cache, cid, "").await;
                (root, nested)
            };

            assert!(matches!(
                root,
                wasmtime_wasi::filesystem::Descriptor::Dir(_)
            ));
            assert_eq!(read_file(&nested), b"nested-under-root-child");

            let (generation, _) = shared.test_generation(&cid).unwrap();
            let root_path = generation.join("content/root");
            let nested_path = generation.join("content/subpaths/root/nested");
            assert!(!nested_path.starts_with(&root_path));
            assert_eq!(
                std::fs::read(root_path.join("marker")).unwrap(),
                b"directory-root"
            );
            assert_eq!(
                std::fs::read(nested_path).unwrap(),
                b"nested-under-root-child"
            );
        }
    }

    #[tokio::test]
    async fn materialize_ipfs_rejects_writes_and_missing_cache() {
        let fs = TestFilesystem::new();
        let (cid, pinner) = test_cid_and_pinner(b"data");
        let cache = fs.isolated_cache(pinner).await;
        let path = IpfsCidPath {
            cid,
            subpath: String::new(),
        };

        assert!(matches!(
            materialize_ipfs_descriptor(&fs.resources, &fs.scope, Some(&cache), &path, true).await,
            Err(MaterializeError::NotPermitted)
        ));
        assert!(matches!(
            materialize_ipfs_descriptor(&fs.resources, &fs.scope, None, &path, false).await,
            Err(MaterializeError::NoEntry)
        ));
    }

    #[tokio::test]
    async fn materialize_ipfs_fetches_subpath_content() {
        let fs = TestFilesystem::new();
        let root_bytes = b"root cid blob bytes";
        let nested_bytes = b"nested file content";
        let (cid, pinner) =
            test_cid_and_pinner_with_subpath(root_bytes, "sub/dir/file.txt", nested_bytes);
        let cache = fs.isolated_cache(pinner).await;
        let path = IpfsCidPath {
            cid,
            subpath: "sub/dir/file.txt".to_string(),
        };

        let descriptor =
            materialize_ipfs_descriptor(&fs.resources, &fs.scope, Some(&cache), &path, false)
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
