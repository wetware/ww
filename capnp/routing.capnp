# Narrow provider-routing capabilities backed by the in-process Kademlia client.
#
# Finder and Announcer are independent object capabilities. Canonical routing-key
# derivation is a pure Component Model import, not a Cap'n Proto capability.
#
# Both capabilities are epoch-scoped. The host wraps each implementation with an
# EpochGuard so calls fail after the authority epoch advances.

@0xbb7178bb658e44b6;

struct ProviderInfo {
  peerId @0 :Data;       # Serialized libp2p PeerID.
  addrs  @1 :List(Data); # Serialized multiaddrs for the provider.
}

interface ProviderSink {
  provider @0 (info :ProviderInfo) -> ();
  # Called once per unique provider. Each response applies backpressure and
  # reports sink closure before Finder requests another result.

  done @1 ();
  # Signals that the finite lookup has completed.
}

interface Finder @0xebb8ace9d47ae6a8 {
  findProviders @0 (key :Text, count :UInt32, sink :ProviderSink) -> ();
  # `count` is the requested maximum number of unique provider peers.
  # A count of zero launches no network query and returns no peers.
  # Wetware applies a host maximum of 16 provider results.
}

interface Announcer @0xf52674c78f631b2f {
  provide @0 (key :Text) -> ();
  # Announce the Wetware host PeerID as a provider for the CID. Local provider
  # registration and republication stop after the owning authority epoch ends.
}
