//! Verified immutable kernel-file selection. The caller retains the selected root.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use cid::Cid;
use futures::future::BoxFuture;

use super::codec::{self, Node};
use super::composer::{self, BlockSource, InspectionBudget};
use super::{CompositionError, SourceKind};
use crate::sched::MAX_COMPONENT_BYTES;

/// Exact verified bytes selected from `/boot/kernel.wasm` at an explicit root.
///
/// This value proves immutable file identity and structure, not WASM validity,
/// kernel compatibility, pin ownership, or authority to execute the component.
#[derive(Clone, Debug)]
pub struct VerifiedKernelFile {
    effective_root: Cid,
    source_file: Cid,
    bytes: Arc<[u8]>,
}

impl VerifiedKernelFile {
    pub fn effective_root(&self) -> Cid {
        self.effective_root
    }

    pub fn source_file(&self) -> Cid {
        self.source_file
    }

    pub fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }
}

/// Selection failures distinguish absent paths from unavailable block content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelFileFailure {
    MissingPath,
    InvalidSource,
    Unavailable,
    Cancelled,
    /// An unclassified Host or protocol fault; callers must fail explicitly.
    Unknown,
}

/// A typed selection failure retaining the original Composer or transport error.
#[derive(Debug)]
pub struct KernelFileError {
    kind: KernelFileFailure,
    source: anyhow::Error,
}

impl KernelFileError {
    pub fn kind(&self) -> KernelFileFailure {
        self.kind
    }

    fn selection(source: anyhow::Error) -> Self {
        let kind = match source.downcast_ref::<CompositionError>() {
            Some(CompositionError::MissingPath(_)) => KernelFileFailure::MissingPath,
            Some(_) => KernelFileFailure::InvalidSource,
            None if ipfs::is_kubo_unavailable_error(&source)
                || source.chain().any(|cause| {
                    cause
                        .downcast_ref::<ipfs::KuboApiError>()
                        .is_some_and(ipfs::KuboApiError::is_server_error)
                        || cause
                            .downcast_ref::<ipfs::KuboResponseLimit>()
                            .is_some_and(|limit| {
                                limit.status.is_server_error() || limit.status.as_u16() == 429
                            })
                }) =>
            {
                KernelFileFailure::Unavailable
            }
            None => KernelFileFailure::Unknown,
        };
        Self { kind, source }
    }

    fn cancelled() -> Self {
        Self {
            kind: KernelFileFailure::Cancelled,
            source: anyhow::anyhow!("kernel file selection cancelled"),
        }
    }
}

impl fmt::Display for KernelFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "kernel file selection failed ({:?}): {:#}",
            self.kind, self.source
        )
    }
}

impl std::error::Error for KernelFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Select and assemble `/boot/kernel.wasm` relative to `effective_root`.
///
/// The caller must retain this root throughout the read. This function never
/// reads the current `CidTree`, acquires pins, follows symlinks, or executes code.
/// Ancestors must be directories; the leaf must be a raw or UnixFS regular file.
/// Composer v1 verifies SHA-256 block identity, structure, lengths, and DAG limits.
/// Declared logical length and assembled bytes separately obey the 8 MiB limit.
/// Ordinary UnixFS mode and mtime metadata are accepted.
///
/// A missing directory entry yields `MissingPath`. Missing blocks, timeouts, and
/// transport failures yield `Unavailable`. Invalid content and resource limits
/// yield `InvalidSource`, preserving `CompositionError` in the error chain.
/// Unclassified Host or protocol faults yield `Unknown` and must fail explicitly.
/// Dropping this future cancels block retrieval; the caller still owns retention.
/// Successful selection requires a separate production kernel validation step.
pub async fn select_kernel_file(
    effective_root: Cid,
    client: &ipfs::BootClient,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<VerifiedKernelFile, KernelFileError> {
    select_with_cancel(client, effective_root, cancel).await
}

async fn select_with_cancel(
    source: &impl BlockSource,
    effective_root: Cid,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<VerifiedKernelFile, KernelFileError> {
    if *cancel.borrow() {
        return Err(KernelFileError::cancelled());
    }
    tokio::select! {
        biased;
        _ = cancel.changed() => Err(KernelFileError::cancelled()),
        result = select_from(source, effective_root) => result,
    }
}

async fn select_from(
    source: &impl BlockSource,
    effective_root: Cid,
) -> Result<VerifiedKernelFile, KernelFileError> {
    async {
        let mut inspection = InspectionBudget::default();
        let selected = composer::inspect_path(
            source,
            effective_root,
            &["boot".into(), "kernel.wasm".into()],
            &mut inspection,
        )
        .await?;
        ensure!(
            selected.kind() == SourceKind::File,
            CompositionError::InvalidSource("/boot/kernel.wasm is not a regular file".into())
        );
        // Retain only verified selected-file blocks. Both inspection passes share
        // the existing cumulative DAG allowance; assembly has separate budgets.
        let verified = composer::inspect_capture(source, selected.cid(), &mut inspection).await?;
        let mut assembly = Assembly {
            blocks: &verified.blocks,
            bytes: Vec::new(),
            work: InspectionBudget::default(),
        };
        let logical_size = assembly.logical_size(selected.cid())?;
        ensure!(
            logical_size <= MAX_COMPONENT_BYTES as u64,
            CompositionError::ResourceLimit("kernel file exceeds 8 MiB logical component limit")
        );
        assembly.bytes.reserve(logical_size as usize);
        assembly.file(selected.cid()).await?;
        ensure!(
            assembly.bytes.len() as u64 == logical_size,
            CompositionError::InvalidDag(
                "assembled kernel file length differs from declared length".into()
            )
        );
        Ok(VerifiedKernelFile {
            effective_root,
            source_file: selected.cid(),
            bytes: assembly.bytes.into(),
        })
    }
    .await
    .map_err(KernelFileError::selection)
}

struct Assembly<'a> {
    blocks: &'a HashMap<Cid, Vec<u8>>,
    bytes: Vec<u8>,
    work: InspectionBudget,
}

fn invalid_dag(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<CompositionError>().is_some() {
        error
    } else {
        CompositionError::InvalidDag(format!("{error:#}")).into()
    }
}

impl Assembly<'_> {
    fn block(&self, cid: Cid) -> Result<&Vec<u8>> {
        self.blocks
            .get(&cid)
            .context("missing verified kernel file block")
            .map_err(invalid_dag)
    }

    fn logical_size(&self, cid: Cid) -> Result<u64> {
        let bytes = self.block(cid)?;
        if cid.codec() == 0x55 {
            return Ok(bytes.len() as u64);
        }
        match codec::decode(&cid, bytes.clone()).map_err(invalid_dag)? {
            Node::File { file, .. } => Ok(file.logical_size),
            Node::Raw { logical_size, .. } => Ok(logical_size),
            _ => Err(CompositionError::InvalidSource(
                "selected kernel is not a regular file".into(),
            )
            .into()),
        }
    }

    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= MAX_COMPONENT_BYTES.saturating_sub(self.bytes.len()),
            CompositionError::ResourceLimit("assembled kernel file exceeds 8 MiB component limit")
        );
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn file(&mut self, cid: Cid) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            // Charge every occurrence, including repeated and zero-length chunks.
            self.work.charge_work()?;
            let bytes = self.block(cid)?.clone();
            if cid.codec() == 0x55 {
                return self.append(&bytes);
            }
            self.append(&codec::file_inline(&bytes).map_err(invalid_dag)?)?;
            match codec::decode(&cid, bytes).map_err(invalid_dag)? {
                Node::File { file, .. } => {
                    for link in file.links {
                        self.file(link.cid).await?;
                    }
                }
                Node::Raw { .. } => (),
                _ => {
                    return Err(CompositionError::InvalidDag(
                        "unsupported kernel file child".into(),
                    )
                    .into())
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use bytes::Bytes;
    use cid::Cid;
    use ipld_dagpb::{PbLink, PbNode};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::image::{codec, composer};
    use crate::sched::MAX_COMPONENT_BYTES;

    #[derive(Default)]
    struct Store {
        blocks: HashMap<Cid, Vec<u8>>,
        reads: Mutex<Vec<Cid>>,
    }

    #[async_trait]
    impl composer::BlockSource for Store {
        async fn get(&self, cid: &Cid) -> Result<Vec<u8>> {
            self.reads.lock().unwrap().push(*cid);
            self.blocks.get(cid).cloned().ok_or_else(|| {
                ipfs::KuboOperationTimeout::new(
                    "test retrieval of missing block",
                    std::time::Duration::from_secs(1),
                )
                .into()
            })
        }
    }

    #[derive(Clone)]
    struct Entry {
        cid: Cid,
        dag_size: u64,
        logical_size: u64,
    }

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value > 0x7f {
            out.push(value as u8 | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    impl Store {
        fn block(&mut self, codec: u64, bytes: Vec<u8>) -> Entry {
            let cid = Cid::new_v1(
                codec,
                cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&bytes)).unwrap(),
            );
            let size = bytes.len() as u64;
            self.blocks.insert(cid, bytes);
            Entry {
                cid,
                dag_size: size,
                logical_size: size,
            }
        }

        fn pb(&mut self, data: Vec<u8>, links: Vec<PbLink>, logical_size: u64) -> Entry {
            let descendants = links.iter().map(|link| link.size.unwrap()).sum::<u64>();
            let mut entry = self.block(
                0x70,
                PbNode {
                    data: Some(Bytes::from(data)),
                    links,
                }
                .into_bytes(),
            );
            entry.dag_size += descendants;
            entry.logical_size = logical_size;
            entry
        }

        fn raw(&mut self, bytes: &[u8]) -> Entry {
            self.block(0x55, bytes.to_vec())
        }

        fn file(&mut self, inline: &[u8], children: &[Entry], metadata: bool) -> Entry {
            let mut data = vec![8, 2];
            if !inline.is_empty() {
                data.push(0x12);
                varint(inline.len() as u64, &mut data);
                data.extend_from_slice(inline);
            }
            let logical_size =
                inline.len() as u64 + children.iter().map(|child| child.logical_size).sum::<u64>();
            data.push(0x18);
            varint(logical_size, &mut data);
            for child in children {
                data.push(0x20);
                varint(child.logical_size, &mut data);
            }
            if metadata {
                // mode 0755 and mtime 42 seconds + 1 nanosecond.
                data.extend_from_slice(&[
                    0x38, 0xed, 0x03, 0x42, 0x07, 0x08, 0x2a, 0x15, 1, 0, 0, 0,
                ]);
            }
            let links = children
                .iter()
                .map(|child| PbLink {
                    cid: child.cid,
                    name: Some(String::new()),
                    size: Some(child.dag_size),
                })
                .collect();
            self.pb(data, links, logical_size)
        }

        fn dir(&mut self, entries: &[(&str, &Entry)]) -> Entry {
            self.pb(
                vec![8, 1],
                entries
                    .iter()
                    .map(|(name, child)| PbLink {
                        cid: child.cid,
                        name: Some((*name).into()),
                        size: Some(child.dag_size),
                    })
                    .collect(),
                0,
            )
        }

        fn root(&mut self, file: &Entry) -> Cid {
            let boot = self.dir(&[("kernel.wasm", file)]);
            self.dir(&[("boot", &boot)]).cid
        }

        fn symlink(&mut self) -> Entry {
            self.pb(vec![8, 4, 0x12, 1, b'x'], Vec::new(), 0)
        }
    }

    async fn select(store: &Store, root: Cid) -> Result<VerifiedKernelFile, KernelFileError> {
        select_from(store, root).await
    }

    fn kind(error: KernelFileError) -> KernelFileFailure {
        error.kind()
    }

    #[tokio::test]
    async fn raw_unixfs_and_metadata_preserve_exact_bytes_and_identity() {
        let mut store = Store::default();
        let bytes = b"\0asm\x0d\0\x01\0";
        let raw = store.raw(bytes);
        let flat = store.file(bytes, &[], false);
        let metadata = store.file(bytes, &[], true);
        assert_ne!(raw.cid, flat.cid);
        assert_ne!(flat.cid, metadata.cid);
        for entry in [raw, flat, metadata] {
            let root = store.root(&entry);
            let selected = select(&store, root).await.unwrap();
            assert_eq!(selected.effective_root(), root);
            assert_eq!(selected.source_file(), entry.cid);
            assert_eq!(selected.bytes().as_ref(), bytes);
        }
    }

    #[tokio::test]
    async fn explicitly_selected_root_determines_the_file() {
        let mut store = Store::default();
        let a = store.raw(b"a");
        let b = store.raw(b"b");
        let root_a = store.root(&a);
        let root_b = store.root(&b);
        assert_eq!(select(&store, root_b).await.unwrap().bytes().as_ref(), b"b");
        assert_eq!(select(&store, root_a).await.unwrap().bytes().as_ref(), b"a");
    }

    #[tokio::test]
    async fn missing_path_is_distinct_from_unavailable_root_or_file_block() {
        let mut store = Store::default();
        let empty = store.dir(&[]);
        assert_eq!(
            kind(select(&store, empty.cid).await.unwrap_err()),
            KernelFileFailure::MissingPath
        );
        let root = store.dir(&[("boot", &empty)]).cid;
        assert_eq!(
            kind(select(&store, root).await.unwrap_err()),
            KernelFileFailure::MissingPath
        );
        let leaf = store.raw(b"x");
        let root = store.root(&leaf);
        store.blocks.remove(&leaf.cid);
        assert_eq!(
            kind(select(&store, root).await.unwrap_err()),
            KernelFileFailure::Unavailable
        );
        store.blocks.remove(&root);
        assert_eq!(
            kind(select(&store, root).await.unwrap_err()),
            KernelFileFailure::Unavailable
        );
    }

    #[tokio::test]
    async fn rejects_file_ancestors_directory_leaf_and_all_symlink_positions() {
        let mut store = Store::default();
        let raw = store.raw(b"x");
        let directory = store.dir(&[]);
        let symlink = store.symlink();
        let file_ancestor = store.dir(&[("boot", &raw)]).cid;
        let symlink_ancestor = store.dir(&[("boot", &symlink)]).cid;
        let directory_leaf = store.root(&directory);
        let symlink_leaf = store.root(&symlink);
        for root in [
            raw.cid,
            symlink.cid,
            file_ancestor,
            symlink_ancestor,
            directory_leaf,
            symlink_leaf,
        ] {
            assert_eq!(
                kind(select(&store, root).await.unwrap_err()),
                KernelFileFailure::InvalidSource
            );
        }
    }

    #[tokio::test]
    async fn rejects_corrupted_raw_and_dagpb_blocks() {
        for corrupt_leaf in [true, false] {
            let mut store = Store::default();
            let leaf = store.raw(b"x");
            let root = store.root(&leaf);
            let cid = if corrupt_leaf { leaf.cid } else { root };
            store.blocks.get_mut(&cid).unwrap()[0] ^= 1;
            let error = select(&store, root).await.unwrap_err();
            assert_eq!(error.kind(), KernelFileFailure::InvalidSource);
            assert!(error.to_string().contains("CID"));
        }
    }

    #[tokio::test]
    async fn malformed_metadata_and_file_links_remain_permanent() {
        let mut store = Store::default();
        let malformed = store.pb(vec![8, 2, 0x12, 1, b'x', 0x18, 2], vec![], 2);
        let raw = store.raw(b"x");
        let mut wrong_size = raw.clone();
        wrong_size.logical_size = 2;
        let wrong_file_length = store.file(&[], &[wrong_size], false);
        let mut wrong_tsize = raw.clone();
        wrong_tsize.dag_size += 1;
        let wrong_child_size = store.file(&[], &[wrong_tsize.clone()], false);
        let directory = store.dir(&[]);
        let directory_chunk = store.file(&[], &[directory], false);
        let symlink = store.symlink();
        let symlink_chunk = store.file(&[], &[symlink], false);
        for entry in [
            malformed,
            wrong_file_length,
            wrong_child_size,
            directory_chunk,
            symlink_chunk,
            wrong_tsize,
        ] {
            let root = store.root(&entry);
            let error = select(&store, root).await.unwrap_err();
            assert_eq!(error.kind(), KernelFileFailure::InvalidSource);
            assert!(std::error::Error::source(&error)
                .unwrap()
                .is::<crate::image::CompositionError>());
        }
    }

    #[tokio::test]
    async fn assembles_inline_nested_raw_and_unixfs_chunks_in_order() {
        let mut store = Store::default();
        let raw = store.raw(b"b");
        let unixfs_raw = store.pb(vec![8, 0, 0x12, 1, b'c', 0x18, 1], vec![], 1);
        let nested = store.file(b"a", &[raw, unixfs_raw], true);
        let leaf = store.file(b"prefix", &[nested.clone(), nested], false);
        let root = store.root(&leaf);
        assert_eq!(
            select(&store, root).await.unwrap().bytes().as_ref(),
            b"prefixabcabc"
        );
    }

    #[tokio::test]
    async fn logical_limit_counts_repeated_chunks_and_excludes_dag_overhead() {
        let mut store = Store::default();
        let chunk = store.raw(&vec![b'x'; codec::MAX_BLOCK_BYTES]);
        let count = MAX_COMPONENT_BYTES / codec::MAX_BLOCK_BYTES;
        let children = vec![chunk; count];
        let exact = store.file(&[], &children, true);
        assert!(exact.dag_size > MAX_COMPONENT_BYTES as u64);
        let root = store.root(&exact);
        let result = select(&store, root).await.unwrap();
        assert_eq!(result.bytes().len(), MAX_COMPONENT_BYTES);
        assert!(result.bytes().iter().all(|byte| *byte == b'x'));
        let oversize = store.file(b"x", &children, false);
        let root = store.root(&oversize);
        assert_eq!(
            kind(select(&store, root).await.unwrap_err()),
            KernelFileFailure::InvalidSource
        );
    }

    #[tokio::test]
    async fn missing_multiblock_child_is_unavailable() {
        let mut store = Store::default();
        let chunk = store.raw(b"x");
        let leaf = store.file(&[], std::slice::from_ref(&chunk), false);
        let root = store.root(&leaf);
        store.blocks.remove(&chunk.cid);
        assert_eq!(
            kind(select(&store, root).await.unwrap_err()),
            KernelFileFailure::Unavailable
        );
    }

    #[tokio::test]
    async fn selection_does_not_claim_to_validate_component_bytes() {
        let mut store = Store::default();
        let leaf = store.raw(b"invalid component bytes");
        let root = store.root(&leaf);
        assert_eq!(
            select(&store, root).await.unwrap().bytes().as_ref(),
            b"invalid component bytes"
        );
    }

    #[tokio::test]
    async fn cancellation_before_read_and_during_block_fetch_is_typed() {
        let mut store = Store::default();
        let leaf = store.raw(b"x");
        let root = store.root(&leaf);
        let (_sender, mut cancel) = tokio::sync::watch::channel(true);
        let error = select_with_cancel(&store, root, &mut cancel)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), KernelFileFailure::Cancelled);
        assert!(store.reads.lock().unwrap().is_empty());

        struct Blocked(tokio::sync::Notify);
        #[async_trait]
        impl composer::BlockSource for Blocked {
            async fn get(&self, _: &Cid) -> Result<Vec<u8>> {
                self.0.notify_one();
                std::future::pending().await
            }
        }
        let blocked = Blocked(tokio::sync::Notify::new());
        let (sender, mut cancel) = tokio::sync::watch::channel(false);
        let (result, ()) = tokio::join!(select_with_cancel(&blocked, root, &mut cancel), async {
            blocked.0.notified().await;
            sender.send(true).unwrap();
        });
        assert_eq!(result.unwrap_err().kind(), KernelFileFailure::Cancelled);
    }

    #[tokio::test]
    async fn actual_assembly_enforces_limit_even_without_declared_size_precheck() {
        let mut store = Store::default();
        let chunk = store.raw(&vec![b'x'; codec::MAX_BLOCK_BYTES]);
        let children = vec![chunk; MAX_COMPONENT_BYTES / codec::MAX_BLOCK_BYTES];
        let exact = store.file(&[], &children, false);
        let oversized = store.file(b"x", &children, false);
        for entry in [exact, oversized] {
            let inspected =
                composer::inspect_capture(&store, entry.cid, &mut InspectionBudget::default())
                    .await
                    .unwrap();
            let mut assembly = Assembly {
                blocks: &inspected.blocks,
                bytes: Vec::new(),
                work: InspectionBudget::default(),
            };
            let result = assembly.file(entry.cid).await;
            assert!(assembly.bytes.len() <= MAX_COMPONENT_BYTES);
            if entry.logical_size == MAX_COMPONENT_BYTES as u64 {
                result.unwrap();
                assert_eq!(assembly.bytes.len(), MAX_COMPONENT_BYTES);
            } else {
                assert!(matches!(
                    result.unwrap_err().downcast_ref::<CompositionError>(),
                    Some(CompositionError::ResourceLimit(_))
                ));
            }
        }
    }

    #[tokio::test]
    async fn preserves_block_size_and_depth_limits_as_permanent_errors() {
        let mut store = Store::default();
        let oversized = store.raw(&vec![0; codec::MAX_BLOCK_BYTES + 1]);
        let root = store.root(&oversized);
        assert_eq!(
            select(&store, root).await.unwrap_err().kind(),
            KernelFileFailure::InvalidSource
        );

        let mut nested = store.raw(b"x");
        for _ in 0..128 {
            nested = store.file(&[], &[nested], false);
        }
        let root = store.root(&nested);
        let error = select(&store, root).await.unwrap_err();
        assert_eq!(error.kind(), KernelFileFailure::InvalidSource);
        assert!(error.to_string().contains("depth limit"));
    }

    #[tokio::test]
    async fn rejects_other_hash_profiles_and_codecs_without_fetching_them() {
        let mut store = Store::default();
        let mut entry = store.raw(b"x");
        entry.cid = Cid::new_v1(
            0x55,
            cid::multihash::Multihash::wrap(0x1e, &[0; 32]).unwrap(),
        );
        let root = store.root(&entry);
        assert_eq!(
            select(&store, root).await.unwrap_err().kind(),
            KernelFileFailure::InvalidSource
        );
        assert!(!store.reads.lock().unwrap().contains(&entry.cid));
        let hash = store.raw(b"y").cid.hash().to_owned();
        entry.cid = Cid::new_v1(0x71, hash);
        let root = store.root(&entry);
        assert_eq!(
            select(&store, root).await.unwrap_err().kind(),
            KernelFileFailure::InvalidSource
        );
        assert!(!store.reads.lock().unwrap().contains(&entry.cid));
    }

    #[tokio::test]
    async fn distinguishes_backend_unavailability_from_unknown_host_and_protocol_faults() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut store = Store::default();
        let root = store.dir(&[]).cid;
        for (status, expected) in [
            ("500 Internal Server Error", KernelFileFailure::Unavailable),
            ("503 Service Unavailable", KernelFileFailure::Unavailable),
            ("429 Too Many Requests", KernelFileFailure::Unavailable),
            ("400 Bad Request", KernelFileFailure::Unknown),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut buffer = [0; 2048];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0, "request ended before its headers");
                    request.extend_from_slice(&buffer[..count]);
                    assert!(request.len() <= 8192, "unexpectedly large test request");
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing"
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let http = ipfs::HttpClient::new(format!("http://{address}"));
            let client = ipfs::BootClient::new(http, 0, 1);
            let (_sender, mut cancel) = tokio::sync::watch::channel(false);
            let error = select_kernel_file(root, &client, &mut cancel)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), expected, "{status}: {error}");
            assert!(error.source.downcast_ref::<ipfs::KuboApiError>().is_some());
            server.await.unwrap();
        }
        let error =
            KernelFileError::selection(anyhow::anyhow!("unexpected Host invariant failure"));
        assert_eq!(error.kind(), KernelFileFailure::Unknown);
    }
}
