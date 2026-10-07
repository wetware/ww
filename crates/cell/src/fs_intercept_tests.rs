//! Tests call the P3 interceptor with a real resource table and WASI descriptors.
use super::*;
use wasmtime::component::ResourceTable;
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::filesystem::Descriptor;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder};

type Fd = Resource<Descriptor>;
const ROOT: &str = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
const NESTED: &str = "bafkreiaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DEEPER: &str = "bafkreibaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SIBLING: &str = "bafkreicaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Harness {
    wasi: WasiCtx,
    table: ResourceTable,
    cache: Option<Arc<cache::CacheMode>>,
    pinner: Arc<CountingPinner>,
    tree: Option<Arc<CidTree>>,
    descriptors: DescriptorContexts,
    staging: tempfile::TempDir,
    scratch: tempfile::TempDir,
}

fn wasi_view(state: &mut Harness) -> WasiFilesystemCtxView<'_> {
    WasiFilesystemCtxView {
        ctx: state.wasi.filesystem(),
        table: &mut state.table,
    }
}
impl FilesystemHostState for Harness {
    fn intercepted_filesystem(&mut self) -> IpfsFilesystemView<'_> {
        IpfsFilesystemView {
            ctx: self.wasi.filesystem(),
            table: &mut self.table,
            cache_mode: &self.cache,
            cid_tree: &self.tree,
            descriptors: &mut self.descriptors,
        }
    }
    fn wasi_filesystem_getter() -> for<'a> fn(&'a mut Self) -> WasiFilesystemCtxView<'a> {
        wasi_view
    }
    fn wasi_filesystem_access(
        store: wasmtime::StoreContextMut<'_, Self>,
    ) -> Access<'_, Self, WasiFilesystem> {
        Access::new(store, |state| wasi_view(state))
    }
}

fn listing(staging: &std::path::Path, cid: &str, children: &[(&str, &str, bool)]) {
    let entries: Vec<_> = children
        .iter()
        .map(|(name, cid, directory)| crate::vfs::DirEntry {
            name: name.to_string(),
            cid: cid.to_string(),
            size: 3,
            entry_type: if *directory {
                crate::vfs::EntryType::Dir
            } else {
                crate::vfs::EntryType::File
            },
        })
        .collect();
    std::fs::write(
        staging.join(format!("{cid}.dirlist.json")),
        serde_json::to_vec(&entries).unwrap(),
    )
    .unwrap();
}
fn harness() -> (Store<Harness>, u32, u32) {
    let staging = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    listing(
        staging.path(),
        ROOT,
        &[
            ("nested", NESTED, true),
            ("sibling", SIBLING, true),
            ("child", ROOT, false),
        ],
    );
    listing(
        staging.path(),
        NESTED,
        &[("deeper", DEEPER, true), ("child", NESTED, false)],
    );
    listing(staging.path(), DEEPER, &[("child", DEEPER, false)]);
    listing(staging.path(), SIBLING, &[("child", SIBLING, false)]);
    let tree = Arc::new(CidTree::new(
        ipfs::cid_identity::parse_cid(ROOT).unwrap(),
        ipfs::HttpClient::new("http://127.0.0.1:1".into()),
        staging.path().into(),
    ));
    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(staging.path(), "/", FsPerms::ReadOnly)
        .unwrap()
        .preopened_dir(scratch.path(), "/tmp", FsPerms::ReadWrite)
        .unwrap();
    let pinner = Arc::new(CountingPinner::default());
    let cache = Some(Arc::new(cache::CacheMode::Isolated(
        cache::IsolatedPinset::new(pinner.clone()).unwrap(),
    )));
    let mut state = Harness {
        wasi: builder.build(),
        table: ResourceTable::new(),
        cache,
        pinner,
        tree: Some(tree),
        descriptors: DescriptorContexts::default(),
        staging,
        scratch,
    };
    let dirs = p3_preopens::Host::get_directories(&mut state.intercepted_filesystem()).unwrap();
    let root = dirs.iter().find(|(_, path)| path == "/").unwrap().0.rep();
    let tmp = dirs
        .iter()
        .find(|(_, path)| path == "/tmp")
        .unwrap()
        .0
        .rep();
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config).unwrap();
    (Store::new(&engine, state), root, tmp)
}
async fn open(
    store: &mut Store<Harness>,
    base: u32,
    path: &str,
    of: p3_types::OpenFlags,
    df: p3_types::DescriptorFlags,
) -> Result<Fd, p3_types::ErrorCode> {
    store
        .run_concurrent(async |access| {
            let access = access.with_getter::<IpfsFilesystem>(Harness::intercepted_filesystem);
            <IpfsFilesystem as p3_types::HostDescriptorWithStore<Harness>>::open_at(
                &access,
                Resource::new_borrow(base),
                p3_types::PathFlags::empty(),
                path.into(),
                of,
                df,
            )
            .await
            .map_err(|e| e.downcast().expect("filesystem error"))
        })
        .await
        .unwrap()
}
async fn read_open(
    store: &mut Store<Harness>,
    base: u32,
    path: &str,
) -> Result<Fd, p3_types::ErrorCode> {
    open(
        store,
        base,
        path,
        p3_types::OpenFlags::empty(),
        p3_types::DescriptorFlags::READ,
    )
    .await
}

#[tokio::test]
async fn alias_root_routes_through_own_tree() {
    let (mut store, _, _) = harness();
    let cid: cid::Cid = NESTED.parse().unwrap();
    let alias = cid
        .to_string_of_base(cid::multibase::Base::Base58Btc)
        .unwrap();
    store.data_mut().tree = Some(Arc::new(CidTree::new(
        ipfs::cid_identity::parse_cid(&alias).unwrap(),
        ipfs::HttpClient::new("http://127.0.0.1:1".into()),
        store.data().staging.path().into(),
    )));
    let dirs =
        p3_preopens::Host::get_directories(&mut store.data_mut().intercepted_filesystem()).unwrap();
    let root = dirs.iter().find(|(_, path)| path == "/").unwrap().0.rep();
    for spelling in [cid.to_string(), alias] {
        let path = format!("ipfs/{spelling}/deeper");
        assert!(matches!(
            store
                .data_mut()
                .intercepted_filesystem()
                .route_open(&Resource::new_borrow(root), &path)
                .unwrap(),
            OpenRoute::CidTree(_, _)
        ));
        let directory = read_open(&mut store, root, &path).await.unwrap();
        assert_eq!(
            store.data().descriptors.directories[&directory.rep()].path,
            "deeper"
        );
    }
    assert!(matches!(
        store
            .data_mut()
            .intercepted_filesystem()
            .route_open(
                &Resource::new_borrow(root),
                &format!("ipfs/{SIBLING}/child")
            )
            .unwrap(),
        OpenRoute::Ipfs(_)
    ));
}

#[tokio::test]
async fn slash_bearing_bare_alias_does_not_change_single_segment_path_grammar() {
    let (mut store, _, _) = harness();
    let cid = cid::Cid::new_v1(
        0x55,
        cid::multihash::Multihash::<64>::wrap(0x12, &[255; 32]).unwrap(),
    );
    let alias = cid.to_string_of_base(cid::multibase::Base::Base64).unwrap();
    assert!(alias.contains('/'));
    assert_eq!(ipfs::cid_identity::parse_cid(&alias).unwrap(), cid);

    store.data_mut().tree = Some(Arc::new(CidTree::new(
        cid,
        ipfs::HttpClient::new("http://127.0.0.1:1".into()),
        store.data().staging.path().into(),
    )));
    let dirs =
        p3_preopens::Host::get_directories(&mut store.data_mut().intercepted_filesystem()).unwrap();
    let root = dirs.iter().find(|(_, path)| path == "/").unwrap().0.rep();
    let path = format!("ipfs/{alias}/child");

    assert!(parse_ipfs_path(&path).is_none());
    match store
        .data_mut()
        .intercepted_filesystem()
        .route_open(&Resource::new_borrow(root), &path)
        .unwrap()
    {
        OpenRoute::CidTree(_, relative) => assert_eq!(relative, path),
        OpenRoute::Ipfs(_) | OpenRoute::Wasi { .. } => {
            panic!("slash-bearing CID text must not select a path-form IPFS route")
        }
    }
}

#[tokio::test]
async fn chained_directory_opens_keep_base_context() {
    let (mut store, root, _) = harness();
    let nested = read_open(&mut store, root, "nested").await.unwrap();
    // Only /nested has a deeper directory. Restarting at root returns NoEntry.
    let deeper = read_open(&mut store, nested.rep(), "deeper").await;
    assert!(deeper.is_ok(), "nested/deeper: {deeper:?}");
}

#[tokio::test]
async fn immutable_flags_reject_before_lookup_or_staging() {
    use p3_types::{DescriptorFlags as D, ErrorCode as E, OpenFlags as O};
    let (mut store, root, _) = harness();
    let before = std::fs::read_dir(store.data().staging.path())
        .unwrap()
        .count();
    for bits in 1..32 {
        let mut of = O::empty();
        let mut df = D::READ;
        if bits & 1 != 0 {
            df |= D::WRITE;
        }
        if bits & 2 != 0 {
            df |= D::MUTATE_DIRECTORY;
        }
        if bits & 4 != 0 {
            of |= O::CREATE;
        }
        if bits & 8 != 0 {
            of |= O::EXCLUSIVE;
        }
        if bits & 16 != 0 {
            of |= O::TRUNCATE;
        }
        for directory in [false, true] {
            let of = if directory { of | O::DIRECTORY } else { of };
            for path in ["child", "missing", "nested", "unlisted/target"] {
                assert_error(open(&mut store, root, path, of, df).await, E::NotPermitted);
            }
        }
    }
    assert_eq!(
        std::fs::read_dir(store.data().staging.path())
            .unwrap()
            .count(),
        before
    );
    assert_eq!(
        store
            .data()
            .pinner
            .0
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        std::fs::read_dir(store.data().cache.as_ref().unwrap().staging_dir())
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn file_base_and_target_types_have_exact_errors() {
    use p3_types::{DescriptorFlags as D, ErrorCode as E, OpenFlags as O};
    let (mut store, root, _) = harness();
    for (path, flags, error) in [
        ("child", O::DIRECTORY, E::NotDirectory),
        ("child/", O::empty(), E::NotDirectory),
        ("child/next", O::empty(), E::NotDirectory),
        ("missing", O::empty(), E::NoEntry),
    ] {
        assert_error(open(&mut store, root, path, flags, D::READ).await, error);
    }
    let file_path = store.data().scratch.path().join("base");
    std::fs::write(&file_path, "bytes").unwrap();
    let base = store
        .data_mut()
        .table
        .push(open_read_only_path(&file_path).unwrap())
        .unwrap();
    for flags in [O::empty(), O::CREATE] {
        assert_error(
            open(&mut store, base.rep(), "nested", flags, D::WRITE).await,
            E::NotDirectory,
        );
    }
    assert!(read_open(&mut store, root, "nested").await.is_ok());
}

fn assert_error(result: Result<Fd, p3_types::ErrorCode>, expected: p3_types::ErrorCode) {
    let actual = result.unwrap_err();
    assert_eq!(
        std::mem::discriminant(&actual),
        std::mem::discriminant(&expected),
        "expected {expected:?}, got {actual:?}"
    );
}

#[derive(Default)]
struct CountingPinner(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl cache::Pinner for CountingPinner {
    async fn pin(&self, _: &cid::Cid) -> Result<()> {
        Ok(())
    }
    async fn unpin(&self, _: &cid::Cid) -> Result<()> {
        Ok(())
    }
    async fn size(&self, _: &cid::Cid) -> Result<u64> {
        Ok(6)
    }
    async fn fetch(&self, cid: &cid::Cid) -> Result<Vec<u8>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(match cid.to_string().as_str() {
            ROOT => b"root".to_vec(),
            NESTED => b"nested".to_vec(),
            DEEPER => b"deeper".to_vec(),
            SIBLING => b"sibling".to_vec(),
            _ => anyhow::bail!("unexpected CID {cid}"),
        })
    }
}
fn read_contents(store: &Store<Harness>, fd: &Fd) -> String {
    use std::io::Read;
    let Descriptor::File(file) = store.data().table.get(fd).unwrap() else {
        panic!("expected file")
    };
    let mut contents = String::new();
    (&*file.file).read_to_string(&mut contents).unwrap();
    contents
}
fn write_contents(store: &Store<Harness>, fd: &Fd, contents: &[u8], append: bool) {
    use std::io::{Seek, SeekFrom, Write};
    let Descriptor::File(file) = store.data().table.get(fd).unwrap() else {
        panic!("expected file")
    };
    if append {
        (&*file.file).seek(SeekFrom::End(0)).unwrap();
    }
    (&*file.file).write_all(contents).unwrap();
}
fn drop_descriptor(store: &mut Store<Harness>, fd: Fd) {
    p3_types::HostDescriptor::drop(&mut store.data_mut().intercepted_filesystem(), fd).unwrap();
}
async fn create_directory(store: &mut Store<Harness>, base: u32, path: &str) {
    store
        .run_concurrent(async |access| {
            let access = access.with_getter::<IpfsFilesystem>(Harness::intercepted_filesystem);
            <IpfsFilesystem as p3_types::HostDescriptorWithStore<Harness>>::create_directory_at(
                &access,
                Resource::new_borrow(base),
                path.into(),
            )
            .await
        })
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn simultaneous_descriptors_select_distinct_same_name_children() {
    let (mut store, root, _) = harness();
    let nested = read_open(&mut store, root, "nested").await.unwrap();
    let sibling = read_open(&mut store, root, "sibling").await.unwrap();
    let deeper = read_open(&mut store, nested.rep(), "deeper").await.unwrap();
    for (base, expected) in [
        (root, "root"),
        (nested.rep(), "nested"),
        (deeper.rep(), "deeper"),
        (sibling.rep(), "sibling"),
        (nested.rep(), "nested"),
    ] {
        let file = read_open(&mut store, base, "child").await.unwrap();
        assert_eq!(read_contents(&store, &file), expected);
    }
}

#[tokio::test]
async fn descriptors_retain_root_snapshot_and_namespace_after_swap() {
    let (mut store, root, _) = harness();
    let nested = read_open(&mut store, root, "nested").await.unwrap();
    store
        .data()
        .tree
        .as_ref()
        .unwrap()
        .swap_root(ipfs::cid_identity::parse_cid(SIBLING).unwrap());
    let file = read_open(&mut store, nested.rep(), "child").await.unwrap();
    assert_eq!(read_contents(&store, &file), "nested");
    let file = read_open(&mut store, root, "child").await.unwrap();
    assert_eq!(read_contents(&store, &file), "root");
    let dirs =
        p3_preopens::Host::get_directories(&mut store.data_mut().intercepted_filesystem()).unwrap();
    let new_root = dirs.iter().find(|(_, path)| path == "/").unwrap().0.rep();
    let file = read_open(&mut store, new_root, "child").await.unwrap();
    assert_eq!(read_contents(&store, &file), "sibling");
    store.data_mut().tree = Some(Arc::new(CidTree::new(
        ipfs::cid_identity::parse_cid(DEEPER).unwrap(),
        ipfs::HttpClient::new("http://127.0.0.1:1".into()),
        store.data().staging.path().into(),
    )));
    let deeper = read_open(&mut store, nested.rep(), "deeper").await.unwrap();
    let file = read_open(&mut store, deeper.rep(), "child").await.unwrap();
    assert_eq!(read_contents(&store, &file), "deeper");
}

#[tokio::test]
async fn drop_and_resource_id_reuse_clear_all_descriptor_context() {
    use p3_types::{DescriptorFlags as D, ErrorCode as E, OpenFlags as O};
    let (mut store, root, tmp) = harness();
    let nested = read_open(&mut store, root, "nested").await.unwrap();
    let id = nested.rep();
    drop_descriptor(&mut store, nested);
    assert!(!store.data().descriptors.directories.contains_key(&id));
    // Reuse the slot with a real non-CidTree descriptor. Its child comes from its host directory.
    let path = store.data().scratch.path().to_path_buf();
    std::fs::write(path.join("child"), "replacement").unwrap();
    let replacement = store
        .data_mut()
        .table
        .push(open_read_only_path(&path).unwrap())
        .unwrap();
    assert_eq!(
        replacement.rep(),
        id,
        "resource slot must actually be reused"
    );
    let file = read_open(&mut store, id, "child").await.unwrap();
    assert_eq!(read_contents(&store, &file), "replacement");
    drop_descriptor(&mut store, file);
    drop_descriptor(&mut store, replacement);

    // Reuse a /tmp slot for a CidTree directory. Mutation must not delegate to WASI.
    drop_descriptor(&mut store, Resource::new_own(tmp));
    assert!(!store.data().descriptors.writable.contains(&tmp));
    let nested = read_open(&mut store, root, "nested").await.unwrap();
    assert_eq!(nested.rep(), tmp);
    assert_error(
        open(&mut store, nested.rep(), "child", O::CREATE, D::READ).await,
        E::NotPermitted,
    );
    let file = read_open(&mut store, nested.rep(), "child").await.unwrap();
    assert_eq!(read_contents(&store, &file), "nested");

    // Reuse a root routing slot for a file: no root or CidTree metadata survives.
    drop_descriptor(&mut store, Resource::new_own(root));
    assert!(!store.data().descriptors.roots.contains(&root));
    assert!(!store.data().descriptors.directories.contains_key(&root));
    let replacement = store
        .data_mut()
        .table
        .push(open_read_only_path(&path.join("child")).unwrap())
        .unwrap();
    assert_eq!(replacement.rep(), root);
    assert_error(
        open(&mut store, root, "nested", O::CREATE, D::WRITE).await,
        E::NotDirectory,
    );
}

#[tokio::test]
async fn nested_paths_stay_in_namespace_and_preserve_literal_spellings() {
    use p3_types::ErrorCode as E;
    let (mut store, root, _) = harness();
    let nested = read_open(&mut store, root, "nested").await.unwrap();
    for path in [
        "..",
        "../sibling",
        "a/../../b",
        "/child",
        "//child",
        "/nested/child",
    ] {
        assert_error(read_open(&mut store, nested.rep(), path).await, E::Invalid);
    }
    for path in [".", "%2e%2e", "%2f", "\\", &format!("ipfs/{ROOT}/child")] {
        assert_error(read_open(&mut store, nested.rep(), path).await, E::NoEntry);
    }
    let file = read_open(&mut store, nested.rep(), "deeper//child")
        .await
        .unwrap();
    assert_eq!(read_contents(&store, &file), "deeper");
    let same = read_open(&mut store, nested.rep(), "").await.unwrap();
    let file = read_open(&mut store, same.rep(), "child").await.unwrap();
    assert_eq!(read_contents(&store, &file), "nested");
    // A directory returned for the root still cannot select global IPFS routing.
    let reopened_root = read_open(&mut store, root, "").await.unwrap();
    assert_error(
        read_open(
            &mut store,
            reopened_root.rep(),
            &format!("ipfs/{SIBLING}/child"),
        )
        .await,
        E::NoEntry,
    );
}

#[tokio::test]
async fn writable_tmp_delegates_and_does_not_inherit_cidtree_context() {
    use p3_types::{DescriptorFlags as D, OpenFlags as O};
    let (mut store, root, tmp) = harness();
    let nested = read_open(&mut store, root, "nested").await.unwrap();
    let reused_id = nested.rep();
    drop_descriptor(&mut store, nested);
    let file = open(&mut store, tmp, "created", O::CREATE, D::READ | D::WRITE)
        .await
        .unwrap();
    assert_eq!(file.rep(), reused_id);
    assert!(!store
        .data()
        .descriptors
        .directories
        .contains_key(&file.rep()));
    assert!(store.data().descriptors.writable.contains(&file.rep()));
    std::fs::write(store.data().scratch.path().join("created"), "scratch-data").unwrap();
    drop_descriptor(&mut store, file);
    let file = open(&mut store, tmp, "created", O::TRUNCATE, D::READ | D::WRITE)
        .await
        .unwrap();
    assert_eq!(read_contents(&store, &file), "");
}

#[tokio::test]
async fn nested_writable_tmp_descriptor_preserves_writable_semantics() {
    use p3_types::{DescriptorFlags as D, OpenFlags as O};
    let (mut store, _, tmp) = harness();

    create_directory(&mut store, tmp, "nested").await;
    let nested = open(
        &mut store,
        tmp,
        "nested",
        O::DIRECTORY,
        D::READ | D::MUTATE_DIRECTORY,
    )
    .await
    .unwrap();
    assert!(store.data().descriptors.writable.contains(&nested.rep()));

    let file = open(
        &mut store,
        nested.rep(),
        "probe.txt",
        O::CREATE | O::EXCLUSIVE,
        D::READ | D::WRITE,
    )
    .await
    .unwrap();
    write_contents(&store, &file, b"nested scratch", false);
    write_contents(&store, &file, b" appended", true);
    drop_descriptor(&mut store, file);
    assert_eq!(
        std::fs::read(store.data().scratch.path().join("nested/probe.txt")).unwrap(),
        b"nested scratch appended"
    );

    let file = open(
        &mut store,
        nested.rep(),
        "probe.txt",
        O::TRUNCATE,
        D::READ | D::WRITE,
    )
    .await
    .unwrap();
    assert_eq!(read_contents(&store, &file), "");
    write_contents(&store, &file, b"after truncate", false);
    drop_descriptor(&mut store, file);

    let file = read_open(&mut store, nested.rep(), "probe.txt")
        .await
        .unwrap();
    assert_eq!(read_contents(&store, &file), "after truncate");
}

#[tokio::test]
async fn mutation_on_uncached_root_does_not_request_a_listing() {
    use p3_types::{DescriptorFlags as D, ErrorCode as E, OpenFlags as O};
    let (mut store, _, _) = harness();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let staging = tempfile::tempdir().unwrap();
    let tree = Arc::new(CidTree::new(
        ipfs::cid_identity::parse_cid(ROOT).unwrap(),
        ipfs::HttpClient::new(format!("http://{}", listener.local_addr().unwrap())),
        staging.path().into(),
    ));
    store.data_mut().tree = Some(tree);
    let dirs =
        p3_preopens::Host::get_directories(&mut store.data_mut().intercepted_filesystem()).unwrap();
    let root = dirs.iter().find(|(_, path)| path == "/").unwrap().0.rep();
    for (of, df) in [
        (O::empty(), D::WRITE),
        (O::empty(), D::MUTATE_DIRECTORY),
        (O::CREATE, D::READ),
        (O::EXCLUSIVE, D::READ),
        (O::TRUNCATE, D::READ),
    ] {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            open(&mut store, root, "uncached", of, df),
        )
        .await
        .expect("mutation must reject without waiting for IPFS");
        assert_error(result, E::NotPermitted);
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
    assert_eq!(
        store
            .data()
            .pinner
            .0
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}
