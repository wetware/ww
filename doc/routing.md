# Routing & Service Discovery

## Overview

Wetware uses Kademlia DHT for **content routing** — announcing and discovering
service providers on the peer-to-peer network. The DHT is untrusted discovery
(like DNS); authentication happens post-connection via Terminal challenge-response.

## Capabilities

The `Routing` capability (explicitly delegated to an ordinary child and read
via `initial_grants.get()`) provides:

| Method | Parameters | Description |
|--------|------------|-------------|
| `provide` | `key` | Announce this node as a provider for a CID key |
| `findProviders` | `key`, `count`, `sink` | Stream provider records to `ProviderSink` |
| `hash` | `data` | Compute a CIDv1 raw SHA-256 key from bytes |
| `resolve` | `name` | Resolve an IPNS name to `/ipfs/<cid>` |
| `mkdir` | `baseCid`, `path`, `parents` | Create a directory in a derived UnixFS root |
| `writeFile` | `baseCid`, `path`, `data`, `createParents` | Write a file in a derived UnixFS root |
| `remove` | `baseCid`, `path`, `recursive` | Remove a path from a derived UnixFS root |
| `publish` | `name`, `cid`, `expectedCurrent` | Publish a CID to IPNS with an optional compare-and-set guard |

All methods are epoch-guarded. They fail with `staleEpoch` when the on-chain
head advances. The Host terminates the old PID0 and starts a fresh PID0 for the
new generation. Ordinary children cannot refresh their grants.

## Service discovery pattern

```text
key = Routing.hash("price-oracle")

Node A: Routing.provide(key)
Node B: Routing.findProviders(key, count, providerSink)
```

Application service names are plain strings. Call `Routing.hash` to derive the
CID key used by `provide` and `findProviders`.

## Trust model

```
DHT discovery (untrusted)          Vat transport and Terminal auth
─────────────────────────          ──────────────────────
Routing.findProviders(key, ...)  →  VatClient.dial(peer, protocol)
  returns peer addresses            receive a fresh Terminal
                                    Terminal.login(signer)
                                    receive policy-selected service cap
```

The DHT is a **public bulletin board** — any node can announce as a provider for
any name. Discovery tells you *who claims to offer a service*. Terminal
challenge-response tells you *whether you trust them*.

## Key format

Provider keys are CIDv1 hashes. `Routing.hash` computes a raw-codec SHA-256 CID
from application bytes. `provide` and `findProviders` accept the resulting key;
they do not hash service names implicitly.

## Mutation semantics

Write operations are **CID-transform** operations:

1. Input: base root CID
2. Apply one mutation (`mkdir`, `write-file`, or `remove`)
3. Output: new root CID

No hidden mutable global root is kept in the daemon.

For IPNS updates, `publish` supports compare-and-set semantics:
if `expected-current` is provided and does not match the currently
resolved head, the call fails instead of silently overwriting.

## Host IPNS records and guest `Routing.publish`

The default host publisher and an IPNS Stem do not use the guest `Routing`
capability. They use `src/ipns.rs` to sign or validate raw records locally and
use Kubo only for HTTP Routing V1 transport:

```text
GET /routing/v1/ipns/{canonical-base36-name}
Accept: application/vnd.ipfs.ipns-record

PUT /routing/v1/ipns/{canonical-base36-name}
Content-Type: application/vnd.ipfs.ipns-record
```

The default signer is `~/.ww/identity`. Kubo does not receive that private key.
Follower and publisher records remain private under `~/.ww/ipns/` and outside
the guest image root.

Guest-visible `Routing.publish` is unchanged in this phase. It still calls
Kubo `name/resolve` for its optional compare-and-set check and Kubo
`name/publish` with the supplied key name. It does not use the host-owned
publisher state or grant access to `~/.ww/identity`. Redesign of that capability
remains deferred to ARCH-13.

## Limitations

- **Content routing only.** No key-value store (`putValue`/`getValue`) — deferred.
- **No DHT hardening.** Namespace collision protection and CID-based verification
  deferred to when TEE attestation lands.
- **Provider records expire.** Kademlia provider records have a TTL (default 24h in
  libp2p). Long-running services should re-provide periodically.

## See also

- [`capnp/routing.capnp`](../capnp/routing.capnp) — Schema definition
- [`doc/architecture.md`](architecture.md) — Capability flow and epoch lifecycle
- [`doc/keys.md`](keys.md) — Key management and identity
