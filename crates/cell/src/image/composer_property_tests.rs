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
    let root =
        match codec::decode(&result.root, fixture.block(&result, &result.root).to_vec()).unwrap() {
            Node::Directory(directory) => directory,
            _ => panic!("golden root must be a directory"),
        };
    assert_eq!(root.data.as_ref(), MODE_0700_MTIME_9_NS_1);
    assert_eq!(root.entries["docs-é"].cid, docs_v0.cid);
    assert_eq!(root.entries["replace"].cid.codec(), 0x70);
    assert!(root.entries.contains_key("追加"));
}
