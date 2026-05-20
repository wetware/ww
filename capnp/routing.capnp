# Content routing capability backed by the in-process Kademlia client.
#
# Mirrors Go's coreiface.RoutingAPI (provide/findProviders only).
# Data transfer flows through the WASI virtual filesystem, not a capability.
# DHT key-value store (putValue/getValue) is deferred.
#
# Epoch-scoped: the host wraps the implementation with an EpochGuard so all
# methods fail with stale-epoch once the epoch advances.

@0xa7c3e8f1d4b29065;

struct ProviderInfo {
  peerId @0 :Data;       # libp2p peer ID, serialized.
  addrs  @1 :List(Data); # Multiaddrs for this provider, each serialized.
}

interface ProviderSink {
  provider @0 (info :ProviderInfo) -> stream;
  # Called once per discovered provider.  -> stream enables
  # Cap'n Proto flow control (backpressure).

  done @1 ();
  # Signals that the search is complete.  Errors from earlier
  # provider() calls surface here.
}

interface Routing {
  provide @0 (key :Text) -> ();
  # Announce this node as a provider for the given CID.

  findProviders @1 (key :Text, count :UInt32, sink :ProviderSink) -> ();
  # Stream providers for a CID into the caller-supplied sink.

  hash @2 (data :Data) -> (key :Text);
  # Compute a deterministic CID (v1, raw codec, sha256) from data.
  # Local operation — does not touch the network or Kubo.

  resolve @3 (name :Text) -> (path :Text);
  # Resolve an IPNS name to an IPFS path via Kubo.
  # Returns e.g. "/ipfs/bafyrei..."

  mkdir @4 (baseCid :Text, path :Text, parents :Bool) -> (rootCid :Text);
  # Build a new UnixFS directory root by creating `path` relative to
  # `baseCid`. Returns the new root CID. No global mutable root is used.

  writeFile @5 (baseCid :Text, path :Text, data :Data, createParents :Bool) -> (rootCid :Text);
  # Build a new UnixFS root by writing file bytes at `path` relative to
  # `baseCid` (overwrite if present). Returns the new root CID.

  remove @6 (baseCid :Text, path :Text, recursive :Bool) -> (rootCid :Text);
  # Build a new UnixFS root by removing `path` relative to `baseCid`.
  # Returns the new root CID.

  publish @7 (name :Text, cid :Text, expectedCurrent :Text) -> (publishedPath :Text);
  # Publish `/ipfs/<cid>` under IPNS `name`.
  #
  # Conflict semantics:
  # - if `expectedCurrent` is empty, publish unconditionally.
  # - if set, `name` must currently resolve to `expectedCurrent` (or fail).
  #   This is a compare-and-set guard to avoid silent last-write-wins.
}
