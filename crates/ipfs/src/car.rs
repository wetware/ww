//! Bounded, single-root CARv1 framing over synchronous streams.

use std::io::{self, Read, Write};

use anyhow::{ensure, Context, Result};
use cid::Cid;
use serde::{Deserialize, Serialize};

const MAX_HEADER_BYTES: usize = 1024;
// A Cid<64> uses at most 64 digest bytes plus its canonical integer fields.
const MAX_CID_BYTES: usize = 96;

/// Bounds for one CAR stream. `max_total_bytes` counts all encoded wire bytes.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_header_bytes: usize,
    pub max_block_bytes: usize,
    pub max_blocks: usize,
    pub max_total_bytes: u64,
}

impl Limits {
    fn validate(self) -> Result<()> {
        ensure!(
            (1..=MAX_HEADER_BYTES).contains(&self.max_header_bytes),
            "CAR header limit must be between 1 and {MAX_HEADER_BYTES}"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    // Fixed cardinality: an advertised array length never controls allocation.
    roots: [Cid; 1],
    version: u64,
}

/// Read a single-root CARv1 stream, rejecting noncanonical framing.
///
/// Exhaust [`Self::next_block`] to validate the end of the stream. Empty CARs
/// are permitted for retention reconciliation. This codec does not verify
/// block hashes, graph closure, duplicate blocks, or release authority.
pub struct Reader<R> {
    input: R,
    limits: Limits,
    root: Cid,
    total: u64,
    blocks: usize,
    failed: bool,
    done: bool,
}

impl<R: Read> Reader<R> {
    pub fn new(mut input: R, limits: Limits) -> Result<Self> {
        limits.validate()?;
        let mut total = 0;
        let length = read_varint(&mut input, &mut total, limits.max_total_bytes)?
            .context("missing CAR header")?;
        ensure!(
            length <= limits.max_header_bytes as u64,
            "CAR header exceeds limit"
        );
        let header_bytes = read_section(&mut input, length, &mut total, limits.max_total_bytes)?;
        let header: Header = serde_ipld_dagcbor::from_slice(&header_bytes)
            .context("invalid single-root CAR header")?;
        ensure!(header.version == 1, "unsupported CAR version");
        ensure!(
            header.roots[0].encoded_len() <= MAX_CID_BYTES,
            "CAR root CID exceeds limit"
        );
        ensure!(
            serde_ipld_dagcbor::to_vec(&header)? == header_bytes,
            "noncanonical CAR header"
        );
        Ok(Self {
            input,
            limits,
            root: header.roots[0],
            total,
            blocks: 0,
            failed: false,
            done: false,
        })
    }

    pub fn root(&self) -> Cid {
        self.root
    }

    /// Return the next bounded block, or `None` only at a clean record boundary.
    /// After an error the reader refuses further parsing.
    pub fn next_block(&mut self) -> Result<Option<(Cid, Vec<u8>)>> {
        ensure!(!self.failed, "CAR reader cannot resume after an error");
        if self.done {
            return Ok(None);
        }
        self.failed = true;
        let result = self.read_block()?;
        self.failed = false;
        self.done = result.is_none();
        Ok(result)
    }

    fn read_block(&mut self) -> Result<Option<(Cid, Vec<u8>)>> {
        let Some(length) = read_varint(
            &mut self.input,
            &mut self.total,
            self.limits.max_total_bytes,
        )?
        else {
            return Ok(None);
        };
        ensure!(
            self.blocks < self.limits.max_blocks,
            "CAR block count exceeds limit"
        );
        let section_limit = (self.limits.max_block_bytes as u64)
            .checked_add(MAX_CID_BYTES as u64)
            .context("CAR section limit overflow")?;
        ensure!(
            length > 0 && length <= section_limit,
            "CAR block section exceeds limit"
        );
        let mut bytes = read_section(
            &mut self.input,
            length,
            &mut self.total,
            self.limits.max_total_bytes,
        )?;
        // Bound the CID parser independently of the block section. Round-trip
        // equality rejects permissive upstream varint/CID aliases.
        let prefix = &bytes[..bytes.len().min(MAX_CID_BYTES)];
        let mut remaining = prefix;
        let cid = Cid::read_bytes(&mut remaining).context("invalid CAR block CID")?;
        let cid_bytes = prefix.len() - remaining.len();
        ensure!(
            cid.to_bytes() == bytes[..cid_bytes],
            "noncanonical CAR block CID"
        );
        ensure!(
            bytes.len() - cid_bytes <= self.limits.max_block_bytes,
            "CAR block exceeds limit"
        );
        bytes.drain(..cid_bytes);
        self.blocks += 1;
        Ok(Some((cid, bytes)))
    }
}

/// Stream CARv1 records in caller-selected order, without graph interpretation.
/// A rejected or partially written record poisons this writer.
pub struct Writer<W> {
    output: W,
    limits: Limits,
    total: u64,
    blocks: usize,
    failed: bool,
}

impl<W: Write> Writer<W> {
    pub fn new(mut output: W, root: Cid, limits: Limits) -> Result<Self> {
        limits.validate()?;
        let header = header_bytes(root)?;
        ensure!(
            header.len() <= limits.max_header_bytes,
            "CAR header exceeds limit"
        );
        let prefix = varint(header.len() as u64);
        let total = (prefix.1 + header.len()) as u64;
        ensure!(
            total <= limits.max_total_bytes,
            "CAR exceeds total byte limit"
        );
        output.write_all(&prefix.0[..prefix.1])?;
        output.write_all(&header)?;
        Ok(Self {
            output,
            limits,
            total,
            blocks: 0,
            failed: false,
        })
    }

    pub fn write_block(&mut self, cid: &Cid, bytes: &[u8]) -> Result<()> {
        ensure!(!self.failed, "CAR writer cannot resume after an error");
        self.failed = true;
        ensure!(
            self.blocks < self.limits.max_blocks,
            "CAR block count exceeds limit"
        );
        ensure!(
            bytes.len() <= self.limits.max_block_bytes,
            "CAR block exceeds limit"
        );
        let cid_bytes = cid.to_bytes();
        ensure!(
            cid_bytes.len() <= MAX_CID_BYTES,
            "CAR block CID exceeds limit"
        );
        let length = (bytes.len() as u64)
            .checked_add(cid_bytes.len() as u64)
            .context("CAR block section size overflow")?;
        let prefix = varint(length);
        let total = self
            .total
            .checked_add(prefix.1 as u64)
            .and_then(|total| total.checked_add(length))
            .context("CAR total size overflow")?;
        ensure!(
            total <= self.limits.max_total_bytes,
            "CAR exceeds total byte limit"
        );
        self.output.write_all(&prefix.0[..prefix.1])?;
        self.output.write_all(&cid_bytes)?;
        self.output.write_all(bytes)?;
        self.total = total;
        self.blocks += 1;
        self.failed = false;
        Ok(())
    }

    /// Flush the stream and recover its writer. No footer or padding is emitted.
    pub fn finish(mut self) -> Result<W> {
        ensure!(!self.failed, "CAR writer cannot finish after an error");
        self.output.flush()?;
        Ok(self.output)
    }
}

pub(crate) fn header_bytes(root: Cid) -> Result<Vec<u8>> {
    ensure!(
        root.encoded_len() <= MAX_CID_BYTES,
        "CAR root CID exceeds limit"
    );
    Ok(serde_ipld_dagcbor::to_vec(&Header {
        roots: [root],
        version: 1,
    })?)
}

fn read_section<R: Read>(
    input: &mut R,
    length: u64,
    total: &mut u64,
    limit: u64,
) -> Result<Vec<u8>> {
    ensure!(
        length <= limit.saturating_sub(*total),
        "CAR exceeds total byte limit"
    );
    let length = usize::try_from(length).context("CAR section cannot fit in memory")?;
    let mut bytes = vec![0; length];
    input
        .read_exact(&mut bytes)
        .context("truncated CAR section")?;
    *total += length as u64;
    Ok(bytes)
}

fn read_varint<R: Read>(input: &mut R, total: &mut u64, limit: u64) -> Result<Option<u64>> {
    let mut value = 0u64;
    for index in 0..10 {
        let mut byte = [0];
        let count = loop {
            match input.read(&mut byte) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        if count == 0 {
            ensure!(index == 0, "truncated CAR length");
            return Ok(None);
        }
        // One byte may be probed at an exact budget boundary to distinguish
        // clean EOF from forbidden trailing data; it is never allocated.
        ensure!(*total < limit, "CAR exceeds total byte limit");
        *total += 1;
        ensure!(index != 9 || byte[0] <= 1, "overflowing CAR length");
        value |= u64::from(byte[0] & 0x7f) << (index * 7);
        if byte[0] & 0x80 == 0 {
            ensure!(index == 0 || byte[0] != 0, "nonminimal CAR length");
            return Ok(Some(value));
        }
    }
    unreachable!("the tenth byte either terminates or exceeds u64")
}

fn varint(mut value: u64) -> ([u8; 10], usize) {
    let mut bytes = [0; 10];
    let mut length = 0;
    while value >= 128 {
        bytes[length] = value as u8 | 0x80;
        value >>= 7;
        length += 1;
    }
    bytes[length] = value as u8;
    (bytes, length + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    fn root() -> Cid {
        "QmUNLLsPACCz1vLxQVkXqqLX5R1X345qqfHbsf67hvA3Nn"
            .parse()
            .unwrap()
    }

    fn limits() -> Limits {
        Limits {
            max_header_bytes: 1024,
            max_block_bytes: 16,
            max_blocks: 4,
            max_total_bytes: 4096,
        }
    }

    // Independently encoded original composed::encode_car fixture: a 56-byte
    // header, then a 38-byte CIDv0 + four-byte block section.
    fn golden() -> Vec<u8> {
        let mut bytes = vec![0x38, 0xa2, 0x65];
        bytes.extend_from_slice(b"roots");
        bytes.extend_from_slice(&[0x81, 0xd8, 0x2a, 0x58, 0x23, 0]);
        bytes.extend_from_slice(&root().to_bytes());
        bytes.push(0x67);
        bytes.extend_from_slice(b"version");
        bytes.extend_from_slice(&[1, 38]);
        bytes.extend_from_slice(&root().to_bytes());
        bytes.extend_from_slice(&[10, 2, 8, 1]);
        assert_eq!(bytes.len(), 96);
        bytes
    }

    fn accepted(bytes: &[u8], limits: Limits) -> bool {
        let Ok(mut reader) = Reader::new(bytes, limits) else {
            return false;
        };
        loop {
            match reader.next_block() {
                Ok(Some(_)) => (),
                Ok(None) => return true,
                Err(_) => return false,
            }
        }
    }

    #[test]
    fn preserves_original_car_bytes_and_empty_reconciliation() {
        let mut writer = Writer::new(Vec::new(), root(), limits()).unwrap();
        writer.write_block(&root(), &[10, 2, 8, 1]).unwrap();
        assert_eq!(writer.finish().unwrap(), golden());
        let bytes = golden();
        let mut reader = Reader::new(bytes.as_slice(), limits()).unwrap();
        assert_eq!(reader.root(), root());
        assert_eq!(
            reader.next_block().unwrap(),
            Some((root(), vec![10, 2, 8, 1]))
        );
        assert!(reader.next_block().unwrap().is_none());
        let empty = Writer::new(Vec::new(), root(), limits())
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(empty, golden()[..57]);
        assert!(accepted(
            &empty,
            Limits {
                max_blocks: 0,
                ..limits()
            }
        ));
    }

    #[test]
    fn rejects_partial_headers_records_and_padding() {
        let bytes = golden();
        for length in 0..bytes.len() {
            if length != 57 {
                assert!(
                    !accepted(&bytes[..length], limits()),
                    "accepted truncation {length}"
                );
            }
        }
        for suffix in [&[0][..], &[0x80], &[1, 0], &[0xff; 10]] {
            let mut bytes = golden();
            bytes.extend_from_slice(suffix);
            assert!(!accepted(&bytes, limits()));
        }
    }

    #[test]
    fn rejects_nonminimal_overflowing_and_overlong_lengths() {
        for prefix in [
            vec![0xb8, 0],
            vec![0x80; 10],
            vec![0xff; 9].into_iter().chain([2]).collect(),
        ] {
            let mut bytes = prefix;
            bytes.extend_from_slice(&golden()[1..]);
            assert!(!accepted(&bytes, limits()));
        }
        for prefix in [
            vec![0xa6, 0],
            vec![0x80; 10],
            vec![0xff; 9].into_iter().chain([2]).collect(),
        ] {
            let mut bytes = golden()[..57].to_vec();
            bytes.extend(prefix);
            bytes.extend_from_slice(&golden()[58..]);
            assert!(!accepted(&bytes, limits()));
        }
    }

    struct PrefixOnly(Cursor<Vec<u8>>);
    impl Read for PrefixOnly {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            assert!(
                (self.0.position() as usize) < self.0.get_ref().len(),
                "read payload after rejecting advertised bounds"
            );
            self.0.read(output)
        }
    }

    #[test]
    fn rejects_huge_advertised_sections_before_reading_payload() {
        // 1025-byte header and u64::MAX block section: neither has a payload.
        assert!(Reader::new(PrefixOnly(Cursor::new(vec![0x81, 0x08])), limits()).is_err());
        let mut bytes = golden()[..57].to_vec();
        bytes.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1]);
        let mut reader = Reader::new(PrefixOnly(Cursor::new(bytes)), limits()).unwrap();
        assert!(reader.next_block().is_err());
        assert!(
            reader.next_block().is_err(),
            "malformed streams cannot resume"
        );

        // Exercise the per-record bound independently of the larger wire budget.
        let mut bytes = golden()[..57].to_vec();
        bytes.push(113); // 16 permitted payload bytes + 96 CID bytes + 1.
        let mut reader = Reader::new(PrefixOnly(Cursor::new(bytes)), limits()).unwrap();
        assert!(reader.next_block().is_err());
    }

    #[test]
    fn rejects_record_count_and_total_budget_before_payload() {
        let mut bytes = golden();
        bytes.push(38);
        let mut reader = Reader::new(
            PrefixOnly(Cursor::new(bytes)),
            Limits {
                max_blocks: 1,
                ..limits()
            },
        )
        .unwrap();
        reader.next_block().unwrap().unwrap();
        assert!(reader.next_block().is_err());
        let mut reader = Reader::new(
            PrefixOnly(Cursor::new(golden()[..58].to_vec())),
            Limits {
                max_total_bytes: 95,
                ..limits()
            },
        )
        .unwrap();
        assert!(reader.next_block().is_err());
    }

    #[test]
    fn enforces_inclusive_header_payload_count_and_wire_limits() {
        let exact = Limits {
            max_header_bytes: 56,
            max_block_bytes: 4,
            max_blocks: 1,
            max_total_bytes: 96,
        };
        assert!(accepted(&golden(), exact));
        for smaller in [
            Limits {
                max_header_bytes: 55,
                ..exact
            },
            Limits {
                max_block_bytes: 3,
                ..exact
            },
            Limits {
                max_blocks: 0,
                ..exact
            },
            Limits {
                max_total_bytes: 95,
                ..exact
            },
        ] {
            assert!(!accepted(&golden(), smaller));
        }
        let mut writer = Writer::new(Vec::new(), root(), exact).unwrap();
        writer.write_block(&root(), &[10, 2, 8, 1]).unwrap();
        assert_eq!(writer.finish().unwrap(), golden());
        for smaller in [
            Limits {
                max_block_bytes: 3,
                ..exact
            },
            Limits {
                max_blocks: 0,
                ..exact
            },
            Limits {
                max_total_bytes: 95,
                ..exact
            },
        ] {
            let mut output = Vec::new();
            let mut writer = Writer::new(&mut output, root(), smaller).unwrap();
            assert!(writer.write_block(&root(), &[10, 2, 8, 1]).is_err());
            assert!(writer.finish().is_err());
            assert_eq!(output, golden()[..57], "rejected record wrote bytes");
        }
    }

    #[test]
    fn rejects_noncanonical_header_version_roots_and_cids() {
        for (position, value) in [(1, 0xa3), (8, 0x80), (8, 0x82), (56, 2)] {
            let mut bytes = golden();
            bytes[position] = value;
            assert!(!accepted(&bytes, limits()));
        }
        let mut bytes = golden();
        bytes[0] += 1;
        bytes.splice(56..57, [0x18, 1]);
        assert!(!accepted(&bytes, limits()));
        let mut bytes = golden()[..57].to_vec();
        // Nonminimal CIDv1 version, followed by an identity multihash.
        bytes.extend_from_slice(&[6, 0x81, 0, 0x55, 0, 1, b'x']);
        assert!(!accepted(&bytes, limits()));
        let mut bytes = golden()[..57].to_vec();
        bytes.extend_from_slice(&[1, 1]);
        assert!(!accepted(&bytes, limits()));
    }

    #[test]
    fn rejects_multiple_roots_and_unbounded_header_array_lengths() {
        let mut bytes = golden();
        bytes[0] += 39;
        bytes[8] = 0x82;
        bytes.splice(48..48, golden()[9..48].iter().copied());
        assert!(!accepted(&bytes, limits()));
        let mut bytes = golden();
        bytes[0] += 8;
        bytes.splice(8..9, [0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert!(!accepted(&bytes, limits()));
    }

    #[test]
    fn rejects_nonminimal_and_overflowing_cid_fields() {
        let canonical = [1, 0x55, 0, 1, b'x'];
        for index in 0..4 {
            let mut bytes = golden()[..57].to_vec();
            bytes.push(6);
            bytes.extend_from_slice(&canonical[..index]);
            bytes.extend_from_slice(&[canonical[index] | 0x80, 0]);
            bytes.extend_from_slice(&canonical[index + 1..]);
            assert!(!accepted(&bytes, limits()));
        }
        for cid in [
            hex::decode("8180808080808080800255000178").unwrap(),
            hex::decode("0155008180808080808080800278").unwrap(),
        ] {
            let mut bytes = golden()[..57].to_vec();
            bytes.push(cid.len() as u8);
            bytes.extend(cid);
            assert!(!accepted(&bytes, limits()));
        }
    }

    #[test]
    fn propagates_io_failures_without_resuming_a_partial_stream() {
        struct FailAfterHeader {
            remaining: usize,
        }
        impl Write for FailAfterHeader {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    return Err(io::Error::other("write failed"));
                }
                let written = self.remaining.min(bytes.len());
                self.remaining -= written;
                Ok(written)
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("flush failed"))
            }
        }
        let mut writer = Writer::new(FailAfterHeader { remaining: 58 }, root(), limits()).unwrap();
        assert!(writer.write_block(&root(), &[1]).is_err());
        assert!(writer.write_block(&root(), &[1]).is_err());
        assert!(writer.finish().is_err());
        assert!(
            Writer::new(FailAfterHeader { remaining: 57 }, root(), limits())
                .unwrap()
                .finish()
                .is_err()
        );

        struct FailAfterPrefix(Cursor<Vec<u8>>);
        impl Read for FailAfterPrefix {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.0.position() as usize == self.0.get_ref().len() {
                    return Err(io::Error::other("read failed"));
                }
                self.0.read(output)
            }
        }
        let mut reader = Reader::new(
            FailAfterPrefix(Cursor::new(golden()[..58].to_vec())),
            limits(),
        )
        .unwrap();
        assert!(reader.next_block().is_err());
        assert!(reader.next_block().is_err());
    }

    #[test]
    fn frames_v1_identity_cids_without_claiming_content_validation() {
        let cid = crate::cid_identity::decode_cid(&[1, 0x55, 0, 1, b'x']).unwrap();
        let mut writer = Writer::new(Vec::new(), cid, limits()).unwrap();
        writer.write_block(&cid, b"different").unwrap();
        let bytes = writer.finish().unwrap();
        let mut reader = Reader::new(bytes.as_slice(), limits()).unwrap();
        assert_eq!(reader.root(), cid);
        assert_eq!(
            reader.next_block().unwrap(),
            Some((cid, b"different".to_vec()))
        );
        assert!(reader.next_block().unwrap().is_none());
    }

    #[test]
    fn streams_multibyte_record_lengths_without_changing_block_order() {
        let limits = Limits {
            max_block_bytes: 16_350,
            max_total_bytes: 32_768,
            ..limits()
        };
        let mut writer = Writer::new(Vec::new(), root(), limits).unwrap();
        writer.write_block(&root(), &[1; 94]).unwrap();
        writer.write_block(&root(), &[2; 16_350]).unwrap();
        let bytes = writer.finish().unwrap();
        // CIDv0 is 34 bytes: record sizes 128 and 16384 cross varint boundaries.
        assert_eq!(&bytes[57..59], &[0x80, 1]);
        assert_eq!(&bytes[187..190], &[0x80, 0x80, 1]);
        let mut reader = Reader::new(bytes.as_slice(), limits).unwrap();
        assert_eq!(reader.next_block().unwrap(), Some((root(), vec![1; 94])));
        assert_eq!(
            reader.next_block().unwrap(),
            Some((root(), vec![2; 16_350]))
        );
        assert!(reader.next_block().unwrap().is_none());
    }
}
