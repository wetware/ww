//! Validate one bounded CAR stage before any storage side effect.
//!
//! A stage may stop traversal only at roots already acknowledged by its release
//! owner. Kubo presence alone is not retention. These checks establish closure
//! of supplied bytes; the subsequent CAR acknowledgement establishes retention.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use cid::Cid;
use ipld_dagpb::PbNode;
use sha2::{Digest, Sha256};

pub(super) const MAX_STAGE_BLOCKS: usize = 16_384;
pub(super) const MAX_STAGE_LINKS: usize = 65_536;
const MAX_CAR_BYTES: usize = 64 * 1024 * 1024;
const MAX_BLOCK_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Limits {
    blocks: usize,
    links: usize,
    car_bytes: usize,
    block_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            blocks: MAX_STAGE_BLOCKS,
            links: MAX_STAGE_LINKS,
            car_bytes: MAX_CAR_BYTES,
            block_bytes: MAX_BLOCK_BYTES,
        }
    }
}

/// Validate all supplied bytes and their complete reachable closure.
///
/// Duplicate identical blocks are allowed but charged against input limits.
/// No input bytes are copied before CAR and block bounds pass. Reachability
/// stores at most 16,384 supplied identities and visits at most 65,536 links.
/// Unsupported codecs and hashes fail closed, including links to retained roots.
pub(super) fn validate_stage(
    root: &Cid,
    blocks: &[(Cid, Vec<u8>)],
    retained: &BTreeSet<Cid>,
) -> Result<()> {
    validate_with_limits(root, blocks, retained, Limits::default())
}

fn validate_with_limits(
    root: &Cid,
    blocks: &[(Cid, Vec<u8>)],
    retained: &BTreeSet<Cid>,
    limits: Limits,
) -> Result<()> {
    validate_cid(root)?;
    ensure!(
        blocks.len() <= limits.blocks,
        "release stage block count exceeded"
    );

    // Match composed::encode_car's conservative charge, including its header.
    // Only the fixed-size single-CID header is allocated before input bounds.
    #[derive(serde::Serialize)]
    struct Header<'a> {
        roots: [&'a Cid; 1],
        version: u64,
    }
    let mut car_bytes = serde_ipld_dagcbor::to_vec(&Header {
        roots: [root],
        version: 1,
    })?
    .len()
        + 10;
    for (cid, block) in blocks {
        validate_cid(cid)?;
        ensure!(
            block.len() <= limits.block_bytes,
            "release stage block exceeds limit"
        );
        car_bytes = car_bytes
            .checked_add(cid.encoded_len())
            .and_then(|size| size.checked_add(block.len()))
            .and_then(|size| size.checked_add(10))
            .context("release stage CAR size overflow")?;
        ensure!(
            car_bytes <= limits.car_bytes,
            "release stage CAR exceeds limit"
        );
    }
    ensure!(
        car_bytes <= limits.car_bytes,
        "release stage CAR exceeds limit"
    );

    let mut supplied = BTreeMap::new();
    for (cid, block) in blocks {
        ensure!(
            cid.hash().digest() == Sha256::digest(block).as_slice(),
            "release block does not match CID {cid}"
        );
        if let Some(previous) = supplied.insert(*cid, block.as_slice()) {
            ensure!(
                previous == block.as_slice(),
                "conflicting duplicate release block {cid}"
            );
        }
    }

    let mut reached = BTreeSet::new();
    let mut pending = vec![*root];
    let mut remaining_links = limits.links;
    while let Some(cid) = pending.pop() {
        if reached.contains(&cid) {
            continue;
        }
        let Some(bytes) = supplied.get(&cid) else {
            ensure!(
                retained.contains(&cid),
                "release stage missing unretained block {cid}"
            );
            continue;
        };
        reached.insert(cid);
        let links = match cid.codec() {
            0x55 => Vec::new(),
            0x70 => dag_pb_links(bytes, remaining_links)?,
            0x71 => crate::map::links(bytes)?,
            _ => unreachable!("validated codec"),
        };
        ensure!(
            links.len() <= remaining_links,
            "release stage link count exceeded"
        );
        remaining_links -= links.len();
        for link in links {
            validate_cid(&link)?;
            pending.push(link);
        }
    }
    ensure!(
        reached.len() == supplied.len(),
        "release stage contains unreachable blocks"
    );
    Ok(())
}

pub(super) fn validate_cid(cid: &Cid) -> Result<()> {
    ensure!(
        matches!(cid.codec(), 0x55 | 0x70 | 0x71),
        "unsupported release codec {}",
        cid.codec()
    );
    ensure!(
        cid.hash().code() == 0x12 && cid.hash().size() == 32,
        "release blocks require SHA-256"
    );
    Ok(())
}

fn dag_pb_links(bytes: &[u8], remaining_links: usize) -> Result<Vec<Cid>> {
    // Count the protobuf's repeated Links before the dependency allocates its
    // Vec. The complete round trip below rejects nonminimal nested fields/CIDs.
    let mut cursor = bytes;
    let mut count = 0usize;
    while let Some((&field, rest)) = cursor.split_first() {
        ensure!(field == 0x0a || field == 0x12, "unsupported DAG-PB field");
        cursor = rest;
        let length = length(&mut cursor)?;
        ensure!(length <= cursor.len(), "truncated DAG-PB field");
        if field == 0x12 {
            count += 1;
            ensure!(
                count <= remaining_links,
                "release stage link count exceeded"
            );
        }
        cursor = &cursor[length..];
    }
    let node =
        PbNode::from_bytes(Bytes::copy_from_slice(bytes)).context("invalid release DAG-PB")?;
    let canonical = node.clone().into_bytes();
    if canonical != bytes {
        // DAG-PB allows either Data/Links or Links/Data, as Composer v1 does.
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
        ensure!(
            data_first == bytes,
            "noncanonical release DAG-PB fields, links, or CIDs"
        );
    }
    Ok(node.links.into_iter().map(|link| link.cid).collect())
}

fn length(cursor: &mut &[u8]) -> Result<usize> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let (&byte, rest) = cursor.split_first().context("truncated DAG-PB length")?;
        *cursor = rest;
        ensure!(shift != 63 || byte <= 1, "overflowing DAG-PB length");
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            ensure!(shift == 0 || byte != 0, "nonminimal DAG-PB length");
            return usize::try_from(value).context("DAG-PB length does not fit memory");
        }
    }
    bail!("overflowing DAG-PB length")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cid::multihash::Multihash;
    use ipld_dagpb::PbLink;

    fn block(codec: u64, bytes: Vec<u8>) -> (Cid, Vec<u8>) {
        (
            Cid::new_v1(
                codec,
                Multihash::wrap(0x12, &Sha256::digest(&bytes)).unwrap(),
            ),
            bytes,
        )
    }

    fn branch(children: &[Cid]) -> (Cid, Vec<u8>) {
        block(
            0x70,
            PbNode {
                links: children
                    .iter()
                    .enumerate()
                    .map(|(i, cid)| PbLink {
                        cid: *cid,
                        name: Some(format!("{i:05}")),
                        size: Some(0),
                    })
                    .collect(),
                data: Some(Bytes::from_static(b"metadata")),
            }
            .into_bytes(),
        )
    }

    fn validate(root: &Cid, blocks: &[(Cid, Vec<u8>)]) -> Result<()> {
        validate_stage(root, blocks, &BTreeSet::new())
    }

    #[test]
    fn complete_shared_graph_and_duplicate_blocks() {
        let leaf = block(0x55, b"leaf".to_vec());
        let left = branch(&[leaf.0]);
        let right = branch(&[leaf.0, leaf.0]);
        let root = branch(&[left.0, right.0]);
        validate(&root.0, &[root.clone(), left, right, leaf.clone(), leaf]).unwrap();
    }

    #[test]
    fn missing_corrupt_and_unreachable_blocks_are_rejected() {
        let leaf = block(0x55, b"leaf".to_vec());
        let root = branch(&[leaf.0]);
        assert!(validate(&root.0, std::slice::from_ref(&root)).is_err());
        let mut corrupt = leaf.clone();
        corrupt.1[0] ^= 1;
        assert!(validate(&root.0, &[root.clone(), corrupt]).is_err());
        assert!(validate(&leaf.0, &[leaf.clone(), root]).is_err());
        assert!(validate(&leaf.0, &[]).is_err());
    }

    #[test]
    fn only_acknowledged_roots_can_terminate_a_stage_walk() {
        let leaf = block(0x55, b"leaf".to_vec());
        let root = branch(&[leaf.0]);
        let retained = BTreeSet::from([leaf.0]);
        validate_stage(&root.0, std::slice::from_ref(&root), &retained).unwrap();
        validate_stage(&leaf.0, &[], &retained).unwrap();
        // Supplying a retained root requires checking its supplied closure.
        assert!(validate_stage(
            &root.0,
            std::slice::from_ref(&root),
            &BTreeSet::from([root.0])
        )
        .is_err());
    }

    #[test]
    fn cbor_tag_42_links_require_their_descendants() {
        let leaf = block(0x55, b"leaf".to_vec());
        // Canonical CBOR {"a": <CID link>}.
        let mut cbor = vec![0xa1, 0x61, b'a', 0xd8, 0x2a, 0x58, 37, 0];
        cbor.extend(leaf.0.to_bytes());
        let root = block(0x71, cbor);
        assert!(validate(&root.0, std::slice::from_ref(&root)).is_err());
        validate(&root.0, &[root.clone(), leaf]).unwrap();
    }

    #[test]
    fn unsupported_codecs_hashes_and_noncanonical_pb_fail_closed() {
        let unknown = block(0x72, vec![]);
        assert!(validate(&unknown.0, std::slice::from_ref(&unknown)).is_err());
        let identity = Cid::new_v1(0x55, Multihash::wrap(0, b"leaf").unwrap());
        assert!(validate(&identity, &[(identity, b"leaf".to_vec())]).is_err());
        for malformed in [
            vec![0x0a, 0x80, 0],
            vec![0x0a, 0],
            vec![0x0a, 1, 0, 0x0a, 1, 0],
            vec![0x18, 0],
        ] {
            let bad = block(0x70, malformed);
            // A single empty Data field is canonical and valid DAG-PB.
            if bad.1 == [0x0a, 0] {
                validate(&bad.0, std::slice::from_ref(&bad)).unwrap();
            } else {
                assert!(validate(&bad.0, std::slice::from_ref(&bad)).is_err());
            }
        }
        let invalid_link = branch(&[identity]);
        assert!(validate_stage(
            &invalid_link.0,
            std::slice::from_ref(&invalid_link),
            &BTreeSet::from([identity])
        )
        .is_err());
    }

    #[test]
    fn dag_pb_accepts_both_permitted_field_orders() {
        let leaf = block(0x55, b"leaf".to_vec());
        let root = branch(&[leaf.0]);
        let node = PbNode::from_bytes(Bytes::from(root.1)).unwrap();
        let mut bytes = PbNode {
            links: vec![],
            data: node.data,
        }
        .into_bytes();
        bytes.extend(
            PbNode {
                links: node.links,
                data: None,
            }
            .into_bytes(),
        );
        let alternate = block(0x70, bytes);
        validate(&alternate.0, &[alternate.clone(), leaf]).unwrap();
    }

    #[test]
    fn block_car_count_and_edge_limits_are_inclusive() {
        let leaf = block(0x55, b"leaf".to_vec());
        let root = branch(&[leaf.0, leaf.0]);
        let blocks = [root.clone(), leaf.clone()];
        let bytes = root.0.encoded_len()
            + 32
            + blocks
                .iter()
                .map(|(cid, b)| cid.encoded_len() + b.len() + 10)
                .sum::<usize>();
        let at = Limits {
            blocks: 2,
            links: 2,
            car_bytes: bytes,
            block_bytes: root.1.len(),
        };
        validate_with_limits(&root.0, &blocks, &BTreeSet::new(), at).unwrap();
        for below in [
            Limits { blocks: 1, ..at },
            Limits { links: 1, ..at },
            Limits {
                car_bytes: bytes - 1,
                ..at
            },
            Limits {
                block_bytes: root.1.len() - 1,
                ..at
            },
        ] {
            assert!(validate_with_limits(&root.0, &blocks, &BTreeSet::new(), below).is_err());
        }
        validate_with_limits(
            &root.0,
            &blocks,
            &BTreeSet::new(),
            Limits {
                blocks: 3,
                links: 3,
                car_bytes: bytes + 1,
                block_bytes: root.1.len() + 1,
            },
        )
        .unwrap();
        assert!(validate_with_limits(
            &leaf.0,
            &[leaf.clone(), leaf.clone()],
            &BTreeSet::new(),
            Limits {
                blocks: 1,
                ..Limits::default()
            }
        )
        .is_err());
    }

    #[test]
    fn dag_pb_link_count_checked_before_decode() {
        let repeated_empty_links = [0x12, 0, 0x12, 0];
        assert!(dag_pb_links(&repeated_empty_links, 1)
            .unwrap_err()
            .to_string()
            .contains("link count"));
        for malformed in [
            vec![
                0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2,
            ],
            vec![0x0a, 0x80],
            vec![0x0a, 10],
        ] {
            assert!(dag_pb_links(&malformed, 10).is_err());
        }
    }
    #[test]
    fn dag_pb_links_reject_trailing_and_nonminimal_cid_bytes() {
        let leaf = block(0x55, b"leaf".to_vec());
        let canonical = leaf.0.to_bytes();
        let mut trailing = canonical.clone();
        trailing.push(0);
        let mut overlong = vec![0x81, 0x00];
        overlong.extend_from_slice(&canonical[1..]);
        for cid_bytes in [trailing, overlong] {
            let mut wire = vec![
                0x12,
                (cid_bytes.len() + 2) as u8,
                0x0a,
                cid_bytes.len() as u8,
            ];
            wire.extend(cid_bytes);
            let malformed = block(0x70, wire);
            assert!(validate(&malformed.0, &[malformed.clone(), leaf.clone()]).is_err());
        }
    }

    #[test]
    fn malformed_cbor_is_rejected_before_storage() {
        for bytes in [
            vec![0xa2, 0x61, b'a', 0xf6, 0x61, b'a', 0xf6], // duplicate key
            vec![0xfa, 0, 0, 0, 0],                         // float
            vec![0xd8, 0x29, 0xf6],                         // unsupported tag
            vec![0x9f, 0xff],                               // indefinite list
            vec![0x18, 0x01],                               // nonminimal integer
            vec![0xf6, 0xf6],                               // trailing data
            vec![0x81; 34],                                 // excessive depth, also truncated
        ] {
            let malformed = block(0x71, bytes);
            assert!(validate(&malformed.0, std::slice::from_ref(&malformed)).is_err());
        }
        let mut deep = vec![0x81; 34];
        deep.push(0xf6);
        let malformed = block(0x71, deep);
        assert!(validate(&malformed.0, std::slice::from_ref(&malformed)).is_err());
    }

    #[test]
    fn fixed_stage_profile_and_actual_block_limit() {
        assert_eq!(MAX_STAGE_BLOCKS, 16_384);
        assert_eq!(MAX_STAGE_LINKS, 65_536);
        assert_eq!(MAX_CAR_BYTES, 67_108_864);
        assert_eq!(MAX_BLOCK_BYTES, 2_097_152);
        let at = block(0x55, vec![0; MAX_BLOCK_BYTES]);
        validate(&at.0, std::slice::from_ref(&at)).unwrap();
        let above = block(0x55, vec![0; MAX_BLOCK_BYTES + 1]);
        assert!(validate(&above.0, std::slice::from_ref(&above)).is_err());
        let empty = block(0x55, vec![]);
        assert!(validate(&empty.0, &vec![empty.clone(); MAX_STAGE_BLOCKS + 1]).is_err());
    }
}
