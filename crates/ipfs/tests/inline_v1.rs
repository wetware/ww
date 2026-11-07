use cid::{multihash::Multihash, Cid};
use ipfs::map::{inline_v1::links, Map, MAX_BLOCK_BYTES, MAX_DEPTH, MAX_LIST_ELEMENTS};

// DAG-CBOR tag 42, identity prefix, CIDv1 raw identity hash of one byte.
const LINK: &[u8] = &[0xd8, 42, 0x46, 0, 1, 0x55, 0, 1, 7];

fn target() -> Cid {
    Cid::new_v1(0x55, Multihash::wrap(0, &[7]).unwrap())
}

#[test]
fn enumerates_links_in_standalone_values_and_map_envelopes() {
    assert_eq!(links(LINK).unwrap(), [target()]);

    // [1, {"a": [link], "b": {"x": link}}]
    let mut envelope = vec![0x82, 1, 0xa2, 0x61, b'a', 0x81];
    envelope.extend_from_slice(LINK);
    envelope.extend_from_slice(&[0x61, b'b', 0xa1, 0x61, b'x']);
    envelope.extend_from_slice(LINK);
    assert_eq!(links(&envelope).unwrap(), [target(), target()]);
}

#[test]
fn invalid_map_version_can_still_be_a_valid_standalone_list() {
    let empty = [0x82, 2, 0xa0]; // [2, {}]
    assert!(Map::decode(&empty).is_err());
    assert!(links(&empty).unwrap().is_empty());

    let mut standalone = vec![0x82, 2, 0xa1, 0x61, b'x'];
    standalone.extend_from_slice(LINK);
    assert!(Map::decode(&standalone).is_err());
    assert_eq!(links(&standalone).unwrap(), [target()]);
}

#[test]
fn map_envelope_does_not_consume_standalone_list_or_depth_budgets() {
    // [1, {"": [null; 16384]}] uses the full list budget inside the Map.
    let mut envelope = vec![0x82, 1, 0xa1, 0x60, 0x99, 0x40, 0];
    envelope.extend(std::iter::repeat_n(0xf6, MAX_LIST_ELEMENTS));
    assert!(links(&envelope).unwrap().is_empty());
    envelope[1] = 2;
    assert!(links(&envelope).is_err());

    let mut deepest = vec![0x81; MAX_DEPTH];
    deepest.push(0xf6);
    assert!(links(&deepest).unwrap().is_empty());
    deepest.insert(0, 0x81);
    assert!(links(&deepest).is_err());

    let mut envelope = vec![0x82, 1, 0xa1, 0x60];
    envelope.extend(std::iter::repeat_n(0x81, MAX_DEPTH - 1));
    envelope.push(0xf6);
    assert!(links(&envelope).unwrap().is_empty());
    envelope[1] = 2;
    assert!(links(&envelope).is_err());
}

#[test]
fn rejects_noncanonical_unsupported_and_unbounded_profile_data() {
    for malformed in [
        &[0xf6, 0xf6][..],                            // Trailing value.
        &[0x18, 0],                                   // Nonminimal integer.
        &[0xf9, 0, 0],                                // Float outside profile.
        &[0xa2, 0x61, b'a', 0, 0x61, b'a', 1],        // Duplicate map key.
        &[0xd8, 42, 0x47, 0, 1, 0x55, 0, 1, 7, 0],    // Trailing CID byte.
        &[0xd8, 42, 0x47, 0, 0x81, 0, 0x55, 0, 1, 7], // Nonminimal CID varint.
    ] {
        assert!(links(malformed).is_err(), "accepted {malformed:?}");
    }
    assert!(links(&vec![0xf6; MAX_BLOCK_BYTES + 1]).is_err());
}
