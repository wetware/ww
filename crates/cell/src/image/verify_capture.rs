//! Bind independently inspected UnixFS content to captured local manifests.
//!
//! Composer validates the complete DAG first. This pass compares exact entry
//! names/types and streams file bytes through SHA-256 from those verified blocks.
//! Neither pass reads host paths or trusts `/add` as a content oracle.
use std::collections::{BTreeMap, HashMap};

use anyhow::{ensure, Context, Result};
use cid::Cid;
use futures::future::BoxFuture;
use ipfs::snapshot::{AddedEntry, CapturedEntry};
use sha2::{Digest, Sha256};

use super::codec::{self, Node};
use super::{capture::CapturedSource, composer, FrozenSource};

pub(super) use composer::InspectionBudget as VerificationBudget;

pub(super) async fn verify(
    source: &impl composer::BlockSource,
    captured: &CapturedSource,
    events: &BTreeMap<String, AddedEntry>,
    budget: &mut VerificationBudget,
) -> Result<FrozenSource> {
    verify_manifest(
        source,
        captured.entries(),
        captured.digests(),
        events,
        budget,
    )
    .await
}

async fn verify_manifest(
    source: &impl composer::BlockSource,
    entries: &[CapturedEntry],
    digests: &BTreeMap<String, [u8; 32]>,
    events: &BTreeMap<String, AddedEntry>,
    budget: &mut VerificationBudget,
) -> Result<FrozenSource> {
    ensure!(
        entries.len() == events.len(),
        "capture and /add entry counts differ"
    );
    let manifest: BTreeMap<_, _> = entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    ensure!(
        manifest.len() == entries.len(),
        "duplicate captured manifest paths"
    );
    ensure!(
        manifest
            .keys()
            .copied()
            .eq(events.keys().map(String::as_str)),
        "capture and /add paths differ"
    );
    let root = events.get("").context("missing captured root event")?;
    let inspected = composer::inspect_capture(source, root.cid, budget).await?;
    let mut comparison = Comparison {
        manifest,
        digests,
        events,
        blocks: &inspected.blocks,
        budget,
        matched: 0,
    };
    comparison.entry(root.cid, "").await?;
    ensure!(
        comparison.matched == entries.len(),
        "DAG omits captured manifest entries"
    );
    Ok(inspected.source)
}

struct Comparison<'a> {
    manifest: BTreeMap<&'a str, &'a CapturedEntry>,
    digests: &'a BTreeMap<String, [u8; 32]>,
    events: &'a BTreeMap<String, AddedEntry>,
    blocks: &'a HashMap<Cid, Vec<u8>>,
    budget: &'a mut VerificationBudget,
    matched: usize,
}

impl Comparison<'_> {
    fn entry<'a>(&'a mut self, cid: Cid, path: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.budget.charge_work()?;
            let expected = *self
                .manifest
                .get(path)
                .context("DAG contains uncaptured path")?;
            let event = self
                .events
                .get(path)
                .context("DAG entry missing from /add events")?;
            ensure!(event.cid == cid, "/add CID differs from DAG at {path:?}");
            let bytes = self
                .blocks
                .get(&cid)
                .context("missing verified capture block")?;
            let dag_size = if cid.codec() == 0x55 {
                ensure!(
                    expected.data.is_some(),
                    "raw DAG node replaces captured directory"
                );
                bytes.len() as u64
            } else {
                codec::capture_inline(bytes)?;
                match codec::decode(&cid, bytes.clone())? {
                    Node::Directory(directory) => {
                        ensure!(
                            expected.data.is_none(),
                            "directory DAG node replaces captured file"
                        );
                        let mut size = bytes.len() as u64;
                        for (name, link) in directory.entries {
                            size = size
                                .checked_add(link.size)
                                .context("capture Tsize overflow")?;
                            // Captured paths are already bounded; reject an unknown path
                            // before descending, including a renamed or extra directory.
                            ensure!(
                                path.len()
                                    .saturating_add(name.len())
                                    .saturating_add(usize::from(!path.is_empty()))
                                    <= 4096,
                                "DAG path exceeds captured manifest path limit"
                            );
                            let child = if path.is_empty() {
                                name
                            } else {
                                format!("{path}/{name}")
                            };
                            self.entry(link.cid, &child).await?;
                        }
                        size
                    }
                    Node::File { size, .. } | Node::Raw { size, .. } => {
                        ensure!(
                            expected.data.is_some(),
                            "file DAG node replaces captured directory"
                        );
                        size
                    }
                    Node::Symlink { .. } => anyhow::bail!("symlink in captured DAG"),
                }
            };
            ensure!(
                event.dag_size == dag_size,
                "/add cumulative size differs from DAG at {path:?}"
            );
            if let Some(content) = &expected.data {
                let mut hash = Sha256::new();
                let mut remaining = content.len() as u64;
                self.hash_file(cid, &mut hash, &mut remaining).await?;
                ensure!(
                    remaining == 0,
                    "DAG file is shorter than captured bytes at {path:?}"
                );
                let expected_hash = self
                    .digests
                    .get(path)
                    .context("missing captured file digest")?;
                ensure!(
                    hash.finalize().as_slice() == expected_hash,
                    "DAG file bytes differ from capture at {path:?}"
                );
            }
            self.matched += 1;
            Ok(())
        })
    }

    fn hash_file<'a>(
        &'a mut self,
        cid: Cid,
        hash: &'a mut Sha256,
        remaining: &'a mut u64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.budget.charge_work()?;
            let bytes = self
                .blocks
                .get(&cid)
                .context("missing verified file block")?;
            if cid.codec() == 0x55 {
                feed(hash, remaining, bytes)?;
                return Ok(());
            }
            let inline = codec::capture_inline(bytes)?;
            feed(hash, remaining, &inline)?;
            match codec::decode(&cid, bytes.clone())? {
                Node::File { file, .. } => {
                    for link in file.links {
                        self.hash_file(link.cid, hash, remaining).await?;
                    }
                }
                Node::Raw { .. } => (),
                _ => anyhow::bail!("unsupported captured file child"),
            }
            Ok(())
        })
    }
}

fn feed(hash: &mut Sha256, remaining: &mut u64, bytes: &[u8]) -> Result<()> {
    *remaining = remaining
        .checked_sub(bytes.len() as u64)
        .context("DAG file exceeds captured byte length")?;
    hash.update(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use ipld_dagpb::{PbLink, PbNode};

    #[derive(Default)]
    struct Store(HashMap<Cid, Vec<u8>>);
    #[async_trait]
    impl composer::BlockSource for Store {
        async fn get(&self, cid: &Cid) -> Result<Vec<u8>> {
            self.0.get(cid).cloned().context("missing block")
        }
    }
    impl Store {
        fn block(&mut self, codec: u64, bytes: Vec<u8>) -> AddedEntry {
            let cid = Cid::new_v1(
                codec,
                cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&bytes)).unwrap(),
            );
            let dag_size = bytes.len() as u64;
            self.0.insert(cid, bytes);
            AddedEntry { cid, dag_size }
        }
        fn pb(&mut self, data: &[u8], links: Vec<PbLink>) -> AddedEntry {
            let children = links.iter().map(|l| l.size.unwrap()).sum::<u64>();
            let mut result = self.block(
                0x70,
                PbNode {
                    data: Some(Bytes::copy_from_slice(data)),
                    links,
                }
                .into_bytes(),
            );
            result.dag_size += children;
            result
        }
    }
    fn file(path: &str, bytes: &'static [u8]) -> CapturedEntry {
        CapturedEntry {
            path: path.into(),
            data: Some(Bytes::from_static(bytes)),
        }
    }
    fn dir(path: &str) -> CapturedEntry {
        CapturedEntry {
            path: path.into(),
            data: None,
        }
    }
    async fn check(
        store: &Store,
        entries: &[CapturedEntry],
        events: BTreeMap<String, AddedEntry>,
    ) -> Result<FrozenSource> {
        let hashes = entries
            .iter()
            .filter_map(|e| {
                e.data
                    .as_ref()
                    .map(|b| (e.path.clone(), Sha256::digest(b).into()))
            })
            .collect();
        verify_manifest(
            store,
            entries,
            &hashes,
            &events,
            &mut VerificationBudget::default(),
        )
        .await
    }
    fn link(name: &str, child: &AddedEntry) -> PbLink {
        PbLink {
            cid: child.cid,
            name: Some(name.into()),
            size: Some(child.dag_size),
        }
    }

    #[tokio::test]
    async fn binds_raw_content_length_digest_cid_and_event_size() {
        let mut store = Store::default();
        let event = store.block(0x55, b"abc".to_vec());
        assert_eq!(
            check(
                &store,
                &[file("", b"abc")],
                BTreeMap::from([(String::new(), event.clone())])
            )
            .await
            .unwrap()
            .dag_size(),
            3
        );
        for bytes in [b"abd".as_slice(), b"ab", b"abcd"] {
            assert!(check(
                &store,
                &[file("", bytes)],
                BTreeMap::from([(String::new(), event.clone())])
            )
            .await
            .is_err());
        }
        let mut bad_size = event.clone();
        bad_size.dag_size = 4;
        assert!(check(
            &store,
            &[file("", b"abc")],
            BTreeMap::from([(String::new(), bad_size)])
        )
        .await
        .is_err());
        store.0.insert(event.cid, b"abd".to_vec());
        assert!(check(
            &store,
            &[file("", b"abc")],
            BTreeMap::from([(String::new(), event)])
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn exact_directory_paths_types_and_empty_directories() {
        let mut store = Store::default();
        let empty = store.pb(&[8, 1], vec![]);
        let raw = store.block(0x55, b"abc".to_vec());
        let root = store.pb(&[8, 1], vec![link("empty", &empty), link("file", &raw)]);
        let events = BTreeMap::from([
            (String::new(), root.clone()),
            ("empty".into(), empty.clone()),
            ("file".into(), raw.clone()),
        ]);
        assert!(check(
            &store,
            &[dir(""), dir("empty"), file("file", b"abc")],
            events.clone()
        )
        .await
        .is_ok());
        assert!(check(
            &store,
            &[dir(""), file("empty", b""), file("file", b"abc")],
            events.clone()
        )
        .await
        .is_err());
        assert!(check(
            &store,
            &[dir(""), dir("empty"), dir("file")],
            events.clone()
        )
        .await
        .is_err());
        let mut wrong_event = events.clone();
        wrong_event.get_mut("file").unwrap().cid = empty.cid;
        assert!(check(
            &store,
            &[dir(""), dir("empty"), file("file", b"abc")],
            wrong_event
        )
        .await
        .is_err());
        let missing = BTreeMap::from([(String::new(), root.clone()), ("file".into(), raw.clone())]);
        assert!(check(&store, &[dir(""), file("file", b"abc")], missing)
            .await
            .is_err());
        let extra = BTreeMap::from([(String::new(), empty.clone()), ("file".into(), raw.clone())]);
        assert!(check(&store, &[dir(""), file("file", b"abc")], extra)
            .await
            .is_err());
        let renamed = BTreeMap::from([
            (String::new(), root),
            ("Empty".into(), empty),
            ("file".into(), raw),
        ]);
        assert!(check(
            &store,
            &[dir(""), dir("Empty"), file("file", b"abc")],
            renamed
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn hashes_file_inline_data_then_chunks_and_checks_chunk_sizes() {
        let mut store = Store::default();
        let chunk = store.block(0x55, b"bc".to_vec());
        // File: inline "a", filesize=3, blocksizes=[2].
        let root = store.pb(&[8, 2, 18, 1, b'a', 24, 3, 32, 2], vec![link("", &chunk)]);
        assert!(check(
            &store,
            &[file("", b"abc")],
            BTreeMap::from([(String::new(), root.clone())])
        )
        .await
        .is_ok());
        assert!(check(
            &store,
            &[file("", b"bca")],
            BTreeMap::from([(String::new(), root)])
        )
        .await
        .is_err());
        // Locally consistent metadata but child actually contains two bytes.
        let bad = store.pb(&[8, 2, 18, 1, b'a', 24, 4, 32, 3], vec![link("", &chunk)]);
        assert!(check(
            &store,
            &[file("", b"abcc")],
            BTreeMap::from([(String::new(), bad)])
        )
        .await
        .is_err());
        store.0.remove(&chunk.cid);
        let missing = store.pb(&[8, 2, 24, 2, 32, 2], vec![link("", &chunk)]);
        assert!(check(
            &store,
            &[file("", b"bc")],
            BTreeMap::from([(String::new(), missing)])
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn rejects_metadata_and_raw_directory_child_tsize_lies() {
        let mut store = Store::default();
        for metadata in [&[8, 1, 56, 0][..], &[8, 1, 66, 2, 8, 0]] {
            let root = store.pb(metadata, vec![]);
            assert!(
                check(&store, &[dir("")], BTreeMap::from([(String::new(), root)]))
                    .await
                    .is_err()
            );
        }
        let raw = store.block(0x55, b"x".to_vec());
        let mut lying_link = link("file", &raw);
        lying_link.size = Some(2);
        let root = store.pb(&[8, 1], vec![lying_link]);
        assert!(check(
            &store,
            &[dir(""), file("file", b"x")],
            BTreeMap::from([(String::new(), root), ("file".into(), raw)])
        )
        .await
        .is_err());
        let metadata_child = store.pb(&[8, 2, 18, 1, b'x', 24, 1, 56, 0], vec![]);
        let clean_parent = store.pb(&[8, 2, 24, 1, 32, 1], vec![link("", &metadata_child)]);
        assert!(check(
            &store,
            &[file("", b"x")],
            BTreeMap::from([(String::new(), clean_parent)])
        )
        .await
        .is_err());
        let metadata_file = store.pb(&[8, 2, 18, 1, b'x', 24, 1, 56, 0], vec![]);
        assert!(check(
            &store,
            &[file("", b"x")],
            BTreeMap::from([(String::new(), metadata_file)])
        )
        .await
        .is_err());
    }
}
