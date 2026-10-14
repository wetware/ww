//! Independent bounded model checks for Composer v1.

use super::{compose_with_limits, BlockSource, Composition, Limits};
use crate::image::codec::{self, Directory, Link, Node};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use cid::Cid;
use ipld_dagpb::PbNode;
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

const DIRECTORY: &[u8] = &[8, 1];
const MODE_0755: &[u8] = &[8, 1, 56, 0xed, 3];
const MODE_0700: &[u8] = &[8, 1, 56, 0xc0, 3];
const MTIME_7: &[u8] = &[8, 1, 66, 2, 8, 7];
const MODE_0755_MTIME_7_NS_9: &[u8] = &[8, 1, 56, 0xed, 3, 66, 7, 8, 7, 21, 9, 0, 0, 0];
const MODE_0700_MTIME_9_NS_1: &[u8] = &[8, 1, 56, 0xc0, 3, 66, 7, 8, 9, 21, 1, 0, 0, 0];
const GOLDEN_BIN_BYTES: &[u8] = &[
    0x12, 0x34, 0x0a, 0x24, 0x01, 0x70, 0x12, 0x20, 0x7a, 0x1e, 0x0a, 0xed, 0x8f, 0x1d, 0xba, 0x50,
    0x78, 0x58, 0xaf, 0x2f, 0x19, 0xcb, 0xec, 0xf3, 0xee, 0xc9, 0x54, 0x30, 0xa1, 0xb8, 0xc4, 0xca,
    0xf7, 0xf8, 0x8f, 0x33, 0xbb, 0xad, 0xeb, 0x13, 0x12, 0x0a, 0x66, 0x69, 0x6e, 0x61, 0x6c, 0x2d,
    0x6c, 0x69, 0x6e, 0x6b, 0x18, 0x0f, 0x12, 0x30, 0x0a, 0x24, 0x01, 0x55, 0x12, 0x20, 0x5e, 0x86,
    0x66, 0x9c, 0xea, 0x4e, 0x8c, 0x4c, 0x5d, 0xf2, 0xb8, 0x46, 0xf8, 0x2e, 0xee, 0x7c, 0x29, 0xe7,
    0x15, 0x3d, 0x85, 0x4d, 0xe0, 0x4b, 0x58, 0x50, 0x04, 0xd4, 0x14, 0xdf, 0x9d, 0x67, 0x12, 0x06,
    0x68, 0x65, 0x6c, 0x70, 0x65, 0x72, 0x18, 0x05, 0x12, 0x2e, 0x0a, 0x24, 0x01, 0x70, 0x12, 0x20,
    0x69, 0x01, 0x3e, 0x4b, 0x3e, 0xed, 0xa1, 0xb5, 0xe3, 0x41, 0x47, 0xdc, 0xe7, 0x59, 0xfc, 0x81,
    0x84, 0x97, 0x15, 0xb1, 0x1a, 0x02, 0x40, 0xb9, 0xee, 0xa6, 0x81, 0x1a, 0x71, 0x4d, 0xd3, 0x45,
    0x12, 0x04, 0x74, 0x6f, 0x6f, 0x6c, 0x18, 0x0f, 0x0a, 0x06, 0x08, 0x01, 0x42, 0x02, 0x08, 0x07,
];
const GOLDEN_ROOT_BYTES: &[u8] = &[
    0x12, 0x2e, 0x0a, 0x24, 0x01, 0x70, 0x12, 0x20, 0x1c, 0x2d, 0x0f, 0x1d, 0xc6, 0x7a, 0x68, 0x91,
    0xe8, 0x17, 0xe5, 0xd6, 0x43, 0xde, 0xd7, 0xac, 0xeb, 0xd7, 0x6f, 0x87, 0xa1, 0x34, 0xc2, 0x38,
    0x0b, 0xe8, 0x5f, 0x05, 0x34, 0x20, 0x4b, 0x83, 0x12, 0x03, 0x62, 0x69, 0x6e, 0x18, 0xc3, 0x01,
    0x12, 0x2f, 0x0a, 0x22, 0x12, 0x20, 0x1f, 0x20, 0x9c, 0x90, 0xea, 0xe5, 0x55, 0x60, 0xdb, 0x85,
    0xfd, 0xa2, 0x97, 0xd7, 0x06, 0x72, 0xa0, 0xf2, 0x3f, 0x5c, 0x12, 0xe2, 0x3f, 0x75, 0x1c, 0x6d,
    0x0c, 0x39, 0x20, 0xf4, 0x8b, 0x0f, 0x12, 0x07, 0x64, 0x6f, 0x63, 0x73, 0x2d, 0xc3, 0xa9, 0x18,
    0x49, 0x12, 0x31, 0x0a, 0x24, 0x01, 0x70, 0x12, 0x20, 0x7a, 0x1e, 0x0a, 0xed, 0x8f, 0x1d, 0xba,
    0x50, 0x78, 0x58, 0xaf, 0x2f, 0x19, 0xcb, 0xec, 0xf3, 0xee, 0xc9, 0x54, 0x30, 0xa1, 0xb8, 0xc4,
    0xca, 0xf7, 0xf8, 0x8f, 0x33, 0xbb, 0xad, 0xeb, 0x13, 0x12, 0x07, 0x72, 0x65, 0x70, 0x6c, 0x61,
    0x63, 0x65, 0x18, 0x0f, 0x12, 0x2b, 0x0a, 0x22, 0x12, 0x20, 0xec, 0x4d, 0x43, 0xb1, 0x77, 0xe7,
    0xba, 0x91, 0x61, 0x2b, 0x61, 0x1d, 0x37, 0x2b, 0xba, 0x76, 0x1c, 0xfa, 0x37, 0x4b, 0x4a, 0x89,
    0x1e, 0x33, 0x25, 0x2a, 0x9e, 0xa5, 0x38, 0xee, 0x5d, 0x25, 0x12, 0x03, 0x73, 0x79, 0x6d, 0x18,
    0x0f, 0x12, 0x2c, 0x0a, 0x24, 0x01, 0x70, 0x12, 0x20, 0x69, 0x01, 0x3e, 0x4b, 0x3e, 0xed, 0xa1,
    0xb5, 0xe3, 0x41, 0x47, 0xdc, 0xe7, 0x59, 0xfc, 0x81, 0x84, 0x97, 0x15, 0xb1, 0x1a, 0x02, 0x40,
    0xb9, 0xee, 0xa6, 0x81, 0x1a, 0x71, 0x4d, 0xd3, 0x45, 0x12, 0x02, 0xce, 0xb4, 0x18, 0x0f, 0x12,
    0x30, 0x0a, 0x24, 0x01, 0x55, 0x12, 0x20, 0x5e, 0xe0, 0x7e, 0x90, 0x92, 0x80, 0xff, 0x0e, 0x9e,
    0x17, 0x50, 0x43, 0xfb, 0x22, 0x70, 0x25, 0x62, 0xbc, 0xfb, 0xb0, 0xc1, 0xbe, 0x74, 0xad, 0x43,
    0xdb, 0x54, 0xa5, 0xec, 0x5a, 0x1a, 0x57, 0x12, 0x06, 0xe8, 0xbf, 0xbd, 0xe5, 0x8a, 0xa0, 0x18,
    0x05, 0x0a, 0x0e, 0x08, 0x01, 0x38, 0xc0, 0x03, 0x42, 0x07, 0x08, 0x09, 0x15, 0x01, 0x00, 0x00,
    0x00,
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelDirectory {
    metadata: Vec<u8>,
    entries: BTreeMap<String, ModelNode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelLeaf {
    cid: Cid,
    size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ModelNode {
    Directory(ModelDirectory),
    File(ModelLeaf),
    Raw(ModelLeaf),
    Symlink(ModelLeaf),
}

impl ModelNode {
    fn directory(metadata: &[u8], entries: impl IntoIterator<Item = (String, ModelNode)>) -> Self {
        Self::Directory(ModelDirectory {
            metadata: metadata.to_vec(),
            entries: entries.into_iter().collect(),
        })
    }
}

/// Apply one semantic layer without using production `Composer::merge`,
/// production `Directory`, links, encoded bytes, sizes, or CIDs for directories.
fn overlay_model(base: &ModelNode, overlay: &ModelNode) -> ModelNode {
    match (base, overlay) {
        (ModelNode::Directory(base), ModelNode::Directory(overlay)) => {
            let mut entries = base.entries.clone();
            for (name, overlay_node) in &overlay.entries {
                let merged = entries.get(name).map_or_else(
                    || overlay_node.clone(),
                    |base_node| overlay_model(base_node, overlay_node),
                );
                entries.insert(name.clone(), merged);
            }
            ModelNode::Directory(ModelDirectory {
                metadata: overlay.metadata.clone(),
                entries,
            })
        }
        (_, overlay) => overlay.clone(),
    }
}

fn compose_model(layers: &[ModelNode]) -> ModelNode {
    layers[1..].iter().fold(layers[0].clone(), |base, overlay| {
        overlay_model(&base, overlay)
    })
}

#[derive(Clone, PartialEq, Message)]
struct TestUnixFs {
    #[prost(int32, optional, tag = "1")]
    kind: Option<i32>,
    #[prost(bytes = "bytes", optional, tag = "2")]
    data: Option<Bytes>,
    #[prost(uint64, optional, tag = "3")]
    filesize: Option<u64>,
}

#[derive(Default)]
struct Fixture {
    blocks: HashMap<Cid, Vec<u8>>,
}

#[async_trait]
impl BlockSource for Fixture {
    async fn get(&self, cid: &Cid) -> Result<Vec<u8>> {
        self.blocks
            .get(cid)
            .cloned()
            .with_context(|| format!("missing fixture block {cid}"))
    }
}

impl Fixture {
    fn raw(&mut self, bytes: &[u8]) -> ModelNode {
        let cid = Cid::new_v1(
            0x55,
            cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
        );
        let link = Link {
            cid,
            size: bytes.len() as u64,
        };
        self.blocks.insert(cid, bytes.to_vec());
        ModelNode::Raw(ModelLeaf {
            cid,
            size: link.size,
        })
    }

    fn unixfs_leaf(&mut self, kind: i32, data: &[u8], cid_v0: bool) -> ModelNode {
        let metadata = TestUnixFs {
            kind: Some(kind),
            data: Some(Bytes::copy_from_slice(data)),
            filesize: (kind == 2).then_some(data.len() as u64),
        }
        .encode_to_vec();
        let bytes = PbNode {
            links: Vec::new(),
            data: Some(Bytes::from(metadata)),
        }
        .into_bytes();
        let hash = cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&bytes)).unwrap();
        let cid = if cid_v0 {
            Cid::new_v0(hash).unwrap()
        } else {
            Cid::new_v1(0x70, hash)
        };
        let link = Link {
            cid,
            size: bytes.len() as u64,
        };
        self.blocks.insert(cid, bytes);
        match kind {
            2 => ModelNode::File(ModelLeaf {
                cid,
                size: link.size,
            }),
            4 => ModelNode::Symlink(ModelLeaf {
                cid,
                size: link.size,
            }),
            _ => unreachable!("test fixture supports File and Symlink leaves"),
        }
    }

    fn stable_leaves(&mut self) -> Vec<ModelNode> {
        let raw_a = self.raw(b"raw-a");
        let raw_a_with_later_size = match &raw_a {
            ModelNode::Raw(leaf) => ModelNode::Raw(ModelLeaf {
                cid: leaf.cid,
                size: leaf.size + 3,
            }),
            _ => unreachable!(),
        };
        vec![
            raw_a,
            self.raw(b"raw-b"),
            self.unixfs_leaf(2, b"file-v1", false),
            self.unixfs_leaf(2, b"file-v0", true),
            self.unixfs_leaf(4, b"../target", false),
            self.unixfs_leaf(4, b"target-v0", true),
            raw_a_with_later_size,
        ]
    }

    fn leaf_link(&self, node: &ModelNode) -> Link {
        let leaf = match node {
            ModelNode::File(leaf) | ModelNode::Raw(leaf) | ModelNode::Symlink(leaf) => leaf,
            ModelNode::Directory(_) => panic!("directory is not a leaf"),
        };
        Link {
            cid: leaf.cid,
            size: leaf.size,
        }
    }

    fn encode_tree(&mut self, node: &ModelNode) -> Link {
        let ModelNode::Directory(directory) = node else {
            return self.leaf_link(node);
        };
        let mut entries = BTreeMap::new();
        for (name, child) in &directory.entries {
            entries.insert(name.clone(), self.encode_tree(child));
        }
        self.encode_directory(&directory.metadata, entries)
    }

    fn encode_directory(&mut self, metadata: &[u8], entries: BTreeMap<String, Link>) -> Link {
        let (cid, bytes, size) = codec::encode(&Directory {
            data: Bytes::copy_from_slice(metadata),
            entries,
        })
        .unwrap();
        self.blocks.insert(cid, bytes);
        Link { cid, size }
    }

    fn directory(&mut self, metadata: &[u8], entries: &[(&str, Link)]) -> Link {
        self.encode_directory(
            metadata,
            entries
                .iter()
                .map(|(name, link)| ((*name).to_owned(), link.clone()))
                .collect(),
        )
    }

    fn directory_in_order(
        &mut self,
        metadata: &[u8],
        mut entries: Vec<(&str, Link)>,
        reverse: bool,
    ) -> Link {
        if reverse {
            entries.reverse();
        }
        let mut ordered = BTreeMap::new();
        for (name, link) in entries {
            ordered.insert(name.to_owned(), link);
        }
        self.encode_directory(metadata, ordered)
    }

    fn block<'a>(&'a self, composition: &'a Composition, cid: &Cid) -> &'a [u8] {
        composition
            .blocks
            .iter()
            .find(|(candidate, _)| candidate == cid)
            .map(|(_, bytes)| bytes.as_slice())
            .or_else(|| self.blocks.get(cid).map(Vec::as_slice))
            .unwrap_or_else(|| panic!("missing result block {cid}"))
    }

    fn decode_semantic(&self, composition: &Composition, cid: Cid) -> ModelNode {
        self.decode_semantic_node(composition, cid, None)
    }

    fn decode_semantic_node(
        &self,
        composition: &Composition,
        cid: Cid,
        advertised_size: Option<u64>,
    ) -> ModelNode {
        if cid.codec() == 0x55 {
            return ModelNode::Raw(ModelLeaf {
                cid,
                size: advertised_size.expect("raw entry must have an advertised size"),
            });
        }
        match codec::decode(&cid, self.block(composition, &cid).to_vec()).unwrap() {
            Node::Directory(directory) => ModelNode::Directory(ModelDirectory {
                metadata: directory.data.to_vec(),
                entries: directory
                    .entries
                    .into_iter()
                    .map(|(name, link)| {
                        (
                            name,
                            self.decode_semantic_node(composition, link.cid, Some(link.size)),
                        )
                    })
                    .collect(),
            }),
            Node::File { .. } => ModelNode::File(ModelLeaf {
                cid,
                size: advertised_size.expect("file entry must have an advertised size"),
            }),
            Node::Raw { .. } => ModelNode::Raw(ModelLeaf {
                cid,
                size: advertised_size.expect("DAG-PB Raw entry must have an advertised size"),
            }),
            Node::Symlink { .. } => ModelNode::Symlink(ModelLeaf {
                cid,
                size: advertised_size.expect("symlink entry must have an advertised size"),
            }),
        }
    }

    fn alternate_directory_representation(&mut self, link: &Link, variant: Representation) -> Link {
        let canonical = self.blocks[&link.cid].clone();
        let bytes = match variant {
            Representation::LinksFirstV0 => canonical,
            Representation::DataFirstV0 | Representation::DataFirstV1 => {
                let node = PbNode::from_bytes(canonical.into()).unwrap();
                let mut data_first = PbNode {
                    links: Vec::new(),
                    data: node.data,
                }
                .into_bytes();
                data_first.extend(
                    PbNode {
                        links: node.links,
                        data: None,
                    }
                    .into_bytes(),
                );
                data_first
            }
        };
        let hash = cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&bytes)).unwrap();
        let cid = match variant {
            Representation::DataFirstV1 => Cid::new_v1(0x70, hash),
            Representation::LinksFirstV0 | Representation::DataFirstV0 => {
                Cid::new_v0(hash).unwrap()
            }
        };
        self.blocks.insert(cid, bytes);
        Link {
            cid,
            size: link.size,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Representation {
    LinksFirstV0,
    DataFirstV0,
    DataFirstV1,
}

struct Deterministic(u64);

impl Deterministic {
    fn new(case: u64) -> Self {
        Self(0x9e37_79b9_7f4a_7c15 ^ case.wrapping_mul(0xd134_2543_de82_ef95))
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn index(&mut self, length: usize) -> usize {
        ((self.next() >> 32) as usize) % length
    }
}

fn generate_directory(rng: &mut Deterministic, leaves: &[ModelNode], depth: usize) -> ModelNode {
    const NAMES: &[&str] = &["a", "b", "é", "e\u{301}", "δ", "slot"];
    const METADATA: &[&[u8]] = &[
        DIRECTORY,
        MODE_0755,
        MODE_0700,
        MTIME_7,
        MODE_0755_MTIME_7_NS_9,
    ];
    let count = 1 + rng.index(4);
    let mut entries = BTreeMap::new();
    for entry_index in 0..count {
        let name = if entry_index == 0 {
            "slot"
        } else {
            NAMES[rng.index(NAMES.len())]
        };
        let child = if depth < 2 && rng.index(4) == 0 {
            generate_directory(rng, leaves, depth + 1)
        } else {
            leaves[rng.index(leaves.len())].clone()
        };
        entries.insert(name.to_owned(), child);
    }
    ModelNode::directory(METADATA[rng.index(METADATA.len())], entries)
}

fn generated_layers(case: u64, leaves: &[ModelNode]) -> Vec<ModelNode> {
    if case == 0 {
        // Pin the core recursive case in the 64-case set. The overlay omits
        // metadata and supplies a different advertised size for the same raw CID.
        return vec![
            ModelNode::directory(
                MODE_0755_MTIME_7_NS_9,
                [
                    ("slot".into(), leaves[0].clone()),
                    (
                        "nested".into(),
                        ModelNode::directory(MODE_0755, [("base".into(), leaves[2].clone())]),
                    ),
                ],
            ),
            ModelNode::directory(
                DIRECTORY,
                [
                    ("slot".into(), leaves[6].clone()),
                    (
                        "nested".into(),
                        ModelNode::directory(DIRECTORY, [("overlay".into(), leaves[4].clone())]),
                    ),
                ],
            ),
        ];
    }
    let mut rng = Deterministic::new(case);
    let layer_count = 2 + rng.index(3);
    (0..layer_count)
        .map(|_| generate_directory(&mut rng, leaves, 0))
        .collect()
}

fn property_limits() -> Limits {
    Limits {
        max_nodes: 512,
        max_read_bytes: 1024 * 1024,
        max_written_bytes: 1024 * 1024,
        max_work: 4096,
    }
}

fn has_nested_directory_collision(base: &ModelNode, overlay: &ModelNode, depth: usize) -> bool {
    let (ModelNode::Directory(base), ModelNode::Directory(overlay)) = (base, overlay) else {
        return false;
    };
    if depth > 0 {
        return true;
    }
    overlay.entries.iter().any(|(name, overlay_node)| {
        base.entries
            .get(name)
            .is_some_and(|base_node| has_nested_directory_collision(base_node, overlay_node, 1))
    })
}

fn insertion_order_cases(
    fixture: &mut Fixture,
    leaves: &[ModelNode],
    reverse: bool,
) -> Vec<Vec<Cid>> {
    let links = leaves
        .iter()
        .map(|leaf| fixture.leaf_link(leaf))
        .collect::<Vec<_>>();
    let ordinary = fixture.directory_in_order(
        DIRECTORY,
        vec![("a", links[0].clone()), ("b", links[1].clone())],
        reverse,
    );

    let nested_child = fixture.directory_in_order(
        MODE_0755,
        vec![("é", links[2].clone()), ("e\u{301}", links[4].clone())],
        reverse,
    );
    let nested = fixture.directory_in_order(
        DIRECTORY,
        vec![("nested", nested_child), ("raw", links[0].clone())],
        reverse,
    );

    let metadata = fixture.directory_in_order(
        MODE_0755_MTIME_7_NS_9,
        vec![("file", links[3].clone()), ("link", links[5].clone())],
        reverse,
    );

    let base = fixture.directory_in_order(
        MODE_0755,
        vec![("keep", links[0].clone()), ("slot", links[1].clone())],
        reverse,
    );
    let overlay = fixture.directory_in_order(
        MODE_0700_MTIME_9_NS_1,
        vec![("add", links[2].clone()), ("slot", links[4].clone())],
        reverse,
    );
    let final_layer = fixture.directory_in_order(
        MTIME_7,
        vec![("last", links[5].clone()), ("more", links[6].clone())],
        reverse,
    );

    vec![
        vec![ordinary.cid],
        vec![nested.cid],
        vec![metadata.cid],
        vec![base.cid, overlay.cid, final_layer.cid],
    ]
}

#[tokio::test]
async fn production_matches_independent_overlay_model_for_64_cases() {
    let mut saw_nested_directory_collision = false;
    for case in 0..64 {
        let mut fixture = Fixture::default();
        let leaves = fixture.stable_leaves();
        let layers = generated_layers(case, &leaves);
        let expected = compose_model(&layers);
        let mut coverage_base = layers[0].clone();
        for overlay in &layers[1..] {
            saw_nested_directory_collision |=
                has_nested_directory_collision(&coverage_base, overlay, 0);
            coverage_base = overlay_model(&coverage_base, overlay);
        }
        let roots = layers
            .iter()
            .map(|layer| fixture.encode_tree(layer).cid)
            .collect::<Vec<_>>();

        let composition = compose_with_limits(&fixture, &roots, property_limits())
            .await
            .unwrap_or_else(|error| panic!("case {case} failed to compose: {error:#}"));
        let actual = fixture.decode_semantic(&composition, composition.root);

        assert_eq!(
            actual, expected,
            "model mismatch for case {case}: {layers:#?}"
        );
    }
    assert!(
        saw_nested_directory_collision,
        "the deterministic cases must exercise recursive directory merge"
    );
}

#[tokio::test]
async fn production_identity_is_independent_of_recursive_insertion_order() {
    let mut fixture = Fixture::default();
    let leaves = fixture.stable_leaves();
    let forward_cases = insertion_order_cases(&mut fixture, &leaves, false);
    let reverse_cases = insertion_order_cases(&mut fixture, &leaves, true);

    for (case, (forward, reverse)) in forward_cases.into_iter().zip(reverse_cases).enumerate() {
        assert_eq!(forward, reverse, "input identity differs for case {case}");

        let forward_result = compose_with_limits(&fixture, &forward, property_limits())
            .await
            .unwrap();
        let reverse_result = compose_with_limits(&fixture, &reverse, property_limits())
            .await
            .unwrap();
        assert_eq!(
            forward_result.root, reverse_result.root,
            "composition identity differs for case {case}"
        );
    }
}

#[tokio::test]
async fn semantically_equal_directories_prefer_base_across_twelve_representation_pairs() {
    let mut fixture = Fixture::default();
    let leaves = fixture.stable_leaves();
    let trees = [
        ModelNode::directory(DIRECTORY, [("file".into(), leaves[2].clone())]),
        ModelNode::directory(DIRECTORY, [("raw".into(), leaves[0].clone())]),
        ModelNode::directory(
            MODE_0755_MTIME_7_NS_9,
            [
                ("file".into(), leaves[2].clone()),
                ("link".into(), leaves[4].clone()),
            ],
        ),
        ModelNode::directory(
            MODE_0700,
            [(
                "nested".into(),
                ModelNode::directory(MTIME_7, [("raw".into(), leaves[1].clone())]),
            )],
        ),
    ];

    for (tree_index, tree) in trees.iter().enumerate() {
        let overlay = fixture.encode_tree(tree);
        for representation in [
            Representation::LinksFirstV0,
            Representation::DataFirstV0,
            Representation::DataFirstV1,
        ] {
            let base = fixture.alternate_directory_representation(&overlay, representation);
            assert_ne!(base.cid, overlay.cid);
            let result = compose_with_limits(&fixture, &[base.cid, overlay.cid], property_limits())
                .await
                .unwrap();
            assert_eq!(
                result.root, base.cid,
                "tree {tree_index}, representation {representation:?}"
            );
        }
    }
}

#[tokio::test]
async fn richer_composer_v1_identity_vector() {
    let mut fixture = Fixture::default();
    let leaves = fixture.stable_leaves();
    let file_v1 = fixture.leaf_link(&leaves[2]);
    let file_v0 = fixture.leaf_link(&leaves[3]);
    let symlink_v1 = fixture.leaf_link(&leaves[4]);
    let symlink_v0 = fixture.leaf_link(&leaves[5]);
    let raw_a = fixture.leaf_link(&leaves[0]);
    let raw_b = fixture.leaf_link(&leaves[1]);

    // Stable Composer v1 identity fixture:
    // - base supplies an unchanged CIDv0 `docs-é` subtree and a CIDv0 file;
    // - overlay recursively merges `bin`, replaces `replace`, and changes metadata;
    // - final adds a symlink and supplies the final mode plus fractional mtime;
    // - the result retains file, raw, symlink, CIDv0, and CIDv1 identities.
    let docs_v1 = fixture.directory(MTIME_7, &[("readme", file_v1.clone())]);
    let docs_v0 =
        fixture.alternate_directory_representation(&docs_v1, Representation::LinksFirstV0);
    let base_bin = fixture.directory(MODE_0755, &[("tool", file_v0)]);
    let replaced = fixture.directory(DIRECTORY, &[("old", raw_a.clone())]);
    let base = fixture.directory(
        MODE_0755_MTIME_7_NS_9,
        &[
            ("bin", base_bin),
            ("docs-é", docs_v0.clone()),
            ("replace", replaced),
            ("sym", symlink_v0),
        ],
    );

    let overlay_bin = fixture.directory(
        MODE_0700,
        &[("helper", raw_b.clone()), ("tool", file_v1.clone())],
    );
    let overlay = fixture.directory(
        MODE_0700,
        &[
            ("bin", overlay_bin),
            ("replace", symlink_v1.clone()),
            ("δ", file_v1.clone()),
        ],
    );

    let final_bin = fixture.directory(MTIME_7, &[("final-link", symlink_v1)]);
    let final_layer = fixture.directory(
        MODE_0700_MTIME_9_NS_1,
        &[("bin", final_bin), ("追加", raw_a)],
    );

    let result = compose_with_limits(
        &fixture,
        &[base.cid, overlay.cid, final_layer.cid],
        property_limits(),
    )
    .await
    .unwrap();

    assert_eq!(
        result.root.to_string(),
        "bafybeihsp2xyejzajvqbbuwz2kep2bgbzho2waqi6pp4dfdlaf7jsqqkaq",
        "intentional Composer profile changes must update this documented vector"
    );
    let expected_bin = "bafybeia4fuhr3rt2nci6qf7f2zb55v5m5plw7b5bgtbdqc7il4ctiiclqm"
        .parse::<Cid>()
        .unwrap();
    let generated = result
        .blocks
        .iter()
        .map(|(cid, bytes)| (*cid, bytes.as_slice()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        generated,
        BTreeMap::from([
            (expected_bin, GOLDEN_BIN_BYTES),
            (result.root, GOLDEN_ROOT_BYTES),
        ]),
        "golden vector must generate exactly the checked-in final bin and root blocks"
    );
    let root =
        match codec::decode(&result.root, fixture.block(&result, &result.root).to_vec()).unwrap() {
            Node::Directory(directory) => directory,
            _ => panic!("golden root must be a directory"),
        };
    assert_eq!(root.entries["bin"].cid, expected_bin);
    assert_eq!(root.data.as_ref(), MODE_0700_MTIME_9_NS_1);
    assert_eq!(root.entries["docs-é"].cid, docs_v0.cid);
    assert_eq!(root.entries["replace"].cid.codec(), 0x70);
    assert!(root.entries.contains_key("追加"));
}
