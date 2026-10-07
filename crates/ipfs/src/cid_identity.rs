//! Strict decoding of bare CID identity at text and binary boundaries.

use anyhow::{bail, Context, Result};
use cid::Cid;

/// Parse a bare CID, accepting alternate multibase encodings and CIDv0 text.
///
/// Path and URL wrappers are rejected. The encoded bytes must be canonical,
/// but the text need not match [`Cid::to_string`]. Version, codec, and hash are
/// preserved; content validation belongs to the caller.
pub fn parse_cid(value: &str) -> Result<Cid> {
    let bytes = if value.starts_with("Qm") {
        cid::multibase::Base::Base58Btc.decode(value)
    } else {
        cid::multibase::decode(value).map(|(_, bytes)| bytes)
    }
    .map_err(|error| anyhow::anyhow!("{error}"))
    .context("invalid bare CID encoding")?;
    decode_cid(&bytes)
}

/// Decode exactly one CID with a canonical binary encoding.
///
/// Rejects trailing bytes and encodings that do not round-trip exactly,
/// including overflowing varints accepted by the underlying CID parser.
pub fn decode_cid(bytes: &[u8]) -> Result<Cid> {
    let mut remaining = bytes;
    let cid = Cid::read_bytes(&mut remaining).context("invalid CID")?;
    if !remaining.is_empty() {
        bail!("invalid CID: trailing bytes");
    }
    if cid.to_bytes() != bytes {
        bail!("invalid CID: noncanonical binary encoding");
    }
    Ok(cid)
}

#[cfg(test)]
mod tests {
    use super::{decode_cid, parse_cid};
    use cid::{multibase::Base, multihash::Multihash, Cid, Version};

    fn assert_rejected(bytes: &[u8]) {
        assert!(decode_cid(bytes).is_err(), "accepted bytes: {bytes:02x?}");
        let text = cid::multibase::encode(Base::Base16Lower, bytes);
        assert!(parse_cid(&text).is_err(), "accepted text: {text}");
    }

    #[test]
    fn alternate_multibase_text_preserves_identity() {
        let hash = Multihash::wrap(0x12, &[0xff; 32]).unwrap();
        let expected = Cid::new_v1(0x70, hash);
        let bytes = expected.to_bytes();
        assert!(cid::multibase::encode(Base::Base64, &bytes).contains('/'));
        for base in [
            Base::Base16Lower,
            Base::Base32Upper,
            Base::Base36Lower,
            Base::Base58Btc,
            Base::Base64,
            Base::Base64Pad,
            Base::Base64Url,
        ] {
            let text = cid::multibase::encode(base, &bytes);
            let actual = parse_cid(&text).unwrap();
            assert_eq!(actual, expected, "base: {base:?}");
            assert_eq!(actual.to_string(), expected.to_string());
            assert_eq!(actual.to_bytes(), bytes);
            assert_eq!(decode_cid(&bytes).unwrap(), expected);
        }
    }

    #[test]
    fn cid_v0_text_and_bytes_preserve_version_and_hash() {
        let mut bytes = vec![0x12, 0x20];
        bytes.extend_from_slice(&[0xab; 32]);
        let text = Base::Base58Btc.encode(&bytes);
        for actual in [parse_cid(&text).unwrap(), decode_cid(&bytes).unwrap()] {
            assert_eq!(actual.version(), Version::V0);
            assert_eq!(actual.codec(), 0x70);
            assert_eq!(actual.hash().code(), 0x12);
            assert_eq!(actual.hash().size(), 32);
            assert_eq!(actual.hash().digest(), &[0xab; 32]);
            assert_eq!(actual.to_bytes(), bytes);
            assert_eq!(actual.to_string(), text);
            let mut v1_bytes = vec![0x01, 0x70];
            v1_bytes.extend_from_slice(&bytes);
            let v1 = decode_cid(&v1_bytes).unwrap();
            assert_eq!(v1.version(), Version::V1);
            assert_eq!(v1.hash(), actual.hash());
            assert_ne!(v1, actual);
        }
    }

    #[test]
    fn identity_decoder_preserves_distinct_codecs_and_hashes() {
        let raw_bytes = [0x01, 0x55, 0x00, 0x01, b'x'];
        let dag_bytes = [0x01, 0x70, 0x00, 0x01, b'x'];
        let raw = decode_cid(&raw_bytes).unwrap();
        let dag = decode_cid(&dag_bytes).unwrap();
        assert_eq!(raw.version(), Version::V1);
        assert_eq!(raw.codec(), 0x55);
        assert_eq!(dag.codec(), 0x70);
        assert_eq!(raw.hash().code(), 0x00);
        assert_eq!(raw.hash().size(), 1);
        assert_eq!(raw.hash().digest(), b"x");
        assert_eq!(raw.hash(), dag.hash());
        assert_ne!(raw, dag);
        for (expected, bytes) in [(raw, raw_bytes), (dag, dag_bytes)] {
            assert_eq!(expected.to_bytes(), bytes);
            let text = cid::multibase::encode(Base::Base64, bytes);
            assert_eq!(parse_cid(&text).unwrap(), expected);
        }
        let other_hash = decode_cid(&[0x01, 0x55, 0x00, 0x01, b'y']).unwrap();
        assert_ne!(raw, other_hash);
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let canonical = [0x01, 0x55, 0x00, 0x01, b'x'];
        for suffix in [&[0][..], &canonical[..], &[0xde, 0xad, 0xbe, 0xef][..]] {
            let mut bytes = canonical.to_vec();
            bytes.extend_from_slice(suffix);
            assert_rejected(&bytes);
        }
        let mut v0 = vec![0x12, 0x20];
        v0.extend_from_slice(&[0xab; 32]);
        v0.push(0);
        assert_rejected(&v0);
        assert!(parse_cid(&Base::Base58Btc.encode(&v0)).is_err());
    }

    #[test]
    fn nonminimal_varints_are_rejected_at_each_position() {
        let canonical = [0x01, 0x55, 0x00, 0x01, b'x'];
        for position in 0..4 {
            let mut bytes = canonical[..position].to_vec();
            bytes.extend_from_slice(&[canonical[position] | 0x80, 0x00]);
            bytes.extend_from_slice(&canonical[position + 1..]);
            assert_rejected(&bytes);
        }
        for prefix in [vec![0x92, 0x00, 0x20], vec![0x12, 0xa0, 0x00]] {
            let mut bytes = prefix;
            bytes.extend_from_slice(&[0xab; 32]);
            assert_rejected(&bytes);
        }
    }

    #[test]
    fn overflowing_and_overlong_varints_are_rejected() {
        for text in [
            "f8180808080808080800255000178",
            "f0155008180808080808080800278",
        ] {
            let (_, bytes) = cid::multibase::decode(text).unwrap();
            // These parse upstream as ordinary CIDs; the binary round-trip
            // must reject the overflowing version and hash-length aliases.
            assert!(Cid::read_bytes(bytes.as_slice()).is_ok());
            assert_rejected(&bytes);
        }
        let canonical = [0x01, 0x55, 0x00, 0x01, b'x'];
        for position in 0..4 {
            let mut bytes = canonical[..position].to_vec();
            bytes.extend_from_slice(&[0x80; 10]);
            bytes.push(0);
            bytes.extend_from_slice(&canonical[position + 1..]);
            assert_rejected(&bytes);
        }
    }

    #[test]
    fn empty_and_truncated_binary_input_is_rejected() {
        let canonical = [0x01, 0x55, 0x00, 0x01, b'x'];
        for length in 0..canonical.len() {
            assert_rejected(&canonical[..length]);
        }
        for position in 0..4 {
            let mut bytes = canonical[..position].to_vec();
            bytes.push(0x80);
            assert_rejected(&bytes);
        }
        let mut v0 = vec![0x12, 0x20];
        v0.extend_from_slice(&[0xab; 31]);
        assert_rejected(&v0);
    }

    #[test]
    fn malformed_text_and_path_wrappers_are_rejected() {
        let hash = Multihash::wrap(0x12, &[0xab; 32]).unwrap();
        for cid in [Cid::new_v0(hash).unwrap(), Cid::new_v1(0x70, hash)] {
            let text = cid.to_string();
            for wrapped in [
                format!("/ipfs/{text}"),
                format!("https://example.com/ipfs/{text}"),
                format!("ipfs://{text}"),
                format!("{text}/child"),
                format!("{text}?arg=escape"),
                format!("{text}&arg=escape"),
                format!("{text}#fragment"),
                format!(" {text}"),
                format!("{text}\n"),
            ] {
                assert!(parse_cid(&wrapped).is_err(), "accepted {wrapped:?}");
            }
        }
        for text in ["", "not-a-cid", "b", "f0", "m!", "🦀"] {
            assert!(parse_cid(text).is_err(), "accepted {text:?}");
        }
    }
}
