//! Canonical pure routing-key derivation.

use cid::Cid;

const RAW_CODEC: u64 = 0x55;
const BLAKE3_256_MULTIHASH_CODE: u64 = 0x1e;

/// Derive the canonical CIDv1/raw/BLAKE3-256 routing key for `data`.
#[must_use]
pub fn derive(data: &[u8]) -> Cid {
    let digest = blake3::hash(data);
    let multihash =
        cid::multihash::Multihash::<64>::wrap(BLAKE3_256_MULTIHASH_CODE, digest.as_bytes())
            .expect("BLAKE3-256 digest fits the routing-key multihash");
    Cid::new_v1(RAW_CODEC, multihash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_exact_canonical_golden_vectors() {
        let vectors: &[(&[u8], &str)] = &[
            (
                b"",
                "bafkr4ifpcne3t5pzugtkaqcn5i3nzskjtpfslsnnyejlpte2spfoihzsmi",
            ),
            (
                b"ww.chess.v1",
                "bafkr4ifcoue3f52zpzpz2xei7dqhs3gajm326llyljbwisxkwea7hbowyy",
            ),
            (
                b"wetware",
                "bafkr4id4izs4xqbbbmlhwbc7gu5bl3ogbclvqyepzdqfxgjpyoovqmqtnm",
            ),
        ];

        for (input, expected) in vectors {
            assert_eq!(derive(input).to_string(), *expected);
        }
    }
}
