# Provider Routing and Service Discovery

## Overview

Wetware uses Kademlia provider records to announce and discover services on the
peer-to-peer network. Provider discovery is untrusted, like DNS. Applications
authenticate a provider after connection through a protocol such as Terminal
challenge-response.

Provider routing has three independent surfaces:

| Surface | Form | Authority |
|---------|------|-----------|
| `routing::Finder` | Cap'n Proto capability | Observe providers and consume bounded host/network resources |
| `routing::Announcer` | Cap'n Proto capability | Assert that the Wetware host PeerID provides a CID |
| `wetware:routing/key@0.1.0` | Optional WIT host import | Pure canonical CID derivation; no object-capability authority |

The host graft exports `Finder` as `routing-finder` and `Announcer` as
`routing-announcer`. A parent can delegate either reference without delegating
the other. The old broad `Routing` capability and the `routing` graft no longer
exist.

## Service discovery pattern

```text
key = wetware:routing/key.derive("price-oracle")

Provider: Announcer.provide(key)
Consumer: Finder.findProviders(key, count, providerSink)
```

Both Cap'n Proto methods accept canonical CID text. They use the CID multihash
as the Kademlia provider-record key. They do not hash application names
implicitly.

## `routing::Finder`

`Finder.findProviders(key, count, sink)` searches the WAN and LAN Kademlia
DHTs. `count` is the maximum number of unique provider PeerIDs delivered to
the sink.

- `count == 0` calls `sink.done()` without starting a network query.
- WAN and LAN results are deduplicated by PeerID.
- The host stops after it delivers `min(count, 16)` unique providers.
- One provider crosses the single-slot handoff to the Cap'n Proto sink at a
  time. The swarm retains selected observations while the slot is occupied.
  The pending set cannot exceed `min(count, 16)`. The 16-result host cap
  matches the configured Kademlia replication factor.
- A failed `sink.provider` callback cancels the WAN, LAN, and associated
  peer-routing queries. If the sink closes between callbacks, the next callback
  detects the closure. The deadline still bounds the query lifetime.
- Each call has a 30-second deadline that starts before swarm-command
  admission. Each admitted query uses a per-request cancellation token that
  does not consume command-channel capacity. Deadline expiry cancels remaining
  work and ends the call with the results already delivered.
- Epoch expiry cancels remaining work and fails the call with `staleEpoch`.

The DHT can return the same PeerID with different observations. The current
contract delivers a PeerID at most once, with the addresses available when the
host selects the provider for delivery.

## `routing::Announcer`

`Announcer.provide(key)` announces the Wetware host PeerID on both the WAN and
LAN Kademlia DHTs. The guest does not select another provider identity. Holding
`Finder` does not grant this authority.

Each `Announcer` server has one host-local owner lease. Repeated provision of
the same CID through the same lease is idempotent. If multiple leases own one
CID, Wetware keeps the local registration until the final owner ends.

Wetware releases an owner's registrations when its authority epoch ends. The
trusted PID0 generation scope also ends its associated lease. Server drop
requests the same cleanup. Final-owner release calls `stop_providing` on both
DHT behaviors. This removes the local provider record that drives future
republication.

Provider records already stored by other peers are not revoked immediately.
Those records expire under the remote DHT's TTL policy. With the current
`libp2p-kad` defaults, local records are republished every 12 hours and remote
provider records have a 48-hour TTL.

Provision succeeds after either WAN or LAN publication succeeds. Wetware keeps
the same owner for both local registrations, including when one initial network
query fails. If both queries fail, Wetware removes the failed ownership claim
and stops local provision when no other owner remains.

## Canonical routing keys

Components can opt into this WIT import:

```wit
package wetware:routing@0.1.0;

interface key {
    derive: func(data: list<u8>) -> string;
}

world key-client {
    import key;
}
```

`derive` applies this exact algorithm:

```text
input bytes
    -> BLAKE3-256
    -> multihash code 0x1e
    -> CIDv1 with raw codec 0x55
    -> canonical CID text
```

The host registers the import for ordinary and PID0 component linkers. A
component receives bindings only when it declares the import. Components that
omit the import instantiate normally and remain unaware of the interface.

The Rust guest wrapper is the `routing-key` crate. The canonical host helper is
`cell::routing_key::derive`. For example:

```text
derive("ww.chess.v1")
    = bafkr4ifcoue3f52zpzpz2xei7dqhs3gajm326llyljbwisxkwea7hbowyy
```

Routing-key derivation is deterministic computation. It has no `Membrane`,
`EpochGuard`, named graft, or delegable capability reference.

## Trust model

```text
DHT discovery (untrusted)       Vat transport and Terminal authentication
-------------------------       ------------------------------------------
Finder.findProviders(...)   ->  VatClient.dial(peer, protocol)
returns provider addresses      receive a fresh Terminal
                                Terminal.login(signer)
                                receive policy-selected service authority
```

Any node can claim to provide a key. `Finder` reports the claim. Terminal or
another application protocol establishes whether the caller trusts the peer.

## Removed guest operations

The breaking replacement removed these methods with the broad `Routing`
interface:

- `hash`: use the optional pure WIT import.
- `resolve`: no guest IPNS resolver is currently exposed.
- `publish`: no guest IPNS publisher is currently exposed.
- `mkdir`, `writeFile`, and `remove`: persistent UnixFS/MFS authoring is not a
  guest capability.

The WASI filesystem remains read-only for deployment/image content and
`/ipfs`, with a private ephemeral writable `/tmp`. These semantics do not
replace the removed persistent CID-transform operations.

Host-owned IPNS publication and IPNS Stem following are separate host
lifecycles. They continue to sign or validate raw records locally and use Kubo
HTTP Routing V1 only for transport:

```text
GET /routing/v1/ipns/{canonical-base36-name}
Accept: application/vnd.ipfs.ipns-record

PUT /routing/v1/ipns/{canonical-base36-name}
Content-Type: application/vnd.ipfs.ipns-record
```

The default signer is `~/.ww/identity`. Kubo and guest provider-routing
capabilities do not receive that private key.

## See also

- [`capnp/routing.capnp`](../capnp/routing.capnp) — `Finder`, `Announcer`, and `ProviderSink`
- [`crates/guest/routing-key/wit/key.wit`](../crates/guest/routing-key/wit/key.wit) — optional routing-key import
- [`doc/capabilities.md`](capabilities.md) — authority and delegation model
- [`doc/architecture.md`](architecture.md) — capability flow and epoch lifecycle
- [`doc/keys.md`](keys.md) — key management and identity
