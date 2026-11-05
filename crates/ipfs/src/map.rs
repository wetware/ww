//! A bounded, immutable, inline IPLD Map, initially used for release metadata.
//!
//! The entire Map encodes into one canonical DAG-CBOR block. Version 1 encodes
//! `[1, {entries...}]`. Nested maps and lists use ordinary Rust [`BTreeMap`]
//! and [`Vec`] values. Updates clone the bounded representation; there is no
//! structural sharing or lazy traversal.
//!
//! Keys are exact UTF-8 bytes: no normalization or locale comparison. The
//! envelope does not count toward collection or depth limits. Depth is the
//! number of edges from the root map to a value (root depth zero). Canonical
//! DAG-CBOR orders string keys by encoded length, then encoded bytes.
//!
//! Logical operations never contact storage. Persistence is explicit and
//! separate from retention. Persisting a Map does not acquire retention, and
//! dropping a Map neither acquires nor releases pins.
//! This inline format does not promise CID equality with future tree formats.
//!
//! [`Value`] defines a versioned inline storage profile, not a universal
//! Wetware value model. This module is not the definitive persistent-collection
//! API explored in [#698](https://github.com/wetware/ww/issues/698).
//! Persistent Maps, bit-partitioned vector tries, and other IPLD-backed
//! collections may need different value types, representations, APIs, and
//! lifetime contracts. That work must evaluate reuse or evolution of this
//! inline implementation before introducing an independent Map. The current
//! profile does not select those future designs.

use std::collections::BTreeMap;

use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use cid::{multihash::Multihash, Cid, Version};
use sha2::{Digest, Sha256};

use crate::cid_identity::decode_cid;

pub const MAX_BLOCK_BYTES: usize = 1024 * 1024;
pub const MAX_DEPTH: usize = 32;
pub const MAX_MAP_ENTRIES: usize = 4096;
pub const MAX_LIST_ELEMENTS: usize = 16384;
pub const MAX_KEY_BYTES: usize = 1024;
pub const FORMAT_VERSION: u64 = 1;

/// Version 1 values. Rust ownership excludes cycles and live capabilities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Text(String),
    Bytes(Vec<u8>),
    Link(Cid),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

/// An immutable map whose complete encoding satisfies the version 1 bounds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Map {
    entries: BTreeMap<String, Value>,
}

/// Explicit block persistence, without retention or cleanup operations.
///
/// `get` implementations must bound acquisition before allocating more than
/// `max_bytes`. Map also checks the returned bytes and verifies their CID.
/// `put` must store the exact bytes under the supplied CID or return an error.
#[async_trait]
pub trait Blockstore: Send + Sync {
    async fn get(&self, cid: &Cid, max_bytes: usize) -> Result<Vec<u8>>;
    async fn put(&self, cid: &Cid, bytes: &[u8]) -> Result<()>;
}

impl Map {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reject duplicate keys rather than silently choosing a value.
    pub fn from_entries(entries: impl IntoIterator<Item = (String, Value)>) -> Result<Self> {
        let mut result = Self::new();
        let mut encoder = Encoder::default();
        for (key, value) in entries {
            encoder.budget.map(1)?;
            ensure!(key.len() <= MAX_KEY_BYTES, "map key exceeds byte limit");
            ensure!(!result.entries.contains_key(&key), "duplicate map key");
            // Account for each item before storing it; an unbounded iterator
            // cannot accumulate an unbounded intermediate map.
            encoder.text(&key)?;
            encoder.value(&value, 1)?;
            result.entries.insert(key, value);
        }
        result.encode()?;
        Ok(result)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.get(key)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate in UTF-8 lexical order. At most 4096 entries can be returned.
    /// Canonical wire order is a separate concern handled by `encode`.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &Value)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    pub fn insert(&self, key: impl Into<String>, value: Value) -> Result<Self> {
        let key = key.into();
        ensure!(key.len() <= MAX_KEY_BYTES, "map key exceeds byte limit");
        let mut result = self.clone();
        result.entries.insert(key, value);
        result.encode()?;
        Ok(result)
    }

    pub fn remove(&self, key: &str) -> Result<Self> {
        let mut result = self.clone();
        result.entries.remove(key);
        Ok(result)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = Encoder::default();
        encoder.head(4, 2)?;
        encoder.head(0, FORMAT_VERSION)?;
        encoder.map(&self.entries, 0)?;
        Ok(encoder.bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(bytes)?;
        ensure!(
            decoder.head()? == (4, 2),
            "expected Map envelope [version, entries]"
        );
        ensure!(
            decoder.head()? == (0, FORMAT_VERSION),
            "unknown Map format version"
        );
        let Value::Map(entries) = decoder.value(0)? else {
            bail!("Map envelope entries must be a map");
        };
        ensure!(decoder.bytes.is_empty(), "trailing Map data");
        Ok(Self { entries })
    }

    pub fn cid(&self) -> Result<Cid> {
        Ok(block_cid(&self.encode()?))
    }

    pub async fn persist(&self, store: &(impl Blockstore + ?Sized)) -> Result<Cid> {
        let bytes = self.encode()?;
        let cid = block_cid(&bytes);
        store.put(&cid, &bytes).await?;
        Ok(cid)
    }

    pub async fn load(store: &(impl Blockstore + ?Sized), cid: &Cid) -> Result<Self> {
        ensure!(
            cid.version() == Version::V1 && cid.codec() == 0x71,
            "Map requires CIDv1 DAG-CBOR"
        );
        ensure!(
            cid.hash().code() == 0x12 && cid.hash().size() == 32,
            "Map requires SHA-256"
        );
        let bytes = store.get(cid, MAX_BLOCK_BYTES).await?;
        ensure!(
            bytes.len() <= MAX_BLOCK_BYTES,
            "Map block exceeds byte limit"
        );
        ensure!(block_cid(&bytes) == *cid, "Map block does not match CID");
        Self::decode(&bytes)
    }
}

fn block_cid(bytes: &[u8]) -> Cid {
    Cid::new_v1(
        0x71,
        Multihash::wrap(0x12, &Sha256::digest(bytes)).expect("SHA-256 is 32 bytes"),
    )
}

#[derive(Default)]
struct Budget {
    maps: usize,
    lists: usize,
}

impl Budget {
    fn map(&mut self, count: usize) -> Result<()> {
        ensure!(
            count <= MAX_MAP_ENTRIES - self.maps,
            "too many total map entries"
        );
        self.maps += count;
        Ok(())
    }

    fn list(&mut self, count: usize) -> Result<()> {
        ensure!(
            count <= MAX_LIST_ELEMENTS - self.lists,
            "too many total list elements"
        );
        self.lists += count;
        Ok(())
    }
}

#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
    budget: Budget,
}

impl Encoder {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= MAX_BLOCK_BYTES - self.bytes.len(),
            "Map block exceeds byte limit"
        );
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn head(&mut self, major: u8, value: u64) -> Result<()> {
        match value {
            0..=23 => self.write(&[(major << 5) | value as u8]),
            24..=0xff => self.write(&[(major << 5) | 24, value as u8]),
            0x100..=0xffff => {
                self.write(&[(major << 5) | 25])?;
                self.write(&(value as u16).to_be_bytes())
            }
            0x10000..=0xffffffff => {
                self.write(&[(major << 5) | 26])?;
                self.write(&(value as u32).to_be_bytes())
            }
            _ => {
                self.write(&[(major << 5) | 27])?;
                self.write(&value.to_be_bytes())
            }
        }
    }

    fn text(&mut self, value: &str) -> Result<()> {
        self.head(3, value.len() as u64)?;
        self.write(value.as_bytes())
    }

    fn map(&mut self, entries: &BTreeMap<String, Value>, depth: usize) -> Result<()> {
        self.budget.map(entries.len())?;
        self.head(5, entries.len() as u64)?;
        let mut ordered: Vec<_> = entries.iter().collect();
        ordered.sort_unstable_by(|(a, _), (b, _)| canonical_key_cmp(a, b));
        for (key, value) in ordered {
            ensure!(key.len() <= MAX_KEY_BYTES, "map key exceeds byte limit");
            self.text(key)?;
            self.value(value, depth + 1)?;
        }
        Ok(())
    }

    fn value(&mut self, value: &Value, depth: usize) -> Result<()> {
        ensure!(depth <= MAX_DEPTH, "Map nesting exceeds depth limit");
        match value {
            Value::Null => self.write(&[0xf6]),
            Value::Bool(value) => self.write(&[if *value { 0xf5 } else { 0xf4 }]),
            Value::Integer(value) if *value >= 0 => self.head(0, *value as u64),
            Value::Integer(value) => self.head(1, (-1 - *value) as u64),
            Value::Text(value) => self.text(value),
            Value::Bytes(value) => {
                self.head(2, value.len() as u64)?;
                self.write(value)
            }
            Value::Link(value) => {
                self.head(6, 42)?;
                let bytes = value.to_bytes();
                // CID objects may come from permissive upstream constructors.
                decode_cid(&bytes)?;
                self.head(2, (bytes.len() + 1) as u64)?;
                self.write(&[0])?;
                self.write(&bytes)
            }
            Value::List(values) => {
                self.budget.list(values.len())?;
                self.head(4, values.len() as u64)?;
                for value in values {
                    self.value(value, depth + 1)?;
                }
                Ok(())
            }
            Value::Map(entries) => self.map(entries, depth),
        }
    }
}

fn canonical_key_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

struct Decoder<'a> {
    bytes: &'a [u8],
    budget: Budget,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_BLOCK_BYTES,
            "Map block exceeds byte limit"
        );
        Ok(Self {
            bytes,
            budget: Budget::default(),
        })
    }

    fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        ensure!(size <= self.bytes.len(), "truncated DAG-CBOR");
        let (value, rest) = self.bytes.split_at(size);
        self.bytes = rest;
        Ok(value)
    }

    fn head(&mut self) -> Result<(u8, u64)> {
        let first = self.take(1)?[0];
        let additional = first & 31;
        let value = match additional {
            0..=23 => additional as u64,
            24..=27 => {
                let width = 1 << (additional - 24);
                let bytes = self.take(width)?;
                let mut value = 0u64;
                for byte in bytes {
                    value = (value << 8) | u64::from(*byte);
                }
                let minimum = match additional {
                    24 => 24,
                    25 => 256,
                    26 => 65536,
                    _ => 1u64 << 32,
                };
                ensure!(value >= minimum, "nonminimal DAG-CBOR encoding");
                value
            }
            _ => bail!("indefinite or reserved DAG-CBOR encoding"),
        };
        Ok((first >> 5, value))
    }

    fn string(&mut self, length: u64) -> Result<&'a str> {
        let length = usize::try_from(length).context("DAG-CBOR length overflow")?;
        std::str::from_utf8(self.take(length)?).context("invalid UTF-8")
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        ensure!(depth <= MAX_DEPTH, "Map nesting exceeds depth limit");
        let (major, argument) = self.head()?;
        match (major, argument) {
            (0, value) => Ok(Value::Integer(
                i64::try_from(value).context("integer exceeds i64")?,
            )),
            (1, value) => Ok(Value::Integer(
                -1 - i64::try_from(value).context("integer below i64")?,
            )),
            (2, length) => {
                let length = usize::try_from(length).context("DAG-CBOR length overflow")?;
                Ok(Value::Bytes(self.take(length)?.to_vec()))
            }
            (3, length) => Ok(Value::Text(self.string(length)?.to_owned())),
            (4, length) => {
                let count = usize::try_from(length).context("DAG-CBOR length overflow")?;
                self.budget.list(count)?;
                ensure!(count <= self.bytes.len(), "truncated list");
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    values.push(self.value(depth + 1)?);
                }
                Ok(Value::List(values))
            }
            (5, length) => {
                let count = usize::try_from(length).context("DAG-CBOR length overflow")?;
                self.budget.map(count)?;
                ensure!(count <= self.bytes.len() / 2, "truncated map");
                let mut entries = BTreeMap::new();
                let mut previous: Option<&str> = None;
                for _ in 0..count {
                    let (major, length) = self.head()?;
                    ensure!(major == 3, "map key must be text");
                    ensure!(length <= MAX_KEY_BYTES as u64, "map key exceeds byte limit");
                    let key = self.string(length)?;
                    if let Some(previous) = previous {
                        ensure!(
                            canonical_key_cmp(previous, key).is_lt(),
                            "duplicate or noncanonical map key"
                        );
                    }
                    previous = Some(key);
                    entries.insert(key.to_owned(), self.value(depth + 1)?);
                }
                Ok(Value::Map(entries))
            }
            (6, 42) => {
                let (major, length) = self.head()?;
                ensure!(major == 2, "CID link requires bytes");
                let length = usize::try_from(length).context("CID length overflow")?;
                let bytes = self.take(length)?;
                ensure!(
                    bytes.first() == Some(&0),
                    "CID link requires identity prefix"
                );
                Ok(Value::Link(decode_cid(&bytes[1..])?))
            }
            (7, 20) => Ok(Value::Bool(false)),
            (7, 21) => Ok(Value::Bool(true)),
            (7, 22) => Ok(Value::Null),
            _ => bail!("unsupported DAG-CBOR value, tag, or float"),
        }
    }
}

/// Strict profile link traversal for release-stage validation. Accepts Map
/// envelopes with Map budgets, or standalone profile values with root depth 0.
pub(crate) fn links(bytes: &[u8]) -> Result<Vec<Cid>> {
    fn visit(value: &Value, links: &mut Vec<Cid>) {
        match value {
            Value::Link(cid) => links.push(*cid),
            Value::List(values) => values.iter().for_each(|value| visit(value, links)),
            Value::Map(values) => values.values().for_each(|value| visit(value, links)),
            _ => {}
        }
    }
    let value = match Map::decode(bytes) {
        Ok(map) => Value::Map(map.entries),
        Err(_) => {
            let mut decoder = Decoder::new(bytes)?;
            let value = decoder.value(0)?;
            ensure!(decoder.bytes.is_empty(), "trailing DAG-CBOR data");
            value
        }
    };
    let mut links = Vec::new();
    visit(&value, &mut links);
    Ok(links)
}
