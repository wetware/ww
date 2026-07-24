# Stem schema: epoch/provenance definitions.
# Compiled by the authority crate (crates/authority/build.rs).
# The host re-exports generated types via `pub use authority::stem_capnp`.

@0x9bce094a026970c4;

struct Epoch {
  # An epoch anchors a point-in-time snapshot of a namespace's content root.
  # The seq field is monotonically increasing regardless of the source backend.
  # The provenance union carries backend-specific metadata about when and how
  # the epoch was adopted.
  #
  # stem::atomic  — on-chain via Atom contract; provenance carries the block
  #                 number at which the HeadUpdated event was finalized.
  # stem::eventual — off-chain via IPNS; provenance carries the wall-clock
  #                  timestamp (Unix seconds) from the IPNS record validity.

  seq @0 :UInt64;        # Monotonic epoch sequence number.
  head @1 :Data;         # Content root (CID bytes).

  provenance :union {
    block @2 :UInt64;    # stem::atomic — Ethereum block number at adoption.
    timestamp @3 :UInt64;# stem::eventual — Unix timestamp of IPNS record.
  }
}
