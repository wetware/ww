//! Pure IPNS name parsing and authenticated signed-record semantics.
//!
//! Raw signed record bytes are preserved exactly; transport, persistence and
//! publication policy belong to callers.

use std::cmp::Ordering;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use cid::{multibase::Base, Cid};
use libp2p_identity::PeerId;
use rust_ipns::Record;

pub const MAX_RECORD_SIZE: usize = 10_240;

const LIBP2P_KEY_CODEC: u64 = 0x72;

/// Parse either a canonical Base36 IPNS name or a legacy base58 Peer ID.
pub fn parse_name(value: &str) -> Result<PeerId> {
    let value = value.strip_prefix("/ipns/").unwrap_or(value);
    if let Ok(peer_id) = value.parse::<PeerId>() {
        return Ok(peer_id);
    }
    let cid = value
        .parse::<Cid>()
        .with_context(|| format!("invalid IPNS name: {value}"))?;
    if cid.codec() != LIBP2P_KEY_CODEC {
        bail!(
            "invalid IPNS name codec 0x{:x}; expected libp2p-key (0x72)",
            cid.codec()
        );
    }
    PeerId::from_multihash(cid.hash().to_owned())
        .map_err(|_| anyhow::anyhow!("IPNS name contains an invalid Peer ID multihash"))
}

/// Format a Peer ID as the canonical Base36 CIDv1 IPNS name.
pub fn canonical_name(peer_id: PeerId) -> String {
    Cid::new_v1(LIBP2P_KEY_CODEC, peer_id.into())
        .to_string_of_base(Base::Base36Lower)
        .expect("Base36 is a valid CID multibase")
}

/// A locally verified signed record and its authenticated semantics.
#[derive(Clone, Debug)]
pub struct SignedRecord {
    raw: Vec<u8>,
    record: Record,
    value: Vec<u8>,
    deployment_cid: Option<Cid>,
    eol: DateTime<Utc>,
    ttl: Duration,
}

impl SignedRecord {
    pub fn decode(name: PeerId, raw: Vec<u8>) -> Result<Self> {
        let record = Record::decode(&raw).context("decode raw IPNS record")?;
        record
            .verify_signature(name)
            .context("verify IPNS record signer and signature")?;
        let value = record.value().to_vec();
        let deployment_cid = parse_value(&value).ok();
        let eol = record
            .validity()
            .context("parse signed IPNS record EOL")?
            .with_timezone(&Utc);
        let ttl = Duration::from_nanos(record.ttl());
        Ok(Self {
            raw,
            record,
            value,
            deployment_cid,
            eol,
            ttl,
        })
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }

    pub fn deployment_cid(&self) -> Option<Cid> {
        self.deployment_cid
    }

    pub fn sequence(&self) -> u64 {
        self.record.sequence()
    }

    pub fn eol(&self) -> DateTime<Utc> {
        self.eol
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        self.eol <= now
    }

    pub fn compare(&self, other: &Self) -> Result<Ordering> {
        self.record
            .compare(&other.record)
            .context("compare signed IPNS records")
    }
}

fn parse_value(value: &[u8]) -> Result<Cid> {
    let value = std::str::from_utf8(value).context("IPNS value is not UTF-8")?;
    let cid = value
        .strip_prefix("/ipfs/")
        .context("IPNS value is not an /ipfs/<cid> deployment binding")?;
    crate::cid_identity::parse_cid(cid).context("IPNS value contains an invalid CID")
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p_identity::Keypair;

    const RECORD_TTL: Duration = Duration::from_secs(5 * 60);

    fn test_cid(byte: u8) -> Cid {
        use cid::multihash::Multihash;
        Cid::new_v1(0x55, Multihash::<64>::wrap(0x00, &[byte]).unwrap())
    }

    #[test]
    fn canonical_name_roundtrip() {
        let keypair = Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id();
        let name = canonical_name(peer_id);
        assert!(name.starts_with("k51"));
        assert_eq!(parse_name(&name).unwrap(), peer_id);
        assert_eq!(parse_name(&peer_id.to_string()).unwrap(), peer_id);
    }

    #[test]
    fn signed_record_roundtrip_uses_authenticated_fields() {
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let cid = test_cid(1);
        let record = Record::new(
            &keypair,
            format!("/ipfs/{cid}"),
            Utc::now() + chrono::Duration::hours(1),
            7,
            RECORD_TTL,
        )
        .unwrap();
        let decoded = SignedRecord::decode(name, record.encode().unwrap()).unwrap();
        assert_eq!(decoded.deployment_cid(), Some(cid));
        assert_eq!(decoded.sequence(), 7);
        assert_eq!(decoded.ttl(), RECORD_TTL);
    }

    #[test]
    fn signed_deployment_alias_keeps_original_value_and_record_bytes() {
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let cid = Cid::new_v1(
            0x55,
            cid::multihash::Multihash::<64>::wrap(0x12, &[0xff; 32]).unwrap(),
        );
        let alias = cid.to_string_of_base(cid::multibase::Base::Base64).unwrap();
        assert!(alias.contains('/'));
        let value = format!("/ipfs/{alias}");
        let raw = Record::new(
            &keypair,
            &value,
            Utc::now() + chrono::Duration::hours(1),
            7,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap();
        let decoded = SignedRecord::decode(name, raw.clone()).unwrap();
        assert_eq!(decoded.deployment_cid(), Some(cid));
        assert_eq!(decoded.value(), value.as_bytes());
        assert_eq!(decoded.raw(), raw);
        assert!(parse_value(format!("/ipfs/{alias}/child").as_bytes()).is_err());
    }

    #[test]
    fn deployment_binding_rejects_noncanonical_binary_aliases() {
        for bytes in [
            vec![1, 0x55, 0, 1, b'x', 0],
            vec![0x81, 0, 0x55, 0, 1, b'x'],
        ] {
            let alias = cid::multibase::encode(cid::multibase::Base::Base32Lower, bytes);
            assert!(parse_value(format!("/ipfs/{alias}").as_bytes()).is_err());
        }
    }

    #[test]
    fn official_v2_only_record_is_consumed() {
        // IPNS Record specification vector 6. The record omits every legacy
        // V1 field and therefore exercises the reviewed PR #503 behavior.
        let raw = hex::decode(concat!(
            "42406a7698585e2b170709d4947e148b18b120d76db43515edacf290df96b71e",
            "29c0ca5a16510256814f8255532ec4549ade26f4ca8335f4c41e2cfffeb1ba0",
            "aa5014a78a56354544c1b000001a3185c50006556616c756558242f697066732f",
            "6261666b7161647477676977773633746d70657168657a6c646e357a67696853",
            "657175656e6365006856616c6964697479581b323132332d30382d3134543132",
            "3a31373a30332e3639343035325a6c56616c69646974795479706500"
        ))
        .unwrap();
        let name: PeerId = "12D3KooWGuR5BdSqp23UeoeesuwYwW3ebQ9rZ8aVwfWEDU8kvCYJ"
            .parse()
            .unwrap();
        let record = SignedRecord::decode(name, raw).unwrap();
        assert_eq!(record.sequence(), 0);
        assert_eq!(record.value(), b"/ipfs/bafkqadtwgiww63tmpeqhezldn5zgi");
        assert!(record.deployment_cid().is_some());
    }
}
