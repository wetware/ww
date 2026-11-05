use ipfs::map::{Map, Value};

#[test]
fn immutable_canonical_map_fixture() {
    let empty = Map::new();
    let first = empty.insert("zz", Value::Integer(-1)).unwrap();
    let second = first.insert("a", Value::Bool(true)).unwrap();
    assert!(empty.get("zz").is_none());
    assert!(first.get("a").is_none());
    assert_eq!(
        second.encode().unwrap(),
        [0x82, 1, 0xa2, 0x61, b'a', 0xf5, 0x62, b'z', b'z', 0x20]
    );
    assert_eq!(Map::decode(&second.encode().unwrap()).unwrap(), second);
    assert_eq!(second.remove("a").unwrap(), first);
}

use anyhow::{bail, Result};
use async_trait::async_trait;
use cid::{multihash::Multihash, Cid};
use ipfs::map::{
    Blockstore, FORMAT_VERSION, MAX_BLOCK_BYTES, MAX_DEPTH, MAX_KEY_BYTES, MAX_LIST_ELEMENTS,
    MAX_MAP_ENTRIES,
};
use std::{collections::BTreeMap, sync::Mutex};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn golden_bytes_cids_and_profile() {
    let empty = Map::new();
    let ordered = empty
        .insert("zz", Value::Integer(-1))
        .unwrap()
        .insert("a", Value::Bool(true))
        .unwrap();
    for (map, bytes, cid) in [
        (
            &empty,
            include_str!("fixtures/map/empty.hex"),
            include_str!("fixtures/map/empty.cid"),
        ),
        (
            &ordered,
            include_str!("fixtures/map/ordered.hex"),
            include_str!("fixtures/map/ordered.cid"),
        ),
    ] {
        assert_eq!(hex(&map.encode().unwrap()), bytes.trim());
        assert_eq!(map.cid().unwrap().to_string(), cid.trim());
    }
    let profile: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/map/profile.json")).unwrap();
    assert_eq!(profile["version"], FORMAT_VERSION);
    for (name, value) in [
        ("max_block_bytes", MAX_BLOCK_BYTES),
        ("max_depth", MAX_DEPTH),
        ("max_map_entries", MAX_MAP_ENTRIES),
        ("max_list_elements", MAX_LIST_ELEMENTS),
        ("max_key_bytes", MAX_KEY_BYTES),
    ] {
        assert_eq!(profile[name], value);
    }
}

#[test]
fn insertion_order_does_not_change_cid_and_keys_are_exact_utf8() {
    let entries = [
        ("aa", 1),
        ("b", 2),
        ("é", 3),
        ("e\u{301}", 4),
        ("", 5),
        ("z", 6),
    ];
    let mut forward = Map::new();
    let mut reverse = Map::new();
    for (key, value) in entries {
        forward = forward.insert(key, Value::Integer(value)).unwrap();
    }
    for (key, value) in entries.into_iter().rev() {
        reverse = reverse.insert(key, Value::Integer(value)).unwrap();
    }
    assert_eq!(forward.cid().unwrap(), reverse.cid().unwrap());
    assert_eq!(forward, reverse);
    assert_eq!(forward.get("é"), Some(&Value::Integer(3)));
    assert_eq!(forward.get("e\u{301}"), Some(&Value::Integer(4)));
    assert_eq!(forward.iter().len(), entries.len());
    let changed = forward.insert("aa", Value::Null).unwrap();
    assert_eq!(forward.get("aa"), Some(&Value::Integer(1)));
    assert_eq!(changed.get("aa"), Some(&Value::Null));
    assert_eq!(changed.remove("absent").unwrap(), changed);
}

#[test]
fn nested_values_links_and_signed_integer_edges_roundtrip() {
    let cid = Cid::new_v1(0x55, Multihash::wrap(0x12, &[7; 32]).unwrap());
    let values = vec![
        Value::Null,
        Value::Bool(false),
        Value::Bool(true),
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Integer(-25),
        Value::Integer(24),
        Value::Text("🦀".into()),
        Value::Bytes(vec![0, 0xff]),
        Value::Link(cid),
        Value::Map(BTreeMap::from([("nested".into(), Value::List(vec![]))])),
    ];
    let map = Map::new().insert("values", Value::List(values)).unwrap();
    let bytes = map.encode().unwrap();
    assert!(bytes.windows(4).any(|bytes| bytes == [0xd8, 42, 0x58, 37]));
    assert_eq!(hex(&bytes), include_str!("fixtures/map/nested.hex").trim());
    assert_eq!(
        map.cid().unwrap().to_string(),
        include_str!("fixtures/map/nested.cid").trim()
    );
    assert_eq!(Map::decode(&bytes).unwrap(), map);
}

fn envelope_value(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = vec![0x82, 1, 0xa1, 0x60];
    encoded.extend_from_slice(bytes);
    encoded
}

#[test]
fn malformed_values_are_rejected() {
    let malformed: &[&[u8]] = &[
        &[0x18, 0], // Nonminimal integer.
        &[0x19, 0, 24],
        &[0x1b, 0, 0, 0, 0, 0, 0, 1, 0],
        &[0x1b, 0x80, 0, 0, 0, 0, 0, 0, 0], // Outside signed i64.
        &[0x3b, 0x80, 0, 0, 0, 0, 0, 0, 0],
        &[0x58, 0], // Nonminimal bytes length.
        &[0x78, 0],
        &[0x98, 0],
        &[0xb8, 0],
        &[0x61, 0xff], // Invalid UTF8.
        &[0xf7],       // Undefined.
        &[0xf8, 20],   // Nonminimal simple value.
        &[0xf9, 0, 0], // Half float.
        &[0xfa, 0x3f, 0x80, 0, 0],
        &[0xfb, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0],
        &[0xff],
        &[0x9f, 0xff], // Indefinite lists, maps, strings.
        &[0xbf, 0xff],
        &[0x5f, 0xff],
        &[0x7f, 0xff],
        &[0xc0, 0xf6], // Unsupported tag.
        &[0xd8, 43, 0x40],
        &[0xd8, 42, 0x60], // Link must wrap byte string with zero prefix.
        &[0xd8, 42, 0x40],
        &[0xd8, 42, 0x41, 0],
        &[0xd8, 42, 0x41, 1],
        &[0xa1, 1, 0],                               // Non-string key.
        &[0xa2, 0x61, b'a', 0, 0x61, b'a', 1],       // Duplicate.
        &[0xa2, 0x62, b'a', b'a', 0, 0x61, b'b', 1], // Lexical but not encoded-key order.
        &[0xa2, 0x61, b'b', 0, 0x61, b'a', 1],
        &[0x5b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], // Claimed allocations.
        &[0x7b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0xbb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    ];
    for bytes in malformed {
        assert!(
            Map::decode(&envelope_value(bytes)).is_err(),
            "accepted {bytes:02x?}"
        );
    }
}

#[test]
fn cid_links_reject_trailing_noncanonical_and_truncated_cids() {
    let cid = Cid::new_v1(0x55, Multihash::wrap(0x12, &[7; 32]).unwrap());
    let canonical = cid.to_bytes();
    let mut trailing = canonical.clone();
    trailing.push(0);
    let mut nonminimal = canonical.clone();
    nonminimal.splice(0..1, [0x81, 0]);
    let mut overflowing = canonical.clone();
    overflowing.splice(
        0..1,
        [0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02],
    );
    for bytes in [
        trailing,
        nonminimal,
        overflowing,
        canonical[..canonical.len() - 1].to_vec(),
    ] {
        let mut link = vec![0xd8, 42, 0x58, (bytes.len() + 1) as u8, 0];
        link.extend(bytes);
        assert!(Map::decode(&envelope_value(&link)).is_err());
    }
}

#[test]
fn malformed_envelopes_trailing_data_and_truncation_are_rejected() {
    for bytes in [
        &[][..],
        &[0x82, 2, 0xa0],
        &[0x82, 0, 0xa0],
        &[0x82, 1, 0x80],
        &[0x82, 1, 0xa0, 0],
        &[0x83, 1, 0xa0, 0],
        &[0xa0],
        &[0x82, 0x18, 1, 0xa0],
    ] {
        assert!(Map::decode(bytes).is_err(), "accepted {bytes:02x?}");
    }
    let bytes = Map::new()
        .insert("a", Value::Text("test".into()))
        .unwrap()
        .encode()
        .unwrap();
    for length in 0..bytes.len() {
        assert!(Map::decode(&bytes[..length]).is_err());
    }
}

#[test]
fn key_and_block_byte_boundaries() {
    let map = Map::new()
        .insert("x".repeat(MAX_KEY_BYTES), Value::Null)
        .unwrap();
    assert_eq!(Map::decode(&map.encode().unwrap()).unwrap(), map);
    assert!(Map::new()
        .insert("x".repeat(MAX_KEY_BYTES + 1), Value::Null)
        .is_err());
    let exact = Map::new()
        .insert("", Value::Bytes(vec![0; MAX_BLOCK_BYTES - 9]))
        .unwrap();
    assert_eq!(exact.encode().unwrap().len(), MAX_BLOCK_BYTES);
    assert_eq!(Map::decode(&exact.encode().unwrap()).unwrap(), exact);
    assert!(Map::new()
        .insert("", Value::Bytes(vec![0; MAX_BLOCK_BYTES - 8]))
        .is_err());
    assert!(Map::decode(&vec![0; MAX_BLOCK_BYTES + 1]).is_err());
    let mut long_key = vec![0x82, 1, 0xa1, 0x79, 4, 1];
    long_key.extend(vec![b'x'; 1025]);
    long_key.push(0xf6);
    assert!(Map::decode(&long_key).is_err());
}

fn nested_list(count: usize) -> Value {
    let mut value = Value::Null;
    for _ in 0..count {
        value = Value::List(vec![value]);
    }
    value
}

#[test]
fn depth_and_total_collection_boundaries() {
    let exact = Map::new().insert("", nested_list(MAX_DEPTH - 1)).unwrap();
    assert_eq!(Map::decode(&exact.encode().unwrap()).unwrap(), exact);
    assert!(Map::new().insert("", nested_list(MAX_DEPTH)).is_err());
    let mut deep_bytes = vec![0x82, 1, 0xa1, 0x60];
    deep_bytes.extend(vec![0x81; MAX_DEPTH]);
    deep_bytes.push(0xf6);
    assert!(Map::decode(&deep_bytes).is_err());
    let list = Map::new()
        .insert("", Value::List(vec![Value::Null; MAX_LIST_ELEMENTS]))
        .unwrap();
    assert_eq!(Map::decode(&list.encode().unwrap()).unwrap(), list);
    assert!(list
        .insert("extra", Value::List(vec![Value::Null]))
        .is_err());
    let entries = (0..MAX_MAP_ENTRIES - 1)
        .map(|n| (n.to_string(), Value::Null))
        .collect();
    let maps = Map::new().insert("", Value::Map(entries)).unwrap();
    assert_eq!(Map::decode(&maps.encode().unwrap()).unwrap(), maps);
    assert!(maps.insert("extra", Value::Null).is_err());
    let mut oversized_list = vec![0x82, 1, 0xa1, 0x60, 0x99, 0x40, 1];
    oversized_list.extend(vec![0xf6; MAX_LIST_ELEMENTS + 1]);
    assert!(Map::decode(&oversized_list).is_err());
    let mut oversized_map = vec![0x82, 1, 0xb9, 0x10, 1];
    oversized_map.extend(vec![0; 2 * (MAX_MAP_ENTRIES + 1)]);
    assert!(Map::decode(&oversized_map).is_err());
}

#[test]
fn input_iterator_is_bounded_and_duplicates_fail() {
    assert!(
        Map::from_entries([("a".into(), Value::Null), ("a".into(), Value::Bool(true))]).is_err()
    );
    let mut seen = 0usize;
    let result = Map::from_entries(std::iter::from_fn(|| {
        seen += 1;
        Some((seen.to_string(), Value::Null))
    }));
    assert!(result.is_err());
    assert_eq!(seen, MAX_MAP_ENTRIES + 1);
}

#[derive(Default)]
struct MemoryStore {
    bytes: Mutex<BTreeMap<Cid, Vec<u8>>>,
    operations: Mutex<Vec<&'static str>>,
    corrupt: bool,
    fail: bool,
}

#[async_trait]
impl Blockstore for MemoryStore {
    async fn get(&self, cid: &Cid, max_bytes: usize) -> Result<Vec<u8>> {
        self.operations.lock().unwrap().push("get");
        assert_eq!(max_bytes, MAX_BLOCK_BYTES);
        if self.fail {
            bail!("read failed");
        }
        if self.corrupt {
            return Ok(vec![0]);
        }
        let bytes = self.bytes.lock().unwrap().get(cid).cloned().unwrap();
        assert!(bytes.len() <= max_bytes);
        Ok(bytes)
    }
    async fn put(&self, cid: &Cid, bytes: &[u8]) -> Result<()> {
        self.operations.lock().unwrap().push("put");
        if self.fail {
            bail!("write failed");
        }
        self.bytes.lock().unwrap().insert(*cid, bytes.to_vec());
        Ok(())
    }
}

#[tokio::test]
async fn explicit_persistence_roundtrip_and_no_hidden_storage_effects() {
    let store = MemoryStore::default();
    let map = Map::new().insert("key", Value::Integer(2)).unwrap();
    drop(map.clone().remove("key").unwrap());
    assert!(store.operations.lock().unwrap().is_empty());
    let cid = map.persist(&store).await.unwrap();
    assert_eq!(Map::load(&store, &cid).await.unwrap(), map);
    drop(map);
    assert_eq!(*store.operations.lock().unwrap(), ["put", "get"]);
}

#[tokio::test]
async fn storage_failures_corruption_and_wrong_identity_fail_closed() {
    let map = Map::new();
    let failed = MemoryStore {
        fail: true,
        ..Default::default()
    };
    assert!(map.persist(&failed).await.is_err());
    assert!(Map::load(&failed, &map.cid().unwrap()).await.is_err());
    let corrupt = MemoryStore {
        corrupt: true,
        ..Default::default()
    };
    assert!(Map::load(&corrupt, &map.cid().unwrap()).await.is_err());
    let store = MemoryStore::default();
    for cid in [
        Cid::new_v1(0x55, *map.cid().unwrap().hash()),
        Cid::new_v1(0x71, Multihash::wrap(0, &[1]).unwrap()),
    ] {
        assert!(Map::load(&store, &cid).await.is_err());
    }
    assert!(store.operations.lock().unwrap().is_empty());
}
