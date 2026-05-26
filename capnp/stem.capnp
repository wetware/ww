# Stem schema: Epoch, Signer, Identity, and Membrane definitions.
# Compiled by the membrane crate (crates/membrane/build.rs).
# The host re-exports generated types via `pub use membrane::stem_capnp`.

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

interface Signer {
  sign @0 (nonce :UInt64, epochSeq :UInt64) -> (sig :Data);
  # Sign a challenge binding (nonce, epochSeq).  The signed payload is
  # nonce.to_be_bytes() || epochSeq.to_be_bytes() (16 bytes total).
  # The random nonce prevents replay within the same epoch; the epochSeq
  # prevents cross-epoch reuse (a signature from epoch N is invalid at N+1).
  # The domain and payload_type are baked in by the issuing Identity hub.
}

interface Identity {
  # Returns a Signer scoped to the requested signing domain.
  signer @0 (domain :Text) -> (signer :Signer);

  verify @1 (data :Data, signature :Data, pubkey :Data) -> (valid :Bool);
  # Verify an Ed25519 signature against an arbitrary public key.
  # Stateless — does not use the node's private key.
  # The pubkey is the 32-byte Ed25519 verifying key.
  # The signature is the 64-byte Ed25519 signature.
}

interface Terminal(Session) {
  login @0 (signer :Signer) -> (session :Session);
  # Authenticate via epoch-bound challenge-response.  The Terminal generates
  # a random nonce + current epoch seq, the Signer signs both, and the
  # Terminal verifies the signature, nonce, epoch freshness, and auth policy.
  # Having a Terminal reference does NOT grant access — the caller must prove
  # identity by signing the challenge with the expected key.
}

using Schema = import "/capnp/schema.capnp";

struct Export {
  name   @0 :Text;
  cap    @1 :Capability;
  schema @2 :Schema.Node;
  # An exported capability with its schema for runtime introspection.
  # name: canonical name (e.g. "host", "identity", "runtime").
  # cap: the capability interface reference.
  # schema: Cap'n Proto schema node describing the interface.
}

interface Membrane {
  graft @0 () -> (
    caps :List(Export)         # All capabilities, named and type-erased.
  );
  # Pure capability provisioning (ocap model). Having a Membrane reference IS
  # authorization — no signer needed. Wrap in Terminal(Membrane) to gate access.
  #
  # Canonical names: "identity", "host", "runtime", "routing", "http-client", "ipfs".
  # Init.d-scoped grants (from `with` blocks) are appended after the core caps.
  #
  # Listener/Dialer accessed via host.network().
  # WASI guests resolve content via the virtual filesystem (CidTree).
  # Non-WASI clients (for example process-local `ww shell`) may also receive
  # the `ipfs` cap and call `system.Ipfs.read` for `/ipfs`/`/ipns`/`/ipld`.
}
