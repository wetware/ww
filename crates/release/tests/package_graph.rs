#![cfg(feature = "package")]

use std::{fs, path::Path};

use cid::{multihash::Multihash, Cid};
use ipfs::map::{Map, Value};
use sha2::{Digest, Sha256};
use ww_release::package::Package;

fn store(spool: &Path, codec: u64, bytes: &[u8]) -> Cid {
    let cid = Cid::new_v1(
        codec,
        Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
    );
    fs::create_dir_all(spool.join("blocks")).unwrap();
    fs::write(spool.join("blocks").join(cid.to_string()), bytes).unwrap();
    cid
}

fn select_root(spool: &Path, root: Cid) {
    #[derive(serde::Serialize)]
    struct Index {
        version: u32,
        root: Cid,
    }
    fs::write(
        spool.join("package.json"),
        serde_json::to_vec(&Index { version: 1, root }).unwrap(),
    )
    .unwrap();
}

fn parent(spool: &Path, child: Cid) -> Cid {
    let bytes = Map::new()
        .insert("child", Value::Link(child))
        .unwrap()
        .encode()
        .unwrap();
    store(spool, 0x71, &bytes)
}

#[test]
fn content_reads_verify_hashes_and_open_requires_all_descendants() {
    let temp = tempfile::tempdir().unwrap();
    let spool = temp.path();
    let leaf = store(spool, 0x55, b"payload");
    let root = parent(spool, leaf);
    select_root(spool, root);
    // Unrelated spool data has never been part of reachable-graph validation.
    fs::write(spool.join("blocks").join("unrelated"), b"ignored").unwrap();
    let package = Package::open(spool).unwrap();
    assert_eq!(package.root(), root);
    assert_eq!(package.links(&root).unwrap(), [leaf]);
    assert_eq!(package.read_block(&leaf).unwrap(), b"payload");

    let path = spool.join("blocks").join(leaf.to_string());
    fs::write(&path, b"changed").unwrap();
    assert!(package.read_block(&leaf).is_err());
    assert!(Package::open(spool).is_err());
    fs::remove_file(path).unwrap();
    assert!(Package::open(spool).is_err());
}

#[test]
fn package_cbor_requires_map_envelopes_even_when_inline_profile_accepts_value() {
    let temp = tempfile::tempdir().unwrap();
    for bytes in [&[0xf6][..], &[0x82, 2, 0xa0]] {
        assert!(ipfs::map::inline_v1::links(bytes).is_ok());
        select_root(temp.path(), store(temp.path(), 0x71, bytes));
        assert!(Package::open(temp.path()).is_err());
    }
    let bytes = Map::new().encode().unwrap();
    select_root(temp.path(), store(temp.path(), 0x71, &bytes));
    assert!(Package::open(temp.path()).is_ok());
}

#[test]
fn unsupported_codec_hash_and_oversized_blocks_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    select_root(
        temp.path(),
        store(temp.path(), 0x55, &vec![0; 2 * 1024 * 1024]),
    );
    assert!(Package::open(temp.path()).is_ok());
    for (codec, bytes) in [(0x99, vec![]), (0x55, vec![0; 2 * 1024 * 1024 + 1])] {
        select_root(temp.path(), store(temp.path(), codec, &bytes));
        assert!(Package::open(temp.path()).is_err());
    }
    let identity = Cid::new_v1(0x55, Multihash::wrap(0, b"payload").unwrap());
    fs::write(
        temp.path().join("blocks").join(identity.to_string()),
        b"payload",
    )
    .unwrap();
    select_root(temp.path(), identity);
    assert!(Package::open(temp.path()).is_err());
}

#[test]
fn package_requires_canonical_dag_pb_encoding() {
    let temp = tempfile::tempdir().unwrap();
    let leaf = store(temp.path(), 0x55, b"payload");
    // One DAG-PB link: Hash=leaf, Name="a", Tsize=7; directory Data=[8, 1].
    let mut entry = vec![0x0a, 36];
    entry.extend_from_slice(&leaf.to_bytes());
    entry.extend_from_slice(&[0x12, 1, b'a', 0x18, 7]);
    let mut canonical = vec![0x12, entry.len() as u8];
    canonical.extend_from_slice(&entry);
    canonical.extend_from_slice(&[0x0a, 2, 8, 1]);
    let root = store(temp.path(), 0x70, &canonical);
    select_root(temp.path(), root);
    assert_eq!(
        Package::open(temp.path()).unwrap().links(&root).unwrap(),
        [leaf]
    );

    let mut noncanonical = vec![0x0a, 2, 8, 1];
    noncanonical.extend_from_slice(&canonical[..canonical.len() - 4]);
    select_root(temp.path(), store(temp.path(), 0x70, &noncanonical));
    assert!(Package::open(temp.path()).is_err());
}

#[test]
fn depth_limit_includes_shared_descendants_reached_first_by_a_shorter_path() {
    let temp = tempfile::tempdir().unwrap();
    let leaf = store(temp.path(), 0x55, b"payload");
    let shared = parent(temp.path(), leaf);
    let mut chain = shared;
    for _ in 0..62 {
        chain = parent(temp.path(), chain);
    }
    // The shared node is traversed first at depth one; its second path reaches
    // the leaf exactly at depth 64. One more link must fail even with caching.
    let make_root = |chain| {
        let bytes = Map::new()
            .insert("a", Value::Link(shared))
            .unwrap()
            .insert("b", Value::Link(chain))
            .unwrap()
            .encode()
            .unwrap();
        store(temp.path(), 0x71, &bytes)
    };
    select_root(temp.path(), make_root(chain));
    assert!(Package::open(temp.path()).is_ok());
    chain = parent(temp.path(), chain);
    select_root(temp.path(), make_root(chain));
    assert!(Package::open(temp.path()).is_err());
}
