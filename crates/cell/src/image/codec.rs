//! Strict UnixFS boundary for Composer v1.
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use cid::Cid;
use ipld_dagpb::{PbLink, PbNode};
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(super) const MAX_BLOCK_BYTES: usize = 1024 * 1024;
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Directory {
    pub data: Bytes,
    pub entries: BTreeMap<String, Link>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Link {
    pub cid: Cid,
    pub size: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct File {
    pub inline_size: u64,
    pub logical_size: u64,
    pub links: Vec<FileLink>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FileLink {
    pub cid: Cid,
    pub size: u64,
    pub logical_size: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Node {
    Directory(Directory),
    File { file: File, size: u64 },
    Raw { logical_size: u64, size: u64 },
    Symlink { size: u64 },
}
// These optional fields preserve presence. Re-encoding below rejects unknown,
// duplicate, nonminimal, out-of-order, and packed fields. Composer v1 accepts
// the canonical UnixFS protobuf emitted by Kubo, including mode and mtime.
#[derive(Clone, PartialEq, Message)]
struct UnixFs {
    #[prost(int32, optional, tag = "1")]
    kind: Option<i32>,
    #[prost(bytes = "bytes", optional, tag = "2")]
    data: Option<Bytes>,
    #[prost(uint64, optional, tag = "3")]
    filesize: Option<u64>,
    #[prost(uint64, repeated, packed = "false", tag = "4")]
    blocksizes: Vec<u64>,
    #[prost(uint64, optional, tag = "5")]
    hash_type: Option<u64>,
    #[prost(uint64, optional, tag = "6")]
    fanout: Option<u64>,
    #[prost(uint32, optional, tag = "7")]
    mode: Option<u32>,
    #[prost(message, optional, tag = "8")]
    mtime: Option<UnixTime>,
}

#[derive(Clone, PartialEq, Message)]
struct UnixTime {
    #[prost(int64, optional, tag = "1")]
    seconds: Option<i64>,
    #[prost(fixed32, optional, tag = "2")]
    fractional_nanoseconds: Option<u32>,
}

fn metadata(data: &[u8]) -> Result<UnixFs> {
    let metadata = UnixFs::decode(data).context("malformed UnixFS metadata")?;
    if metadata.encode_to_vec() != data {
        bail!("noncanonical, duplicate, or unknown UnixFS metadata fields");
    }
    if metadata.kind.is_none() {
        bail!("missing UnixFS node type");
    }
    if metadata.mode.is_some_and(|mode| mode > 0xfff) {
        bail!("unsupported UnixFS mode bits");
    }
    if let Some(mtime) = &metadata.mtime {
        if mtime.seconds.is_none()
            || mtime
                .fractional_nanoseconds
                .is_some_and(|value| value == 0 || value >= 1_000_000_000)
        {
            bail!("invalid UnixFS mtime");
        }
    }
    Ok(metadata)
}

fn validate_directory_metadata(metadata: &UnixFs) -> Result<()> {
    if metadata.kind != Some(1)
        || metadata.data.is_some()
        || metadata.filesize.is_some()
        || !metadata.blocksizes.is_empty()
        || metadata.hash_type.is_some()
        || metadata.fanout.is_some()
    {
        bail!("unsupported UnixFS directory metadata; only Type, mode, and mtime are supported");
    }
    Ok(())
}

enum MetadataKind {
    Directory,
    File { inline_size: u64, logical_size: u64 },
    Raw { logical_size: u64 },
    Symlink,
}

fn validated_metadata(data: &[u8]) -> Result<(UnixFs, MetadataKind)> {
    let metadata = metadata(data)?;
    let kind = match metadata.kind {
        Some(1) => {
            validate_directory_metadata(&metadata)?;
            MetadataKind::Directory
        }
        Some(0 | 2 | 4) => {
            if metadata.hash_type.is_some() || metadata.fanout.is_some() {
                bail!("unsupported UnixFS leaf metadata");
            }
            let inline_size = metadata.data.as_ref().map_or(0, |data| data.len() as u64);
            match metadata.kind {
                Some(2) => {
                    let logical_size = metadata
                        .blocksizes
                        .iter()
                        .try_fold(inline_size, |sum, size| {
                            sum.checked_add(*size).context("UnixFS file size overflow")
                        })?;
                    if metadata.filesize != Some(logical_size) {
                        bail!("UnixFS file size does not match inline data and blocksizes");
                    }
                    MetadataKind::File {
                        inline_size,
                        logical_size,
                    }
                }
                Some(0) => {
                    if !metadata.blocksizes.is_empty() || metadata.filesize != Some(inline_size) {
                        bail!("invalid UnixFS Raw node");
                    }
                    MetadataKind::Raw {
                        logical_size: inline_size,
                    }
                }
                Some(4) => {
                    if !metadata.blocksizes.is_empty() || metadata.filesize.is_some() {
                        bail!("invalid UnixFS symlink metadata or links");
                    }
                    let target = metadata
                        .data
                        .as_deref()
                        .context("missing UnixFS symlink target")?;
                    std::str::from_utf8(target).context("invalid UTF-8 UnixFS symlink target")?;
                    MetadataKind::Symlink
                }
                _ => unreachable!(),
            }
        }
        Some(5) => bail!("Composer v1 does not support UnixFS HAMT shards"),
        Some(kind) => bail!("unsupported UnixFS node type {kind}"),
        None => unreachable!(),
    };
    Ok((metadata, kind))
}

pub(super) fn validate_metadata(data: &[u8]) -> Result<()> {
    validated_metadata(data).map(|_| ())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.bytes().any(|byte| byte.is_ascii_control())
    {
        bail!("invalid UnixFS directory entry name: {name:?}");
    }
    Ok(())
}

pub(super) fn verify_cid(cid: &Cid, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_BLOCK_BYTES {
        bail!("Composer v1 block exceeds {MAX_BLOCK_BYTES} bytes");
    }
    if cid.hash().code() != 0x12 || cid.hash().size() != 32 {
        bail!("Composer v1 supports only sha2-256 CIDs");
    }
    if cid.hash().digest() != Sha256::digest(bytes).as_slice() {
        bail!("block does not match requested CID {cid}");
    }
    Ok(())
}

pub(super) fn decode(cid: &Cid, bytes: Vec<u8>) -> Result<Node> {
    verify_cid(cid, &bytes)?;
    if cid.codec() != 0x70 {
        bail!("expected DAG-PB UnixFS block, got codec {}", cid.codec());
    }
    let block_len = bytes.len() as u64;
    let node = PbNode::from_bytes(Bytes::from(bytes.clone())).context("malformed DAG-PB block")?;
    // DAG-PB permits Data before or after the contiguous Links section.
    // Comparison also rejects duplicate PBLink fields and noncanonical CIDs.
    let canonical = node.clone().into_bytes();
    if canonical != bytes {
        let mut data_first = PbNode {
            links: Vec::new(),
            data: node.data.clone(),
        }
        .into_bytes();
        data_first.extend(
            PbNode {
                links: node.links.clone(),
                data: None,
            }
            .into_bytes(),
        );
        if data_first != bytes {
            bail!("noncanonical DAG-PB block: invalid fields, CID encoding, or link order");
        }
    }
    let data = node.data.context("missing UnixFS metadata")?;
    let (metadata, metadata_kind) = validated_metadata(&data)?;
    let mut size = block_len;
    for link in &node.links {
        let link_size = link.size.context("missing DAG-PB link Tsize")?;
        size = size
            .checked_add(link_size)
            .context("UnixFS cumulative size overflow")?;
    }
    match metadata_kind {
        MetadataKind::Directory => {
            let mut entries = BTreeMap::new();
            for link in node.links {
                let name = link.name.context("missing UnixFS directory link name")?;
                validate_name(&name)?;
                if entries
                    .insert(
                        name.clone(),
                        Link {
                            cid: link.cid,
                            size: link.size.unwrap(),
                        },
                    )
                    .is_some()
                {
                    bail!("duplicate UnixFS directory entry name: {name:?}");
                }
            }
            Ok(Node::Directory(Directory { data, entries }))
        }
        MetadataKind::File {
            inline_size,
            logical_size,
        } => {
            if metadata.blocksizes.len() != node.links.len() {
                bail!("UnixFS file blocksizes do not match links");
            }
            if node
                .links
                .iter()
                .any(|link| link.name.as_ref().is_some_and(|name| !name.is_empty()))
            {
                bail!("UnixFS file links must be unnamed");
            }
            let links = node
                .links
                .into_iter()
                .zip(metadata.blocksizes)
                .map(|(link, logical_size)| FileLink {
                    cid: link.cid,
                    size: link.size.unwrap(),
                    logical_size,
                })
                .collect();
            Ok(Node::File {
                file: File {
                    inline_size,
                    logical_size,
                    links,
                },
                size,
            })
        }
        MetadataKind::Raw { logical_size } => {
            if !node.links.is_empty() {
                bail!("invalid UnixFS Raw node");
            }
            Ok(Node::Raw { logical_size, size })
        }
        MetadataKind::Symlink => {
            if !node.links.is_empty() {
                bail!("invalid UnixFS symlink metadata or links");
            }
            Ok(Node::Symlink { size })
        }
    }
}

pub(super) fn encode(directory: &Directory) -> Result<(Cid, Vec<u8>, u64)> {
    validate_directory_metadata(&metadata(&directory.data)?)?;
    let mut links = Vec::with_capacity(directory.entries.len());
    let mut children_size = 0_u64;
    for (name, link) in &directory.entries {
        validate_name(name)?;
        children_size = children_size
            .checked_add(link.size)
            .context("UnixFS cumulative size overflow")?;
        links.push(PbLink {
            cid: link.cid,
            name: Some(name.clone()),
            size: Some(link.size),
        });
    }
    // ipld-dagpb writes sorted Links before Data, and Hash/Name/Tsize in each
    // link. Rust String ordering is lexicographic UTF-8 byte ordering.
    let bytes = PbNode {
        links,
        data: Some(directory.data.clone()),
    }
    .into_bytes();
    if bytes.len() > MAX_BLOCK_BYTES {
        bail!(
            "Composer v1 directory exceeds {MAX_BLOCK_BYTES} bytes; HAMT sharding is unsupported"
        );
    }
    let size = children_size
        .checked_add(bytes.len() as u64)
        .context("UnixFS cumulative size overflow")?;
    let digest = Sha256::digest(&bytes);
    let hash = cid::multihash::Multihash::wrap(0x12, &digest).context("invalid SHA-256 digest")?;
    Ok((Cid::new_v1(0x70, hash), bytes, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cid(bytes: &[u8]) -> Cid {
        Cid::new_v1(
            0x70,
            cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
        )
    }
    fn block(data: &[u8], links: Vec<PbLink>) -> Vec<u8> {
        PbNode {
            data: Some(Bytes::copy_from_slice(data)),
            links,
        }
        .into_bytes()
    }
    fn parse(bytes: Vec<u8>) -> Result<Node> {
        decode(&cid(&bytes), bytes)
    }
    fn link(name: &str) -> PbLink {
        PbLink {
            cid: cid(b"child"),
            name: Some(name.into()),
            size: Some(5),
        }
    }
    #[test]
    fn empty_directory_has_interoperable_encoding() {
        let input = vec![0x0a, 2, 8, 1];
        let Node::Directory(directory) = parse(input.clone()).unwrap() else {
            panic!()
        };
        let (root, bytes, size) = encode(&directory).unwrap();
        assert_eq!(bytes, input);
        assert_eq!(size, 4);
        assert_eq!(root.codec(), 0x70);
        assert_eq!(root.version(), cid::Version::V1);
    }
    #[test]
    fn malformed_blocks_and_metadata_fail_closed() {
        for bytes in [
            vec![],
            vec![0x0a, 9, 8, 1],
            block(&[], vec![]),
            block(&[8, 1, 8, 1], vec![]),
            block(&[8, 1, 72, 1], vec![]),
            block(&[8, 99], vec![]),
            block(&[8, 3], vec![]),
            block(&[8, 5, 40, 0x22, 48, 0x80, 2], vec![]),
            block(&[8, 1, 24, 0], vec![]),
        ] {
            assert!(parse(bytes.clone()).is_err(), "accepted {bytes:?}");
        }
    }
    #[test]
    fn directory_rejects_missing_duplicate_or_invalid_links() {
        for links in [
            vec![link("same"), link("same")],
            vec![PbLink {
                name: None,
                ..link("x")
            }],
            vec![PbLink {
                size: None,
                ..link("x")
            }],
            vec![link("")],
            vec![link(".")],
            vec![link("..")],
            vec![link("a/b")],
            vec![link("a\n")],
        ] {
            assert!(parse(block(&[8, 1], links)).is_err());
        }
    }
    #[test]
    fn names_and_directory_metadata_are_preserved() {
        // mode=0755, mtime seconds=7, fractional nanoseconds=8.
        let metadata = [8, 1, 56, 0xed, 3, 66, 7, 8, 7, 21, 8, 0, 0, 0];
        let Node::Directory(directory) =
            parse(block(&metadata, vec![link("é\\?"), link("e\u{301}")])).unwrap()
        else {
            panic!()
        };
        assert_eq!(directory.data.as_ref(), metadata);
        assert_eq!(directory.entries.len(), 2);
        assert!(directory.entries.contains_key("é\\?"));
        let (_, encoded, _) = encode(&directory).unwrap();
        let node = PbNode::from_bytes(encoded.into()).unwrap();
        assert_eq!(node.data.unwrap().as_ref(), metadata);
        assert_eq!(node.links[0].name.as_deref(), Some("e\u{301}"));
    }
    #[test]
    fn directory_links_use_utf8_byte_order() {
        let child = Link {
            cid: cid(b"child"),
            size: 5,
        };
        let directory = Directory {
            data: Bytes::from_static(&[8, 1]),
            entries: ["\u{10000}", "z", "\u{e000}", "a"]
                .into_iter()
                .map(|name| (name.to_owned(), child.clone()))
                .collect(),
        };

        let (_, encoded, _) = encode(&directory).unwrap();
        let node = PbNode::from_bytes(encoded.into()).unwrap();
        let names: Vec<_> = node
            .links
            .iter()
            .map(|link| link.name.as_deref().unwrap())
            .collect();

        assert_eq!(names, ["a", "z", "\u{e000}", "\u{10000}"]);
    }
    #[test]
    fn cumulative_size_overflow_is_rejected() {
        assert!(parse(block(
            &[8, 1],
            vec![PbLink {
                size: Some(u64::MAX),
                ..link("x")
            }]
        ))
        .is_err());
    }
    #[test]
    fn accepts_both_standard_pbnode_field_orders() {
        let links = PbNode {
            data: None,
            links: vec![link("x")],
        }
        .into_bytes();
        let mut data_first = vec![0x0a, 2, 8, 1];
        data_first.extend(links);
        assert_eq!(
            parse(data_first).unwrap(),
            parse(block(&[8, 1], vec![link("x")])).unwrap()
        );
    }
    #[test]
    fn hash_mismatch_and_block_limit_are_rejected() {
        assert!(decode(&cid(b"wrong"), block(&[8, 1], vec![])).is_err());
        assert!(parse(vec![0; MAX_BLOCK_BYTES + 1]).is_err());
    }
    #[test]
    fn validates_file_raw_and_symlink_roots() {
        assert!(matches!(
            parse(block(&[8, 2, 18, 1, b'x', 24, 1], vec![])).unwrap(),
            Node::File { .. }
        ));
        assert!(matches!(
            parse(block(&[8, 0, 18, 1, b'x', 24, 1], vec![])).unwrap(),
            Node::Raw { .. }
        ));
        assert!(matches!(
            parse(block(&[8, 4, 18, 1, b'x'], vec![])).unwrap(),
            Node::Symlink { .. }
        ));
        let unnamed = PbLink {
            name: Some(String::new()),
            ..link("x")
        };
        assert!(matches!(
            parse(block(&[8, 2, 24, 9, 32, 9], vec![unnamed.clone()])).unwrap(),
            Node::File { .. }
        ));
        for (data, links) in [
            (vec![8, 2, 24, 8, 32, 9], vec![unnamed.clone()]),
            (vec![8, 2, 24, 9], vec![unnamed.clone()]),
            (vec![8, 4, 18, 1, 255], vec![]),
            (vec![8, 4, 18, 1, b'x'], vec![unnamed]),
        ] {
            assert!(parse(block(&data, links)).is_err());
        }
    }
    #[test]
    fn repeated_dagpb_fields_and_noncanonical_cids_are_rejected() {
        let mut duplicate_data = block(&[8, 1], vec![]);
        duplicate_data.extend([0x0a, 2, 8, 1]);
        assert!(parse(duplicate_data).is_err());

        // Hand-assemble links so the fixture encoder cannot canonicalize them.
        let canonical_cid = cid(b"child").to_bytes();
        let mut duplicate_name = vec![10, canonical_cid.len() as u8];
        duplicate_name.extend(&canonical_cid);
        duplicate_name.extend([18, 1, b'x', 18, 1, b'x', 24, 5]);
        let mut duplicate_size = vec![10, canonical_cid.len() as u8];
        duplicate_size.extend(&canonical_cid);
        duplicate_size.extend([18, 1, b'x', 24, 5, 24, 5]);
        let mut nonminimal_cid = vec![10, canonical_cid.len() as u8 + 1, 0x81, 0];
        nonminimal_cid.extend(&canonical_cid[1..]);
        nonminimal_cid.extend([18, 1, b'x', 24, 5]);
        for link in [duplicate_name, duplicate_size, nonminimal_cid] {
            let mut bytes = vec![18, link.len() as u8];
            bytes.extend(link);
            bytes.extend([10, 2, 8, 1]);
            assert!(parse(bytes).is_err());
        }
    }

    #[test]
    fn invalid_mtime_and_directory_payload_fail_closed() {
        for metadata in [
            vec![8, 1, 66, 0],                                // absent required seconds
            vec![8, 1, 66, 7, 8, 1, 21, 0, 0xca, 0x9a, 0x3b], // 1e9 ns
            vec![8, 1, 66, 4, 8, 1, 8, 1],                    // duplicate seconds
            vec![8, 1, 56, 0x80, 0x20],                       // unsupported mode bits
            vec![8, 1, 18, 0], // forbidden directory payload, even empty
            vec![8, 1, 48, 0], // forbidden fanout, even zero
        ] {
            assert!(parse(block(&metadata, vec![])).is_err());
        }
    }

    #[test]
    fn mtime_fraction_is_absent_or_between_one_and_999999999() {
        // UnixFS Directory with mtime seconds=7. Fraction uses fixed32 LE.
        for (label, metadata, accepted) in [
            ("absent", vec![8, 1, 66, 2, 8, 7], true),
            ("zero", vec![8, 1, 66, 7, 8, 7, 21, 0, 0, 0, 0], false),
            ("one", vec![8, 1, 66, 7, 8, 7, 21, 1, 0, 0, 0], true),
            (
                "maximum",
                vec![8, 1, 66, 7, 8, 7, 21, 0xff, 0xc9, 0x9a, 0x3b],
                true,
            ),
            (
                "one billion",
                vec![8, 1, 66, 7, 8, 7, 21, 0, 0xca, 0x9a, 0x3b],
                false,
            ),
        ] {
            let decoded = parse(block(&metadata, vec![]));
            assert_eq!(decoded.is_ok(), accepted, "fraction {label}: {decoded:?}");
            let directory = Directory {
                data: Bytes::from(metadata),
                entries: BTreeMap::new(),
            };
            assert_eq!(
                encode(&directory).is_ok(),
                accepted,
                "encoding fraction {label}"
            );
        }
    }

    #[test]
    fn legacy_raw_requires_explicit_filesize_matching_inline_data() {
        for (label, metadata, accepted) in [
            ("nonempty correct", vec![8, 0, 18, 1, b'x', 24, 1], true),
            ("nonempty missing", vec![8, 0, 18, 1, b'x'], false),
            ("nonempty incorrect", vec![8, 0, 18, 1, b'x', 24, 2], false),
            ("empty correct", vec![8, 0, 18, 0, 24, 0], true),
            ("omitted data correct", vec![8, 0, 24, 0], true),
            ("empty missing", vec![8, 0, 18, 0], false),
            ("empty incorrect", vec![8, 0, 18, 0, 24, 1], false),
        ] {
            let decoded = parse(block(&metadata, vec![]));
            assert_eq!(decoded.is_ok(), accepted, "Raw {label}: {decoded:?}");
            if accepted {
                assert!(matches!(decoded.unwrap(), Node::Raw { .. }), "Raw {label}");
            }
        }
    }

    #[test]
    fn new_directories_are_deterministic_and_include_child_cumulative_sizes() {
        let mut a = Directory {
            data: Bytes::from_static(&[8, 1]),
            entries: BTreeMap::new(),
        };
        let mut b = a.clone();
        for name in ["z", "a", "é", "e\u{301}"] {
            a.entries.insert(
                name.into(),
                Link {
                    cid: cid(name.as_bytes()),
                    size: 9,
                },
            );
        }
        for name in ["e\u{301}", "é", "a", "z"] {
            b.entries.insert(
                name.into(),
                Link {
                    cid: cid(name.as_bytes()),
                    size: 9,
                },
            );
        }
        let (root, bytes, size) = encode(&a).unwrap();
        assert_eq!(size, bytes.len() as u64 + 36);
        assert_eq!(encode(&b).unwrap(), (root, bytes.clone(), size));
        assert_eq!(parse(bytes).unwrap(), Node::Directory(a));
    }

    #[test]
    fn oversized_directory_and_encode_size_overflow_are_rejected() {
        let mut directory = Directory {
            data: Bytes::from_static(&[8, 1]),
            entries: BTreeMap::new(),
        };
        directory.entries.insert(
            "x".into(),
            Link {
                cid: cid(b"child"),
                size: u64::MAX,
            },
        );
        assert!(encode(&directory).is_err());
        directory.entries.clear();
        directory.entries.insert(
            "x".repeat(MAX_BLOCK_BYTES),
            Link {
                cid: cid(b"child"),
                size: 0,
            },
        );
        assert!(encode(&directory).is_err());
    }

    #[test]
    fn validates_cid_v0_and_rejects_unsupported_hashes() {
        let bytes = block(&[8, 1], vec![]);
        let v0 = Cid::new_v0(*cid(&bytes).hash()).unwrap();
        assert!(decode(&v0, bytes.clone()).is_ok());
        let identity = Cid::new_v1(0x70, cid::multihash::Multihash::wrap(0, &bytes).unwrap());
        assert!(decode(&identity, bytes).is_err());
    }

    #[test]
    fn file_size_distinguishes_logical_content_from_cumulative_dag_size() {
        let bytes = block(
            &[8, 2, 24, 9, 32, 9],
            vec![PbLink {
                name: Some(String::new()),
                ..link("x")
            }],
        );
        let expected_size = bytes.len() as u64 + 5;
        assert_eq!(
            parse(bytes).unwrap(),
            Node::File {
                file: File {
                    inline_size: 0,
                    logical_size: 9,
                    links: vec![FileLink {
                        cid: cid(b"child"),
                        size: 5,
                        logical_size: 9,
                    }],
                },
                size: expected_size,
            }
        );
    }

    #[test]
    fn directory_semantics_round_trip_and_reencode_deterministically() {
        let raw_bytes = b"raw-payload";
        let raw_cid = Cid::new_v1(
            0x55,
            cid::multihash::Multihash::wrap(0x12, &Sha256::digest(raw_bytes)).unwrap(),
        );
        let file_bytes = block(&[8, 2, 18, 1, b'f', 24, 1], vec![]);
        let file_cid = cid(&file_bytes);
        let symlink_bytes = block(
            &[
                8, 4, 18, 9, b'.', b'.', b'/', b't', b'a', b'r', b'g', b'e', b't',
            ],
            vec![],
        );
        let symlink_cid = cid(&symlink_bytes);
        let (nested_cid, nested_bytes, nested_size) = encode(&Directory {
            data: Bytes::from_static(&[8, 1, 56, 0xed, 3]),
            entries: BTreeMap::new(),
        })
        .unwrap();

        let mixed_entries = BTreeMap::from([
            (
                "file".to_owned(),
                Link {
                    cid: file_cid,
                    size: file_bytes.len() as u64,
                },
            ),
            (
                "nested".to_owned(),
                Link {
                    cid: nested_cid,
                    size: nested_size,
                },
            ),
            (
                "raw".to_owned(),
                Link {
                    cid: raw_cid,
                    size: raw_bytes.len() as u64,
                },
            ),
            (
                "symlink".to_owned(),
                Link {
                    cid: symlink_cid,
                    size: symlink_bytes.len() as u64,
                },
            ),
        ]);
        let cases = [
            Directory {
                data: Bytes::from_static(&[8, 1]),
                entries: BTreeMap::new(),
            },
            Directory {
                data: Bytes::from_static(&[8, 1]),
                entries: BTreeMap::from([(
                    "raw".to_owned(),
                    Link {
                        cid: raw_cid,
                        size: raw_bytes.len() as u64,
                    },
                )]),
            },
            Directory {
                data: Bytes::from_static(&[8, 1, 56, 0xed, 3]),
                entries: BTreeMap::new(),
            },
            Directory {
                data: Bytes::from_static(&[8, 1, 66, 2, 8, 7]),
                entries: BTreeMap::new(),
            },
            Directory {
                data: Bytes::from_static(&[8, 1, 66, 7, 8, 7, 21, 9, 0, 0, 0]),
                entries: BTreeMap::new(),
            },
            Directory {
                data: Bytes::from_static(&[8, 1, 56, 0xc0, 3, 66, 7, 8, 9, 21, 1, 0, 0, 0]),
                entries: mixed_entries,
            },
        ];

        for (case_index, directory) in cases.into_iter().enumerate() {
            let expected_children_size = directory
                .entries
                .values()
                .map(|link| link.size)
                .sum::<u64>();
            let encoded = encode(&directory).unwrap();
            for _ in 0..4 {
                assert_eq!(encode(&directory).unwrap(), encoded, "case {case_index}");
            }
            let (root, bytes, cumulative_size) = encoded;
            assert_eq!(
                cumulative_size,
                bytes.len() as u64 + expected_children_size,
                "case {case_index}"
            );
            let Node::Directory(decoded) = decode(&root, bytes.clone()).unwrap() else {
                panic!("case {case_index} did not decode as a directory")
            };
            assert_eq!(decoded, directory, "case {case_index}");
            assert_eq!(
                encode(&decoded).unwrap(),
                (root, bytes, cumulative_size),
                "case {case_index}"
            );
        }

        assert_eq!(nested_bytes.len() as u64, nested_size);
    }
}
