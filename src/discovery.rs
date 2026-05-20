//! Discovery primitives.
//!
//! Provides the well-known LAN Kademlia record key used by wetware nodes
//! to announce themselves.
#![cfg(not(target_arch = "wasm32"))]

use std::sync::LazyLock;

/// Well-known CID that wetware nodes provide on the LAN DHT.
///
/// Computed as `CIDv1(raw, BLAKE3(b"wetware"))`.  Any peer providing
/// this key is advertising itself as a wetware host.
pub static DISCOVERY_CID: LazyLock<cid::Cid> = LazyLock::new(|| {
    let digest = blake3::hash(b"wetware");
    let mh = cid::multihash::Multihash::<64>::wrap(0x1e, digest.as_bytes())
        .expect("blake3 digest always fits in 64-byte multihash");
    cid::Cid::new_v1(0x55, mh)
});

/// The discovery CID as a Kad record key (raw CID bytes).
pub fn discovery_record_key() -> libp2p::kad::RecordKey {
    libp2p::kad::RecordKey::new(&DISCOVERY_CID.to_bytes())
}
