use super::*;
use bytes::Bytes;
use ipld_dagpb::{PbLink, PbNode};
use prost::Message;
use sha2::{Digest, Sha256};
use std::sync::Mutex;

#[derive(Clone, PartialEq, Message)]
struct TestUnixFs {
    #[prost(int32, optional, tag = "1")]
    kind: Option<i32>,
    #[prost(bytes = "bytes", optional, tag = "2")]
    data: Option<Bytes>,
    #[prost(uint64, optional, tag = "3")]
    filesize: Option<u64>,
    #[prost(uint64, repeated, packed = "false", tag = "4")]
    blocksizes: Vec<u64>,
}

#[derive(Default)]
struct Memory {
    blocks: HashMap<Cid, Vec<u8>>,
    reads: Mutex<Vec<Cid>>,
}

#[async_trait]
impl BlockSource for Memory {
    async fn get(&self, cid: &Cid) -> Result<Vec<u8>> {
        self.reads.lock().unwrap().push(*cid);
        self.blocks
            .get(cid)
            .cloned()
            .context("missing fixture block")
    }
}

fn raw(bytes: &[u8]) -> Link {
    Link {
        cid: Cid::new_v1(
            0x55,
            cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
        ),
        size: bytes.len() as u64,
    }
}

impl Memory {
    fn raw_block(&mut self, bytes: &[u8]) -> Link {
        let link = raw(bytes);
        self.blocks.insert(link.cid, bytes.to_vec());
        link
    }

    fn unixfs_node(
        &mut self,
        kind: i32,
        data: &[u8],
        filesize: Option<u64>,
        blocksizes: Vec<u64>,
        children: &[Link],
    ) -> Link {
        let bytes = PbNode {
            links: children
                .iter()
                .map(|child| PbLink {
                    cid: child.cid,
                    name: None,
                    size: Some(child.size),
                })
                .collect(),
            data: Some(Bytes::from(
                TestUnixFs {
                    kind: Some(kind),
                    data: Some(Bytes::copy_from_slice(data)),
                    filesize,
                    blocksizes,
                }
                .encode_to_vec(),
            )),
        }
        .into_bytes();
        let cid = Cid::new_v1(
            0x70,
            cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&bytes)).unwrap(),
        );
        let size = children
            .iter()
            .try_fold(bytes.len() as u64, |sum, child| sum.checked_add(child.size))
            .unwrap();
        self.blocks.insert(cid, bytes);
        Link { cid, size }
    }

    fn file(&mut self, data: &[u8], blocksizes: Vec<u64>, children: &[Link]) -> Link {
        let filesize = blocksizes
            .iter()
            .try_fold(data.len() as u64, |sum, size| sum.checked_add(*size))
            .unwrap();
        self.unixfs_node(2, data, Some(filesize), blocksizes, children)
    }

    fn dagpb_raw(&mut self, data: &[u8]) -> Link {
        self.unixfs_node(0, data, Some(data.len() as u64), vec![], &[])
    }

    fn dir(&mut self, entries: &[(&str, Link)]) -> Link {
        self.dir_with_data(entries, &[8, 1])
    }

    fn dir_with_data(&mut self, entries: &[(&str, Link)], data: &[u8]) -> Link {
        let directory = Directory {
            data: Bytes::copy_from_slice(data),
            entries: entries
                .iter()
                .map(|(name, link)| (name.to_string(), link.clone()))
                .collect(),
        };
        let (cid, bytes, size) = codec::encode(&directory).unwrap();
        self.blocks.insert(cid, bytes);
        Link { cid, size }
    }

    fn result_dir(&self, composition: &Composition, cid: Cid) -> Directory {
        let bytes = composition
            .blocks
            .iter()
            .find(|(c, _)| *c == cid)
            .map(|(_, bytes)| bytes)
            .or_else(|| self.blocks.get(&cid))
            .unwrap()
            .clone();
        match codec::decode(&cid, bytes).unwrap() {
            Node::Directory(directory) => directory,
            _ => panic!("expected directory"),
        }
    }
}

#[tokio::test]
async fn ordered_overlay_semantics_and_structural_sharing() {
    let mut memory = Memory::default();
    let old = raw(b"old");
    let new = raw(b"new");
    let last = raw(b"last");
    let unchanged = memory.dir(&[("keep", old.clone())]);
    let added = memory.dir(&[("new", new.clone())]);
    let nested_base = memory.dir(&[("base", old.clone()), ("overridden", old.clone())]);
    let nested_overlay = memory.dir(&[("overlay", new.clone()), ("overridden", new.clone())]);
    let base = memory.dir(&[
        ("unchanged", unchanged.clone()),
        ("recursive", nested_base),
        ("file-to-directory", old.clone()),
        ("directory-to-file", unchanged.clone()),
        ("override", old.clone()),
    ]);
    let overlay = memory.dir(&[
        ("recursive", nested_overlay),
        ("file-to-directory", added.clone()),
        ("directory-to-file", new.clone()),
        ("override", new.clone()),
        ("added", added.clone()),
    ]);
    let final_layer = memory.dir(&[("override", last.clone())]);
    let composed = compose(&memory, &[base.cid, overlay.cid, final_layer.cid])
        .await
        .unwrap();
    let root = memory.result_dir(&composed, composed.root);
    assert_eq!(root.entries["unchanged"], unchanged);
    assert_eq!(root.entries["file-to-directory"], added);
    assert_eq!(root.entries["added"], added);
    assert_eq!(root.entries["directory-to-file"], new);
    assert_eq!(root.entries["override"], last);
    let nested = memory.result_dir(&composed, root.entries["recursive"].cid);
    assert_eq!(nested.entries["base"], old);
    assert_eq!(nested.entries["overlay"], new);
    assert_eq!(nested.entries["overridden"], new);
    assert_eq!(
        composed.blocks.len(),
        2,
        "only final root and changed recursive child"
    );
    assert!(composed.blocks.iter().all(|(cid, _)| cid.codec() == 0x70));
    assert!(
        memory
            .reads
            .lock()
            .unwrap()
            .iter()
            .all(|cid| cid.codec() == 0x70),
        "raw file payloads must never be read"
    );
}

#[tokio::test]
async fn disjoint_layers_and_exact_utf8_keys() {
    let mut memory = Memory::default();
    let a = raw(b"a");
    let b = raw(b"b");
    let base = memory.dir(&[("é", a.clone()), ("back\\slash", a.clone())]);
    let overlay = memory.dir(&[("e\u{301}", b.clone()), ("x?arg=&y#z%", b.clone())]);
    let composed = compose(&memory, &[base.cid, overlay.cid]).await.unwrap();
    let root = memory.result_dir(&composed, composed.root);
    assert_eq!(root.entries.len(), 4);
    assert_eq!(root.entries["é"], a);
    assert_eq!(root.entries["e\u{301}"], b);
}

#[tokio::test]
async fn no_op_and_wholesale_overlay_reuse_original_cids() {
    let mut memory = Memory::default();
    let a = raw(b"a");
    let b = raw(b"b");
    let base = memory.dir(&[("a", a.clone())]);
    let empty = memory.dir(&[]);
    for layers in [
        vec![base.cid],
        vec![base.cid, base.cid],
        vec![base.cid, empty.cid],
    ] {
        let result = compose(&memory, &layers).await.unwrap();
        assert_eq!(result.root, base.cid);
        assert!(result.blocks.is_empty());
    }
    let overlay = memory.dir(&[("a", b)]);
    let result = compose(&memory, &[base.cid, overlay.cid]).await.unwrap();
    assert_eq!(result.root, overlay.cid);
    assert!(result.blocks.is_empty());
}

#[tokio::test]
async fn profile_determinism_and_insertion_order() {
    assert_eq!(PROFILE, "wetware-composer-v1");
    let mut memory = Memory::default();
    let a = raw(b"a");
    let b = raw(b"b");
    let one = memory.dir(&[("a", a.clone()), ("b", b.clone())]);
    let two = memory.dir(&[("b", b), ("a", a)]);
    assert_eq!(one, two);
    let overlay = memory.dir(&[("c", raw(b"c"))]);
    let expected = compose(&memory, &[one.cid, overlay.cid]).await.unwrap();
    assert_eq!(
        expected.root.to_string(),
        "bafybeido7bhqc3aydbft6xciw2ayu7d4y6sjvirmz3x426gtd4iuebu2qu",
        "Composer v1 identity fixture"
    );
    for _ in 0..8 {
        let actual = compose(&memory, &[two.cid, overlay.cid]).await.unwrap();
        assert_eq!(actual.root, expected.root);
        assert_eq!(actual.blocks, expected.blocks);
    }
}

#[tokio::test]
async fn recursive_merge_uses_overlay_metadata_bytes() {
    let mut memory = Memory::default();
    // UnixFS Directory, mode 0755; overlay mode 0700.
    let base = memory.dir_with_data(&[("a", raw(b"a"))], &[8, 1, 56, 0xed, 3]);
    let overlay = memory.dir_with_data(&[("b", raw(b"b"))], &[8, 1, 56, 0xc0, 3]);
    let composed = compose(&memory, &[base.cid, overlay.cid]).await.unwrap();
    assert_eq!(
        memory.result_dir(&composed, composed.root).data.as_ref(),
        &[8, 1, 56, 0xc0, 3]
    );
    let directory = memory.result_dir(&composed, composed.root);
    assert_eq!(directory.entries["a"], raw(b"a"));
    assert_eq!(directory.entries["b"], raw(b"b"));
    assert_eq!(composed.blocks.len(), 1);
}

#[tokio::test]
async fn root_metadata_only_changes_replace_fields_wholesale() {
    // Directory, mode=0755, mtime.seconds=7.
    const BASE_DATA: &[u8] = &[8, 1, 56, 0xed, 3, 66, 2, 8, 7];
    for overlay_data in [
        &[8, 1, 56, 0xc0, 3, 66, 2, 8, 7][..], // mode=0700
        &[8, 1, 56, 0xed, 3, 66, 2, 8, 9][..], // mtime.seconds=9
        &[8, 1, 56, 0xc0, 3][..],              // omit mtime
        &[8, 1, 66, 2, 8, 9][..],              // omit mode
        &[8, 1][..],                           // omit both
    ] {
        let mut memory = Memory::default();
        let child = raw(b"unchanged");
        let base = memory.dir_with_data(&[("file", child.clone())], BASE_DATA);
        let overlay = memory.dir_with_data(&[("file", child.clone())], overlay_data);
        assert_ne!(base.cid, overlay.cid);
        for _ in 0..3 {
            let result = compose(&memory, &[base.cid, overlay.cid]).await.unwrap();
            assert_eq!(
                result.root, overlay.cid,
                "overlay metadata {overlay_data:?}"
            );
            assert!(
                result.blocks.is_empty(),
                "reuse the already encoded overlay"
            );
            let root = memory.result_dir(&result, result.root);
            assert_eq!(root.data.as_ref(), overlay_data);
            assert_eq!(root.entries["file"], child);
        }
    }
}

#[tokio::test]
async fn nested_metadata_only_change_encodes_changed_directory_and_ancestors() {
    const BASE_DATA: &[u8] = &[8, 1, 56, 0xed, 3, 66, 2, 8, 7];
    for overlay_data in [&[8, 1, 56, 0xc0, 3, 66, 2, 8, 9][..], &[8, 1][..]] {
        let mut memory = Memory::default();
        let child = raw(b"unchanged");
        let untouched = memory.dir(&[("file", child.clone())]);
        let base_etc = memory.dir_with_data(&[("a", child.clone())], BASE_DATA);
        let overlay_etc = memory.dir_with_data(&[], overlay_data);
        let base = memory.dir(&[("etc", base_etc.clone()), ("untouched", untouched.clone())]);
        let overlay = memory.dir(&[("etc", overlay_etc.clone())]);
        let result = compose(&memory, &[base.cid, overlay.cid]).await.unwrap();
        let root = memory.result_dir(&result, result.root);
        let etc = memory.result_dir(&result, root.entries["etc"].cid);
        assert_eq!(etc.data.as_ref(), overlay_data);
        assert_eq!(etc.entries["a"], child);
        assert_eq!(root.entries["untouched"], untouched);
        assert_ne!(root.entries["etc"].cid, base_etc.cid);
        assert_ne!(root.entries["etc"].cid, overlay_etc.cid);
        let generated: HashSet<_> = result.blocks.iter().map(|(cid, _)| *cid).collect();
        assert_eq!(
            generated,
            HashSet::from([result.root, root.entries["etc"].cid])
        );
        let repeated = compose(&memory, &[base.cid, overlay.cid]).await.unwrap();
        assert_eq!(repeated.root, result.root);
        assert_eq!(repeated.blocks, result.blocks);
    }
}

#[tokio::test]
async fn semantically_equal_directories_reuse_cidv0_base_before_cidv1_overlay() {
    let mut memory = Memory::default();
    let overlay = memory.dir(&[("file", raw(b"same"))]);
    let base = Cid::new_v0(*overlay.cid.hash()).unwrap();
    memory
        .blocks
        .insert(base, memory.blocks[&overlay.cid].clone());
    for _ in 0..3 {
        let result = compose(&memory, &[base, overlay.cid]).await.unwrap();
        assert_eq!(result.root, base);
        assert!(result.blocks.is_empty());
    }
}

#[tokio::test]
async fn semantically_equal_directories_reuse_data_first_base_before_links_first_overlay() {
    let mut memory = Memory::default();
    let overlay = memory.dir(&[("file", raw(b"same"))]);
    let links_first = &memory.blocks[&overlay.cid];
    assert!(links_first.ends_with(&[10, 2, 8, 1]));
    // Move the hand-checked Directory Data field before the Links section.
    let mut data_first = vec![10, 2, 8, 1];
    data_first.extend_from_slice(&links_first[..links_first.len() - 4]);
    let base = Cid::new_v1(
        0x70,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&data_first)).unwrap(),
    );
    assert_ne!(base, overlay.cid);
    memory.blocks.insert(base, data_first);
    for _ in 0..3 {
        let result = compose(&memory, &[base, overlay.cid]).await.unwrap();
        assert_eq!(result.root, base);
        assert!(result.blocks.is_empty());
    }
}

#[tokio::test]
async fn rejects_missing_blocks_wrong_sizes_and_nondirectory_roots() {
    let mut memory = Memory::default();
    assert!(compose(&memory, &[raw(b"file").cid]).await.is_err());
    let dir = memory.dir(&[]);
    let wrong = Link {
        size: dir.size + 1,
        ..dir
    };
    let parent = memory.dir(&[("bad", wrong)]);
    assert!(compose(&memory, &[parent.cid])
        .await
        .unwrap_err()
        .to_string()
        .contains("Tsize"));
    memory.blocks.remove(&dir.cid);
    assert!(compose(&memory, &[dir.cid]).await.is_err());
}

#[tokio::test]
async fn rejects_directory_as_file_child() {
    let mut memory = Memory::default();
    let directory = memory.dir(&[]);
    let file = memory.file(&[], vec![0], &[directory]);
    let root = memory.dir(&[("file", file)]);

    let error = compose(&memory, &[root.cid]).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("file child"),
        "unexpected error: {error:#}"
    );
}

#[tokio::test]
async fn rejects_file_blocksize_overstatement_and_understatement() {
    for advertised in [0, 7] {
        let mut memory = Memory::default();
        let child = memory.dagpb_raw(b"x");
        let file = memory.file(&[], vec![advertised], &[child]);
        let root = memory.dir(&[("file", file)]);

        let error = compose(&memory, &[root.cid]).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("blocksize"),
            "advertised {advertised}: {error:#}"
        );
    }
}

#[tokio::test]
async fn accepts_recursive_file_and_computes_logical_sizes() {
    let mut memory = Memory::default();
    let raw_codec = memory.raw_block(b"abc");
    let legacy_raw = memory.dagpb_raw(b"de");
    let nested = memory.file(b"f", vec![3, 2], &[raw_codec, legacy_raw]);
    let file = memory.file(b"gh", vec![6], &[nested]);
    let root = memory.dir(&[("file", file)]);

    let result = compose(&memory, &[root.cid]).await.unwrap();
    assert_eq!(result.root, root.cid);
    assert!(result.blocks.is_empty());
}

#[tokio::test]
async fn validates_shared_file_descendant_once() {
    let mut memory = Memory::default();
    let chunk = memory.raw_block(b"shared");
    let shared = memory.file(&[], vec![6], std::slice::from_ref(&chunk));
    let file = memory.file(&[], vec![6, 6], &[shared.clone(), shared.clone()]);
    let root = memory.dir(&[("file", file)]);

    compose(&memory, &[root.cid]).await.unwrap();
    let reads = memory.reads.lock().unwrap();
    assert_eq!(reads.iter().filter(|cid| **cid == shared.cid).count(), 1);
    assert_eq!(reads.iter().filter(|cid| **cid == chunk.cid).count(), 1);
}

#[tokio::test]
async fn cached_file_height_cannot_bypass_depth_limit() {
    let mut memory = Memory::default();
    let mut file = memory.raw_block(b"x");
    for _ in 0..MAX_DEPTH - 1 {
        file = memory.file(&[], vec![1], &[file]);
    }
    let shallow = memory.dir(&[("file", file.clone())]);
    let nested = memory.dir(&[("file", file)]);
    let deep = memory.dir(&[("nested", nested)]);

    let error = compose(&memory, &[shallow.cid, deep.cid])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("depth limit"), "{error:#}");
}

fn limits(
    max_nodes: usize,
    max_read_bytes: usize,
    max_written_bytes: usize,
    max_work: usize,
) -> Limits {
    Limits {
        max_nodes,
        max_read_bytes,
        max_written_bytes,
        max_work,
    }
}

#[tokio::test]
async fn node_limit_accepts_boundary_and_rejects_one_beyond() {
    let mut memory = Memory::default();
    let root = memory.dir(&[("a", raw(b"a")), ("b", raw(b"b"))]);

    compose_with_limits(
        &memory,
        &[root.cid],
        limits(3, usize::MAX, usize::MAX, usize::MAX),
    )
    .await
    .unwrap();
    let error = compose_with_limits(
        &memory,
        &[root.cid],
        limits(2, usize::MAX, usize::MAX, usize::MAX),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("node limit"), "{error:#}");
}

#[tokio::test]
async fn generated_nodes_count_toward_the_node_limit() {
    let mut memory = Memory::default();
    let base = memory.dir(&[("base", raw(b"base"))]);
    let overlay = memory.dir(&[("overlay", raw(b"overlay"))]);

    let result = compose_with_limits(
        &memory,
        &[base.cid, overlay.cid],
        limits(5, usize::MAX, usize::MAX, usize::MAX),
    )
    .await
    .unwrap();
    assert_eq!(result.blocks.len(), 1);

    let error = compose_with_limits(
        &memory,
        &[base.cid, overlay.cid],
        limits(4, usize::MAX, usize::MAX, usize::MAX),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("node limit"), "{error:#}");
}

#[tokio::test]
async fn input_byte_limit_accepts_boundary_and_rejects_one_beyond() {
    let mut memory = Memory::default();
    let child = memory.dir(&[]);
    let root = memory.dir(&[("child", child.clone())]);
    let bytes = memory.blocks[&root.cid].len() + memory.blocks[&child.cid].len();

    compose_with_limits(
        &memory,
        &[root.cid],
        limits(usize::MAX, bytes, usize::MAX, usize::MAX),
    )
    .await
    .unwrap();
    let error = compose_with_limits(
        &memory,
        &[root.cid],
        limits(usize::MAX, bytes - 1, usize::MAX, usize::MAX),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("input byte limit"), "{error:#}");
}

#[tokio::test]
async fn generated_byte_limit_accepts_boundary_and_rejects_one_beyond() {
    let mut memory = Memory::default();
    let base_child = memory.dir(&[("a", raw(b"a"))]);
    let overlay_child = memory.dir(&[("b", raw(b"b"))]);
    let base = memory.dir(&[("nested", base_child)]);
    let overlay = memory.dir(&[("nested", overlay_child)]);
    let expected = compose(&memory, &[base.cid, overlay.cid]).await.unwrap();
    assert_eq!(expected.blocks.len(), 2);
    let bytes: usize = expected.blocks.iter().map(|(_, block)| block.len()).sum();

    compose_with_limits(
        &memory,
        &[base.cid, overlay.cid],
        limits(usize::MAX, usize::MAX, bytes, usize::MAX),
    )
    .await
    .unwrap();
    let error = compose_with_limits(
        &memory,
        &[base.cid, overlay.cid],
        limits(usize::MAX, usize::MAX, bytes - 1, usize::MAX),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("output byte limit"), "{error:#}");
}

#[tokio::test]
async fn discarded_intermediate_blocks_count_toward_the_generated_byte_limit() {
    let mut memory = Memory::default();
    let base = memory.dir(&[("base", raw(b"base"))]);
    let overlay = memory.dir(&[("overlay", raw(b"overlay"))]);
    let final_layer = memory.dir(&[("final", raw(b"final"))]);

    let intermediate = compose(&memory, &[base.cid, overlay.cid]).await.unwrap();
    let complete = compose(&memory, &[base.cid, overlay.cid, final_layer.cid])
        .await
        .unwrap();
    assert_eq!(intermediate.blocks.len(), 1);
    assert_eq!(complete.blocks.len(), 1);
    assert_ne!(intermediate.root, complete.root);
    assert!(
        complete
            .blocks
            .iter()
            .all(|(cid, _)| *cid != intermediate.root),
        "the first merge result must be discarded from the final block set"
    );
    let charged_bytes = intermediate.blocks[0].1.len() + complete.blocks[0].1.len();

    compose_with_limits(
        &memory,
        &[base.cid, overlay.cid, final_layer.cid],
        limits(usize::MAX, usize::MAX, charged_bytes, usize::MAX),
    )
    .await
    .unwrap();
    let error = compose_with_limits(
        &memory,
        &[base.cid, overlay.cid, final_layer.cid],
        limits(usize::MAX, usize::MAX, charged_bytes - 1, usize::MAX),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("output byte limit"), "{error:#}");
}

#[tokio::test]
async fn work_limit_accepts_boundary_and_rejects_one_beyond() {
    let mut memory = Memory::default();
    let root = memory.dir(&[("file", raw(b"x"))]);

    compose_with_limits(
        &memory,
        &[root.cid],
        limits(usize::MAX, usize::MAX, usize::MAX, 2),
    )
    .await
    .unwrap();
    let error = compose_with_limits(
        &memory,
        &[root.cid],
        limits(usize::MAX, usize::MAX, usize::MAX, 1),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("work limit"), "{error:#}");
}

#[tokio::test]
async fn shared_recursive_merges_preserve_aggregate_accounting() {
    let mut memory = Memory::default();
    let base_child = memory.dir(&[("base", raw(b"base"))]);
    let overlay_child = memory.dir(&[("overlay", raw(b"overlay"))]);
    let base_child_cid = base_child.cid;
    let overlay_child_cid = overlay_child.cid;
    let base = memory.dir(&[("left", base_child.clone()), ("right", base_child)]);
    let overlay = memory.dir(&[("left", overlay_child.clone()), ("right", overlay_child)]);

    let input_directories = [base.cid, base_child_cid, overlay.cid, overlay_child_cid];
    let read_bytes = input_directories
        .iter()
        .map(|cid| memory.blocks[cid].len())
        .sum();
    let result = compose_with_limits(
        &memory,
        &[base.cid, overlay.cid],
        limits(usize::MAX, read_bytes, usize::MAX, usize::MAX),
    )
    .await
    .unwrap();
    assert_eq!(result.blocks.len(), 2);
    {
        let reads = memory.reads.lock().unwrap();
        for cid in input_directories {
            assert_eq!(
                reads.iter().filter(|read| **read == cid).count(),
                1,
                "shared input directory {cid} must be charged once"
            );
        }
    }

    let error = compose_with_limits(
        &memory,
        &[base.cid, overlay.cid],
        limits(usize::MAX, read_bytes - 1, usize::MAX, usize::MAX),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("input byte limit"), "{error:#}");

    compose_with_limits(
        &memory,
        &[base.cid, overlay.cid],
        limits(8, usize::MAX, usize::MAX, usize::MAX),
    )
    .await
    .unwrap();
    let error = compose_with_limits(
        &memory,
        &[base.cid, overlay.cid],
        limits(7, usize::MAX, usize::MAX, usize::MAX),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("node limit"), "{error:#}");

    let result = compose_with_limits(
        &memory,
        &[base.cid, overlay.cid],
        limits(usize::MAX, usize::MAX, usize::MAX, 18),
    )
    .await
    .unwrap();
    assert_eq!(result.blocks.len(), 2);

    let error = compose_with_limits(
        &memory,
        &[base.cid, overlay.cid],
        limits(usize::MAX, usize::MAX, usize::MAX, 17),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("work limit"), "{error:#}");
}

#[tokio::test]
async fn rejects_layer_and_depth_bounds_even_in_reused_subtrees() {
    let mut memory = Memory::default();
    let mut root = memory.dir(&[]);
    compose(&memory, &vec![root.cid; MAX_LAYERS]).await.unwrap();
    assert!(compose(&memory, &vec![root.cid; MAX_LAYERS + 1])
        .await
        .is_err());
    for _ in 0..MAX_DEPTH {
        root = memory.dir(&[("d", root)]);
    }
    compose(&memory, &[root.cid]).await.unwrap();
    let parent = memory.dir(&[("d", root.clone())]);
    assert!(
        compose(&memory, &[root.cid, parent.cid])
            .await
            .unwrap_err()
            .to_string()
            .contains("depth"),
        "cached subtree height must count at the new depth"
    );
    root = parent;
    assert!(compose(&memory, &[root.cid])
        .await
        .unwrap_err()
        .to_string()
        .contains("depth"));
}

#[tokio::test]
async fn rejects_actual_kubo_hamt_even_below_wholesale_reused_directory() {
    let mut memory = Memory::default();
    let bytes = include_bytes!("../../tests/fixtures/composer/hamt-root.dagpb").to_vec();
    let cid = Cid::new_v1(
        0x70,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&bytes)).unwrap(),
    );
    memory.blocks.insert(cid, bytes);
    let root = memory.dir(&[("sharded", Link { cid, size: 1 })]);
    for layers in [vec![cid], vec![root.cid], vec![root.cid, root.cid]] {
        assert!(format!("{:#}", compose(&memory, &layers).await.unwrap_err()).contains("HAMT"));
    }
}

#[tokio::test]
async fn cancellation_can_interrupt_block_read_without_writes() {
    struct Pending(Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
    #[async_trait]
    impl BlockSource for Pending {
        async fn get(&self, _: &Cid) -> Result<Vec<u8>> {
            self.0.lock().unwrap().take().unwrap().send(()).unwrap();
            std::future::pending().await
        }
    }
    let mut memory = Memory::default();
    let root = memory.dir(&[]);
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let source = Pending(Mutex::new(Some(started_tx)));
    let layers = [root.cid];
    let pending = super::super::await_or_cancel(&mut rx, compose(&source, &layers));
    tokio::pin!(pending);
    tokio::select! {
        _ = &mut pending => panic!("read must remain pending"),
        started = started_rx => started.unwrap(),
    }
    tx.send(true).unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn same_raw_cid_uses_later_advertised_size() {
    let mut memory = Memory::default();
    let raw = raw(b"a");
    let base = memory.dir(&[("file", raw.clone())]);
    let overlay = memory.dir(&[(
        "file",
        Link {
            size: raw.size + 1,
            ..raw
        },
    )]);
    let result = compose(&memory, &[base.cid, overlay.cid]).await.unwrap();
    assert_eq!(result.root, overlay.cid);
    assert!(result.blocks.is_empty());
}

async fn frozen_mount(memory: &Memory, source: &Link, target: &str) -> FrozenMount {
    FrozenMount::new(
        inspect_source(memory, source.cid).await.unwrap(),
        GuestTarget::new(target).unwrap(),
    )
    .unwrap()
}

fn deterministic_error(error: &anyhow::Error) -> &CompositionError {
    error
        .downcast_ref::<CompositionError>()
        .unwrap_or_else(|| panic!("expected typed deterministic error: {error:#}"))
}

#[test]
fn guest_targets_validate_lexically_without_normalizing_names() {
    assert!(GuestTarget::root().components().is_empty());
    assert_eq!(GuestTarget::new("/").unwrap(), GuestTarget::root());
    for invalid in [
        "", "relative", "//", "/a/", "/a//b", "/.", "/..", "/a/./b", "/a/../b",
    ] {
        let error = GuestTarget::new(invalid).unwrap_err();
        assert!(
            matches!(
                deterministic_error(&error),
                CompositionError::InvalidTarget(_)
            ),
            "{invalid:?}"
        );
    }
    for byte in (0u8..=31).chain(std::iter::once(127)) {
        let target = format!("/a{}b", char::from(byte));
        let error = GuestTarget::new(&target).unwrap_err();
        assert!(matches!(
            deterministic_error(&error),
            CompositionError::InvalidTarget(_)
        ));
    }
    let target = GuestTarget::new("/ a\\b : /é/e\u{301}/\u{85}\u{200d}").unwrap();
    assert_eq!(
        target.components(),
        &[" a\\b : ", "é", "e\u{301}", "\u{85}\u{200d}"]
    );
    assert_ne!(
        GuestTarget::new("/é").unwrap(),
        GuestTarget::new("/e\u{301}").unwrap()
    );
}

#[test]
fn guest_target_byte_and_depth_limits_are_inclusive() {
    assert!(GuestTarget::new(&format!("/{}", "x".repeat(4095))).is_ok());
    assert!(GuestTarget::new(&format!("/{}", "x".repeat(4096))).is_err());
    assert!(GuestTarget::new(&format!("/{}x", "é".repeat(2047))).is_ok());
    assert!(GuestTarget::new(&format!("/{}", "é".repeat(2048))).is_err());
    assert_eq!(
        GuestTarget::new(&"/x".repeat(MAX_DEPTH))
            .unwrap()
            .components()
            .len(),
        MAX_DEPTH
    );
    assert!(GuestTarget::new(&"/x".repeat(MAX_DEPTH + 1)).is_err());
}

#[tokio::test]
async fn standalone_sources_report_verified_dag_sizes_and_exact_target_cids() {
    let mut memory = Memory::default();
    let raw = memory.raw_block(b"raw");
    let legacy = memory.dagpb_raw(b"legacy");
    let inline = memory.file(b"inline", vec![], &[]);
    let chunked = memory.file(b"prefix", vec![3, 6], &[raw.clone(), legacy.clone()]);
    let empty = memory.dir(&[]);
    let directory = memory.dir(&[("child", inline.clone())]);
    for (link, kind) in [
        (raw, SourceKind::File),
        (legacy, SourceKind::File),
        (inline, SourceKind::File),
        (chunked, SourceKind::File),
        (empty, SourceKind::Directory),
        (directory, SourceKind::Directory),
    ] {
        let frozen = inspect_source(&memory, link.cid).await.unwrap();
        assert_eq!(frozen.cid(), link.cid);
        assert_eq!(frozen.kind(), kind);
        assert_eq!(frozen.dag_size(), link.size);
        let operation = FrozenMount::new(frozen, GuestTarget::new("/exact").unwrap()).unwrap();
        let result = compose_mounts(&memory, None, &[operation]).await.unwrap();
        let root = memory.result_dir(&result, result.root);
        assert_eq!(root.entries.len(), 1);
        assert_eq!(root.entries["exact"], link);
    }
}

#[tokio::test]
async fn root_mount_requires_directory_and_empty_operations_require_head() {
    let mut memory = Memory::default();
    let file = memory.raw_block(b"file");
    let source = inspect_source(&memory, file.cid).await.unwrap();
    let error = FrozenMount::new(source, GuestTarget::root()).unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::InvalidSource(_)
    ));
    let error = compose_mounts(&memory, None, &[]).await.unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::EmptyOperations
    ));
    let root = memory.dir(&[]);
    let result = compose_mounts(&memory, Some(root.cid), &[]).await.unwrap();
    assert_eq!(result.root, root.cid);
    assert!(result.blocks.is_empty());
}

#[tokio::test]
async fn standalone_validation_distinguishes_invalid_content_profile_and_transport() {
    let mut memory = Memory::default();
    let raw = memory.raw_block(b"correct");
    memory.blocks.insert(raw.cid, b"corrupt".to_vec());
    let error = inspect_source(&memory, raw.cid).await.unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::InvalidDag(_)
    ));
    memory.blocks.remove(&raw.cid);
    let error = inspect_source(&memory, raw.cid).await.unwrap_err();
    assert!(
        error.downcast_ref::<CompositionError>().is_none(),
        "transport failure became deterministic: {error:#}"
    );
    let unsupported = Cid::new_v1(0x71, *raw.cid.hash());
    let error = inspect_source(&memory, unsupported).await.unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::UnsupportedProfile(_)
    ));
    let unsupported_hash = Cid::new_v1(
        0x55,
        cid::multihash::Multihash::wrap(0, b"identity").unwrap(),
    );
    let error = inspect_source(&memory, unsupported_hash).await.unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::UnsupportedProfile(_)
    ));
    let bytes = vec![0xff];
    let malformed = Cid::new_v1(
        0x70,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&bytes)).unwrap(),
    );
    memory.blocks.insert(malformed, bytes);
    let error = inspect_source(&memory, malformed).await.unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::InvalidDag(_)
    ));
}

#[tokio::test]
async fn standalone_file_validation_rejects_incorrect_chunk_sizes_and_types() {
    for bad_blocksize in [0, 2] {
        let mut memory = Memory::default();
        let chunk = memory.raw_block(b"x");
        let file = memory.file(&[], vec![bad_blocksize], &[chunk]);
        let error = inspect_source(&memory, file.cid).await.unwrap_err();
        assert!(matches!(
            deterministic_error(&error),
            CompositionError::InvalidDag(_)
        ));
    }
    let mut memory = Memory::default();
    let chunk = memory.raw_block(b"x");
    let wrong = Link {
        size: chunk.size + 1,
        ..chunk
    };
    let file = memory.file(&[], vec![1], &[wrong]);
    let error = inspect_source(&memory, file.cid).await.unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::InvalidDag(_)
    ));
    let directory = memory.dir(&[]);
    let file = memory.file(&[], vec![0], &[directory]);
    let error = inspect_source(&memory, file.cid).await.unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::InvalidDag(_)
    ));
}

#[tokio::test]
async fn targeted_edit_preserves_every_strict_ancestor_metadata_and_unrelated_link() {
    const ROOT: &[u8] = &[8, 1, 56, 0xed, 3, 66, 2, 8, 1];
    const ETC: &[u8] = &[8, 1, 56, 0xc0, 3, 66, 2, 8, 2];
    const APP: &[u8] = &[8, 1, 56, 0x80, 3, 66, 2, 8, 3];
    let mut memory = Memory::default();
    let old = raw(b"old");
    let app = memory.dir_with_data(&[("config", old.clone()), ("keep", old.clone())], APP);
    let etc = memory.dir_with_data(&[("app", app), ("keep", old.clone())], ETC);
    let root = memory.dir_with_data(&[("etc", etc), ("keep", old.clone())], ROOT);
    let new = memory.raw_block(b"new");
    let mount = frozen_mount(&memory, &new, "/etc/app/config").await;
    let result = compose_mounts(&memory, Some(root.cid), &[mount])
        .await
        .unwrap();
    let root = memory.result_dir(&result, result.root);
    let etc = memory.result_dir(&result, root.entries["etc"].cid);
    let app = memory.result_dir(&result, etc.entries["app"].cid);
    for (directory, expected) in [(&root, ROOT), (&etc, ETC), (&app, APP)] {
        assert_eq!(directory.data.as_ref(), expected);
        assert_eq!(directory.entries["keep"], old);
    }
    assert_eq!(app.entries["config"], new);
    assert_eq!(result.blocks.len(), 3);
}

#[tokio::test]
async fn targeted_directory_metadata_omissions_are_local_to_collision() {
    const DATA: &[u8] = &[8, 1, 56, 0xed, 3, 66, 2, 8, 7];
    let mut memory = Memory::default();
    let child = raw(b"keep");
    let app = memory.dir_with_data(&[("keep", child.clone())], DATA);
    let root = memory.dir_with_data(&[("app", app)], DATA);
    let overlay = memory.dir(&[]);
    let mount = frozen_mount(&memory, &overlay, "/app").await;
    let result = compose_mounts(&memory, Some(root.cid), &[mount])
        .await
        .unwrap();
    let root = memory.result_dir(&result, result.root);
    let app = memory.result_dir(&result, root.entries["app"].cid);
    assert_eq!(root.data.as_ref(), DATA);
    assert_eq!(app.data.as_ref(), &[8, 1]);
    assert_eq!(app.entries["keep"], child);
}

#[tokio::test]
async fn target_only_missing_ancestors_are_canonical_empty_metadata() {
    let mut memory = Memory::default();
    let file = memory.raw_block(b"config");
    let mount = frozen_mount(&memory, &file, "/etc/app/config").await;
    let result = compose_mounts(&memory, None, &[mount]).await.unwrap();
    let root = memory.result_dir(&result, result.root);
    let etc = memory.result_dir(&result, root.entries["etc"].cid);
    let app = memory.result_dir(&result, etc.entries["app"].cid);
    for directory in [&root, &etc, &app] {
        assert_eq!(directory.data.as_ref(), &[8, 1]);
        assert_eq!(directory.entries.len(), 1);
    }
    assert_eq!(app.entries["config"], file);
    assert_eq!(
        result.blocks.len(),
        3,
        "discard unreachable empty seed and intermediate roots"
    );
}

#[tokio::test]
async fn root_and_target_operations_keep_original_interleaving() {
    let mut memory = Memory::default();
    let a = memory.raw_block(b"a");
    let b = memory.raw_block(b"b");
    let c = memory.raw_block(b"c");
    let root_a = memory.dir(&[("value", a.clone()), ("keep", a)]);
    let root_c = memory.dir(&[("value", c.clone())]);
    let root_a = frozen_mount(&memory, &root_a, "/").await;
    let target_b = frozen_mount(&memory, &b, "/value").await;
    let root_c = frozen_mount(&memory, &root_c, "/").await;
    for (mounts, expected) in [
        (
            vec![root_a.clone(), target_b.clone(), root_c.clone()],
            c.clone(),
        ),
        (
            vec![target_b.clone(), root_c.clone(), target_b.clone()],
            b.clone(),
        ),
        (vec![root_a, root_c, target_b], b),
    ] {
        let result = compose_mounts(&memory, None, &mounts).await.unwrap();
        assert_eq!(
            memory.result_dir(&result, result.root).entries["value"],
            expected
        );
    }
}

#[tokio::test]
async fn exact_targets_replace_files_directories_and_symlinks_without_following() {
    let mut memory = Memory::default();
    let file = memory.raw_block(b"new");
    let old = memory.raw_block(b"old");
    let directory = memory.dir(&[("child", old.clone())]);
    let symlink = memory.unixfs_node(4, b"/elsewhere", None, vec![], &[]);
    let error = inspect_source(&memory, symlink.cid).await.unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::InvalidSource(_)
    ));
    for (base, replacement) in [
        (old.clone(), file.clone()),
        (directory.clone(), file.clone()),
        (symlink.clone(), file),
        (old, directory.clone()),
        (symlink, directory),
    ] {
        let root = memory.dir(&[("exact", base)]);
        let mount = frozen_mount(&memory, &replacement, "/exact").await;
        let result = compose_mounts(&memory, Some(root.cid), &[mount])
            .await
            .unwrap();
        assert_eq!(
            memory.result_dir(&result, result.root).entries["exact"],
            replacement
        );
    }
}

#[tokio::test]
async fn strict_ancestor_obstruction_is_typed_and_can_be_explicitly_replaced() {
    let mut memory = Memory::default();
    let file = memory.raw_block(b"file");
    let symlink = memory.unixfs_node(4, b"/other", None, vec![], &[]);
    let directory = memory.dir(&[]);
    let child_mount = frozen_mount(&memory, &file, "/a/b").await;
    let replace = frozen_mount(&memory, &directory, "/a").await;
    for obstruction in [file.clone(), symlink] {
        let root = memory.dir(&[("a", obstruction)]);
        let error = compose_mounts(&memory, Some(root.cid), std::slice::from_ref(&child_mount))
            .await
            .unwrap_err();
        match deterministic_error(&error) {
            CompositionError::TargetObstruction { target, ancestor } => {
                assert_eq!(target, &GuestTarget::new("/a/b").unwrap());
                assert_eq!(ancestor, &GuestTarget::new("/a").unwrap());
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let result = compose_mounts(
            &memory,
            Some(root.cid),
            &[replace.clone(), child_mount.clone()],
        )
        .await
        .unwrap();
        let root = memory.result_dir(&result, result.root);
        assert_eq!(
            memory.result_dir(&result, root.entries["a"].cid).entries["b"],
            file
        );
    }
}

#[tokio::test]
async fn parent_child_order_merges_directories_and_later_files_remove_subtrees() {
    let mut memory = Memory::default();
    let file = memory.raw_block(b"child");
    let other = memory.raw_block(b"other");
    let directory = memory.dir(&[("other", other.clone())]);
    let child = frozen_mount(&memory, &file, "/a/b").await;
    let parent = frozen_mount(&memory, &directory, "/a").await;
    for mounts in [
        vec![child.clone(), parent.clone()],
        vec![parent, child.clone()],
    ] {
        let result = compose_mounts(&memory, None, &mounts).await.unwrap();
        let root = memory.result_dir(&result, result.root);
        let a = memory.result_dir(&result, root.entries["a"].cid);
        assert_eq!(a.entries["b"], file);
        assert_eq!(a.entries["other"], other);
    }
    let parent_file = frozen_mount(&memory, &other, "/a").await;
    let result = compose_mounts(&memory, None, &[child.clone(), parent_file.clone()])
        .await
        .unwrap();
    assert_eq!(memory.result_dir(&result, result.root).entries["a"], other);
    assert_eq!(
        result.blocks.len(),
        1,
        "removed child subtree is unreachable"
    );
    let error = compose_mounts(&memory, None, &[parent_file, child])
        .await
        .unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::TargetObstruction { .. }
    ));
}

#[tokio::test]
async fn targeted_directory_identity_prefers_full_base_cid_and_root_seed_is_unchanged() {
    let mut memory = Memory::default();
    let overlay = memory.dir(&[("child", raw(b"same"))]);
    let v0 = Cid::new_v0(*overlay.cid.hash()).unwrap();
    memory
        .blocks
        .insert(v0, memory.blocks[&overlay.cid].clone());
    let base = Link {
        cid: v0,
        size: overlay.size,
    };
    let root = memory.dir(&[("a", base.clone())]);
    let mount = frozen_mount(&memory, &overlay, "/a").await;
    let result = compose_mounts(&memory, Some(root.cid), &[mount])
        .await
        .unwrap();
    assert_eq!(result.root, root.cid);
    assert!(result.blocks.is_empty());
    let mount = frozen_mount(&memory, &base, "/").await;
    let result = compose_mounts(&memory, None, &[mount]).await.unwrap();
    assert_eq!(result.root, v0);
    assert!(result.blocks.is_empty());
}

#[tokio::test]
async fn targeted_source_height_includes_file_chunks_even_after_cached_shallow_placement() {
    let mut memory = Memory::default();
    let mut file = memory.raw_block(b"x");
    for _ in 0..MAX_DEPTH - 1 {
        file = memory.file(&[], vec![1], &[file]);
    }
    let shallow = frozen_mount(&memory, &file, "/a").await;
    compose_mounts(&memory, None, std::slice::from_ref(&shallow))
        .await
        .unwrap();
    let deep = frozen_mount(&memory, &file, "/a/b").await;
    let error = compose_mounts(&memory, None, &[shallow, deep])
        .await
        .unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::ResourceLimit(_)
    ));
}

#[tokio::test]
async fn target_operation_limit_counts_authoritative_head_but_not_empty_seed() {
    let mut memory = Memory::default();
    let file = memory.raw_block(b"x");
    let mount = frozen_mount(&memory, &file, "/a").await;
    let root = memory.dir(&[]);
    compose_mounts(&memory, None, &vec![mount.clone(); MAX_LAYERS])
        .await
        .unwrap();
    compose_mounts(
        &memory,
        Some(root.cid),
        &vec![mount.clone(); MAX_LAYERS - 1],
    )
    .await
    .unwrap();
    for head in [None, Some(root.cid)] {
        let count = MAX_LAYERS + usize::from(head.is_none());
        let error = compose_mounts(&memory, head, &vec![mount.clone(); count])
            .await
            .unwrap_err();
        assert!(matches!(
            deterministic_error(&error),
            CompositionError::ResourceLimit(_)
        ));
    }
}

#[tokio::test]
async fn targeted_composition_charges_reads_nodes_generated_bytes_and_work() {
    let mut memory = Memory::default();
    let file = memory.raw_block(b"payload");
    let root = memory.dir(&[]);
    let mounts = [frozen_mount(&memory, &file, "/a").await];
    let expected = compose_mounts(&memory, Some(root.cid), &mounts)
        .await
        .unwrap();
    let input_bytes = memory.blocks[&root.cid].len() + memory.blocks[&file.cid].len();
    let generated_bytes = expected.blocks.iter().map(|(_, bytes)| bytes.len()).sum();
    for bounds in [
        limits(3, usize::MAX, usize::MAX, usize::MAX),
        limits(usize::MAX, input_bytes, usize::MAX, usize::MAX),
        limits(usize::MAX, usize::MAX, generated_bytes, usize::MAX),
        // One target-path visit and one final-root entry visit.
        limits(usize::MAX, usize::MAX, usize::MAX, 2),
    ] {
        let result = compose_mounts_with_limits(&memory, Some(root.cid), &mounts, bounds)
            .await
            .unwrap();
        assert_eq!(result.root, expected.root);
    }
    for bounds in [
        limits(2, usize::MAX, usize::MAX, usize::MAX),
        limits(usize::MAX, input_bytes - 1, usize::MAX, usize::MAX),
        limits(usize::MAX, usize::MAX, generated_bytes - 1, usize::MAX),
        limits(usize::MAX, usize::MAX, usize::MAX, 1),
    ] {
        let error = compose_mounts_with_limits(&memory, Some(root.cid), &mounts, bounds)
            .await
            .unwrap_err();
        assert!(matches!(
            deterministic_error(&error),
            CompositionError::ResourceLimit(_)
        ));
    }
}

#[tokio::test]
async fn composition_revalidates_sources_even_when_later_operations_overwrite_them() {
    let mut memory = Memory::default();
    let bad = memory.raw_block(b"captured");
    let good = memory.raw_block(b"replacement");
    let mounts = [
        frozen_mount(&memory, &bad, "/a").await,
        frozen_mount(&memory, &good, "/a").await,
    ];
    memory.blocks.insert(bad.cid, b"corrupt".to_vec());
    let error = compose_mounts(&memory, None, &mounts).await.unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::InvalidDag(_)
    ));
}

#[tokio::test]
async fn targeted_directory_reuses_alternate_encoding_and_leaf_replacement_uses_later_cid() {
    let mut memory = Memory::default();
    let overlay = memory.dir(&[("child", raw(b"same"))]);
    let links_first = &memory.blocks[&overlay.cid];
    assert!(links_first.ends_with(&[10, 2, 8, 1]));
    let mut data_first = vec![10, 2, 8, 1];
    data_first.extend_from_slice(&links_first[..links_first.len() - 4]);
    let alternate = Cid::new_v1(
        0x70,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&data_first)).unwrap(),
    );
    memory.blocks.insert(alternate, data_first);
    let base = Link {
        cid: alternate,
        size: overlay.size,
    };
    let root = memory.dir(&[("a", base)]);
    let mount = frozen_mount(&memory, &overlay, "/a").await;
    let result = compose_mounts(&memory, Some(root.cid), &[mount])
        .await
        .unwrap();
    assert_eq!(result.root, root.cid);
    assert!(result.blocks.is_empty());

    let file = memory.file(b"same", vec![], &[]);
    let v0 = Cid::new_v0(*file.cid.hash()).unwrap();
    memory.blocks.insert(v0, memory.blocks[&file.cid].clone());
    let root = memory.dir(&[(
        "a",
        Link {
            cid: v0,
            size: file.size,
        },
    )]);
    let mount = frozen_mount(&memory, &file, "/a").await;
    let result = compose_mounts(&memory, Some(root.cid), &[mount])
        .await
        .unwrap();
    assert_eq!(memory.result_dir(&result, result.root).entries["a"], file);
}

#[tokio::test]
async fn frozen_directory_preserves_opaque_raw_link_size_without_fetching_payload() {
    let mut memory = Memory::default();
    let raw = Link {
        size: 1234,
        ..raw(b"not stored")
    };
    let directory = memory.dir(&[("raw", raw.clone())]);
    let source = inspect_source(&memory, directory.cid).await.unwrap();
    assert_eq!(source.dag_size(), directory.size);
    let mount = FrozenMount::new(source, GuestTarget::new("/a").unwrap()).unwrap();
    let result = compose_mounts(&memory, None, &[mount]).await.unwrap();
    let root = memory.result_dir(&result, result.root);
    assert_eq!(root.entries["a"], directory);
    assert!(!memory.reads.lock().unwrap().contains(&raw.cid));
}

#[tokio::test]
async fn standalone_raw_obeys_block_size_limit() {
    let mut memory = Memory::default();
    let accepted = memory.raw_block(&vec![0; codec::MAX_BLOCK_BYTES]);
    assert_eq!(
        inspect_source(&memory, accepted.cid)
            .await
            .unwrap()
            .dag_size(),
        codec::MAX_BLOCK_BYTES as u64
    );
    let oversized = memory.raw_block(&vec![0; codec::MAX_BLOCK_BYTES + 1]);
    let error = inspect_source(&memory, oversized.cid).await.unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::ResourceLimit(_)
    ));
}

#[tokio::test]
async fn targeted_composition_and_standalone_inspection_are_cancellable_during_reads() {
    struct Pending(Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
    #[async_trait]
    impl BlockSource for Pending {
        async fn get(&self, _: &Cid) -> Result<Vec<u8>> {
            self.0.lock().unwrap().take().unwrap().send(()).unwrap();
            std::future::pending().await
        }
    }
    let mut memory = Memory::default();
    let file = memory.raw_block(b"payload");
    let mounts = [frozen_mount(&memory, &file, "/a").await];
    for inspecting in [true, false] {
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let source = Pending(Mutex::new(Some(started_tx)));
        let work = async {
            if inspecting {
                inspect_source(&source, file.cid).await.map(|_| ())
            } else {
                compose_mounts(&source, None, &mounts).await.map(|_| ())
            }
        };
        let pending = super::super::await_or_cancel(&mut cancel_rx, work);
        tokio::pin!(pending);
        tokio::select! {
            _ = &mut pending => panic!("block read must remain pending"),
            started = started_rx => started.unwrap(),
        }
        cancel_tx.send(true).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), pending)
                .await
                .unwrap()
                .is_err()
        );
    }
}

#[tokio::test]
async fn standalone_raw_is_verified_even_when_already_cached_as_directory_child() {
    let mut memory = Memory::default();
    let raw = memory.raw_block(b"payload");
    let root = memory.dir(&[(
        "opaque",
        Link {
            size: 999,
            ..raw.clone()
        },
    )]);
    let mount = frozen_mount(&memory, &raw, "/verified").await;
    let result = compose_mounts(&memory, Some(root.cid), std::slice::from_ref(&mount))
        .await
        .unwrap();
    let directory = memory.result_dir(&result, result.root);
    assert_eq!(directory.entries["opaque"].size, 999);
    assert_eq!(directory.entries["verified"], raw);
    memory.blocks.insert(raw.cid, b"corrupt".to_vec());
    let error = compose_mounts(&memory, Some(root.cid), std::slice::from_ref(&mount))
        .await
        .unwrap_err();
    assert!(matches!(
        deterministic_error(&error),
        CompositionError::InvalidDag(_)
    ));
    memory.blocks.remove(&raw.cid);
    let error = compose_mounts(&memory, Some(root.cid), &[mount])
        .await
        .unwrap_err();
    assert!(
        error.downcast_ref::<CompositionError>().is_none(),
        "missing source block must retain its transport classification: {error:#}"
    );
}

#[tokio::test]
async fn root_adapter_rejects_raw_files_without_fetching_their_payload() {
    let memory = Memory::default();
    let file = raw(b"not available locally");
    for error in [
        compose(&memory, &[file.cid]).await.unwrap_err(),
        compose_mounts(&memory, Some(file.cid), &[])
            .await
            .unwrap_err(),
    ] {
        assert!(
            matches!(
                error.downcast_ref::<CompositionError>(),
                Some(CompositionError::InvalidSource(_))
            ),
            "{error:#}"
        );
    }
    assert!(memory.reads.lock().unwrap().is_empty());
}
