//! ABI types and decoding for the Atom contract.
//!
//! HeadUpdated event and head() view. Decode from JSON-RPC log shape and eth_call return.
//! Uses alloy sol-types for ABI decoding; head() returns (uint64, bytes), event HeadUpdated(seq, writer, cid, cidHash).

use alloy::sol_types::SolType;
use anyhow::{Context, Result};
use serde_json::Value;

/// First 4 bytes of keccak256("HeadUpdated(uint64,address,bytes,bytes32)").
pub const HEAD_UPDATED_TOPIC0: [u8; 4] = [0x85, 0xf2, 0xcb, 0x2e];

/// Selector for head().
pub const HEAD_SELECTOR: [u8; 4] = [0x8f, 0x7d, 0xcf, 0xa3];

/// Observed HeadUpdated event with chain metadata (observed-only; no reorg safety).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadUpdatedObserved {
    pub seq: u64,
    pub writer: [u8; 20],
    pub cid: Vec<u8>,
    pub cid_hash: [u8; 32],
    pub block_number: u64,
    pub tx_hash: [u8; 32],
    pub log_index: u64,
}

/// Current head state (from head() or from events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentHead {
    pub seq: u64,
    pub cid: Vec<u8>,
}

/// Decode a JSON-RPC log (eth_subscription / eth_getLogs result) into HeadUpdatedObserved.
/// Option A: event HeadUpdated(uint64 indexed seq, address indexed writer, bytes cid, bytes32 indexed cidHash).
/// Data is ABI-encoded single bytes: offset (32) then at offset: length then cid.
pub fn decode_log_to_observed(log_value: &Value) -> Result<HeadUpdatedObserved> {
    let block_number = parse_hex_u64(
        log_value
            .get("blockNumber")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing blockNumber"))?,
    )?;
    let log_index = parse_hex_u64(
        log_value
            .get("logIndex")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing logIndex"))?,
    )?;
    let tx_hash = parse_hex_bytes_32(
        log_value
            .get("transactionHash")
            .and_then(|h| h.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing transactionHash"))?,
    )?;
    let data = parse_hex_bytes(
        log_value
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing data"))?,
    )?;
    let topics = log_value
        .get("topics")
        .and_then(|t| t.as_array())
        .ok_or_else(|| anyhow::anyhow!("Missing topics"))?;
    if topics.len() < 4 {
        anyhow::bail!("Expected at least 4 topics, got {}", topics.len());
    }
    let seq = {
        let t1 = parse_hex_bytes(
            topics[1]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("topic1 not str"))?,
        )?;
        if t1.len() < 8 {
            anyhow::bail!("topic1 too short for uint64");
        }
        u64::from_be_bytes(
            t1[t1.len() - 8..]
                .try_into()
                .context("topic1 last 8 bytes for uint64")?,
        )
    };
    let writer = parse_hex_bytes_20(
        topics[2]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("topic2 not str"))?,
    )?;
    let cid_hash = parse_hex_bytes_32(
        topics[3]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("topic3 not str"))?,
    )?;
    // Event data: single ABI-encoded `bytes`. Try alloy first; fall back to manual when contract uses non-standard offset (e.g. 64).
    let cid = decode_event_data_bytes(&data).context("decode event data bytes")?;

    Ok(HeadUpdatedObserved {
        seq,
        writer,
        cid,
        cid_hash,
        block_number,
        tx_hash,
        log_index,
    })
}

/// Decode head() return data (eth_call result) as a `(uint64, bytes)` ABI parameter sequence.
pub fn decode_head_return(data: &[u8]) -> Result<CurrentHead> {
    type HeadReturn = (
        alloy::sol_types::sol_data::Uint<64>,
        alloy::sol_types::sol_data::Bytes,
    );
    let (seq, cid) =
        HeadReturn::abi_decode_sequence(data, true).context("decode head() return parameters")?;
    Ok(CurrentHead {
        seq,
        cid: cid.to_vec(),
    })
}

/// Decode event data (single ABI `bytes`). Uses alloy when layout is standard; falls back to manual when offset != 32 (e.g. 64).
fn decode_event_data_bytes(data: &[u8]) -> Result<Vec<u8>> {
    use alloy::sol_types::sol_data::Bytes;
    if let Ok(b) = Bytes::abi_decode(data, false) {
        return Ok(b.to_vec());
    }
    decode_event_data_bytes_manual(data)
}

/// Manual decode of ABI-encoded single `bytes`: reads offset from first word (bytes 28..32), then length + payload.
fn decode_event_data_bytes_manual(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 32 {
        anyhow::bail!("event data too short");
    }
    let cid_offset = u32::from_be_bytes(
        data[28..32]
            .try_into()
            .context("event data offset bytes [28..32]")?,
    ) as usize;
    if data.len() < cid_offset + 32 {
        anyhow::bail!("event data too short for cid offset");
    }
    let len = u32::from_be_bytes(
        data[cid_offset + 28..cid_offset + 32]
            .try_into()
            .context("event data cid length bytes")?,
    ) as usize;
    if data.len() < cid_offset + 32 + len {
        anyhow::bail!("event data too short for cid len {}", len);
    }
    Ok(data[cid_offset + 32..cid_offset + 32 + len].to_vec())
}

fn parse_hex_u64(s: &str) -> Result<u64> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(s, 16).context("parse hex u64")
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(s).context("parse hex bytes")
}

fn parse_hex_bytes_32(s: &str) -> Result<[u8; 32]> {
    let bytes = parse_hex_bytes(s)?;
    if bytes.len() != 32 {
        anyhow::bail!("Expected 32 bytes, got {}", bytes.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn parse_hex_bytes_20(s: &str) -> Result<[u8; 20]> {
    let bytes = parse_hex_bytes(s)?;
    if bytes.len() == 20 {
        let mut out = [0u8; 20];
        out.copy_from_slice(&bytes);
        Ok(out)
    } else if bytes.len() == 32 {
        let mut out = [0u8; 20];
        out.copy_from_slice(&bytes[12..32]);
        Ok(out)
    } else {
        anyhow::bail!("Expected 20 or 32 bytes for address, got {}", bytes.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::sol_types::SolType;

    #[test]
    fn topic0_constant() {
        assert_eq!(HEAD_UPDATED_TOPIC0, [0x85, 0xf2, 0xcb, 0x2e]);
    }

    type HeadReturn = (
        alloy::sol_types::sol_data::Uint<64>,
        alloy::sol_types::sol_data::Bytes,
    );

    fn encode_head_return_sequence(seq: u64, cid: &[u8]) -> Vec<u8> {
        let padded_len = cid.len().div_ceil(32) * 32;
        let mut data = vec![0; 96 + padded_len];
        data[24..32].copy_from_slice(&seq.to_be_bytes());
        data[56..64].copy_from_slice(&64_u64.to_be_bytes());
        data[88..96].copy_from_slice(&(cid.len() as u64).to_be_bytes());
        data[96..96 + cid.len()].copy_from_slice(cid);
        data
    }

    #[test]
    fn decode_head_return_solidity_parameter_sequence() {
        // Binary form of bafkreibm6jg3ux5qugqkmfqt5uj5rxszb4sa4e3u7jj4c5ukv5s4xvcc7a.
        let cid =
            hex::decode("015512202cf24dba5fb0a1a0a61613ed13d8de590f240e1374fa53c1768aaf65cbd442f8")
                .unwrap();
        let data = encode_head_return_sequence(8, &cid);

        let (alloy_seq, alloy_cid) = HeadReturn::abi_decode_sequence(&data, true).unwrap();
        assert_eq!(alloy_seq, 8);
        assert_eq!(alloy_cid.as_ref(), cid);

        let head = decode_head_return(&data).unwrap();

        assert_eq!(head, CurrentHead { seq: 8, cid });
    }

    #[test]
    fn decode_head_return_minimal() {
        let data = encode_head_return_sequence(0, &[]);
        let head = decode_head_return(&data).unwrap();
        assert_eq!(head.seq, 0);
        assert!(head.cid.is_empty());
    }

    #[test]
    fn decode_head_return_with_cid() {
        let seq: u64 = 42;
        let cid: &[u8] = b"QmFoo";
        let data = encode_head_return_sequence(seq, cid);
        let head = decode_head_return(&data).unwrap();
        assert_eq!(head.seq, 42);
        assert_eq!(head.cid.as_slice(), cid);
    }

    // -- Error path tests for defensive unwrap→? migration --

    #[test]
    fn decode_head_return_too_short() {
        let data = vec![0u8; 10]; // way too short
        assert!(decode_head_return(&data).is_err());
    }

    #[test]
    fn decode_head_return_bad_cid_offset() {
        // 64 bytes passes the length check, but cid_offset points past end
        let mut data = vec![0u8; 64];
        // word1 bytes [60..64] = cid_offset, set to 9999
        data[60..64].copy_from_slice(&9999u32.to_be_bytes());
        assert!(decode_head_return(&data).is_err());
    }

    #[test]
    fn decode_head_return_cid_len_overflow() {
        // Valid offset but cid_len exceeds remaining data
        let mut data = vec![0u8; 128];
        // cid_offset = 64 (valid, data[64..] exists)
        data[60..64].copy_from_slice(&64u32.to_be_bytes());
        // cid_len at offset 64+28..64+32 = 9999 (way past end)
        data[92..96].copy_from_slice(&9999u32.to_be_bytes());
        assert!(decode_head_return(&data).is_err());
    }

    #[test]
    fn decode_head_return_truncated_cid_payload() {
        let data = encode_head_return_sequence(8, &[0x42; 36]);
        for truncated_len in [131, 159] {
            assert!(
                decode_head_return(&data[..truncated_len]).is_err(),
                "accepted head return truncated to {truncated_len} bytes"
            );
        }
    }

    #[test]
    fn decode_head_return_rejects_uint64_overflow() {
        let mut data = encode_head_return_sequence(8, &[]);
        data[0] = 1;
        assert!(decode_head_return(&data).is_err());
    }

    #[test]
    fn decode_head_return_rejects_trailing_bytes() {
        let mut data = encode_head_return_sequence(8, &[]);
        data.push(0);
        assert!(decode_head_return(&data).is_err());
    }

    #[test]
    fn decode_event_data_too_short() {
        let data = vec![0u8; 4]; // way too short
        assert!(decode_event_data_bytes(&data).is_err());
    }

    #[test]
    fn decode_event_data_bad_offset() {
        let mut data = vec![0u8; 32];
        // offset points past end
        data[28..32].copy_from_slice(&9999u32.to_be_bytes());
        assert!(decode_event_data_bytes(&data).is_err());
    }

    #[test]
    fn decode_event_data_len_overflow() {
        // Valid offset but length exceeds remaining data
        let mut data = vec![0u8; 96];
        data[28..32].copy_from_slice(&32u32.to_be_bytes()); // offset = 32
        data[60..64].copy_from_slice(&9999u32.to_be_bytes()); // len = 9999
        assert!(decode_event_data_bytes(&data).is_err());
    }

    #[test]
    fn decode_event_data_bytes_standard() {
        use alloy::sol_types::sol_data::Bytes;
        let cid: &[u8] = b"cid-1";
        let data = Bytes::abi_encode(&alloy::primitives::Bytes::from(cid));
        let decoded = super::decode_event_data_bytes(&data).unwrap();
        assert_eq!(decoded.as_slice(), cid);
    }

    #[test]
    fn decode_event_data_bytes_offset64() {
        // Manual layout when contract emits offset 64: word0 = 64, then at 64: length (32 bytes) + cid.
        let cid: &[u8] = b"cid-1";
        let mut data = vec![0u8; 32];
        data[28..32].copy_from_slice(&64u32.to_be_bytes());
        data.resize(64 + 32 + cid.len(), 0);
        data[64 + 28..64 + 32].copy_from_slice(&(cid.len() as u32).to_be_bytes());
        data[64 + 32..64 + 32 + cid.len()].copy_from_slice(cid);
        let decoded = super::decode_event_data_bytes(&data).unwrap();
        assert_eq!(decoded.as_slice(), cid);
    }
}
