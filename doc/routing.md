# Routing & Service Discovery

## Overview

Wetware uses Kademlia DHT for **content routing** — announcing and discovering
service providers on the peer-to-peer network. The DHT is untrusted discovery
(like DNS); authentication happens post-connection via Terminal challenge-response.

## Capabilities

The `Routing` capability (obtained via `membrane.graft()`) provides:

| Method | Shell syntax | Description |
|--------|-------------|-------------|
| `provide` | `(perform routing :provide "name")` | Hash name to CID, announce this node as a provider |
| `find` | `(perform routing :find "name" [:count N])` | Discover providers for a name (default 20) |
| `hash` | `(perform routing :hash "data")` | Compute CID from data (advanced — `provide` hashes internally) |
| `resolve` | `(perform routing :resolve "k51...")` | Resolve an IPNS name to `/ipfs/<cid>` |
| `mkdir` | `(perform routing :mkdir "<base-cid>" "path" true)` | Create directory in a derived UnixFS root; returns new root CID |
| `write-file` | `(perform routing :write-file "<base-cid>" "path" "data" true)` | Write file content in a derived UnixFS root; returns new root CID |
| `remove` | `(perform routing :remove "<base-cid>" "path" true)` | Remove path from a derived UnixFS root; returns new root CID |
| `publish` | `(perform routing :publish "ww" "<cid>" "/ipfs/<expected>")` | Publish CID to IPNS with optional CAS guard |

All methods are epoch-guarded: they fail with `staleEpoch` when the on-chain head
advances, forcing a re-graft.

## Service discovery pattern

```clojure
;; Node A: announce as a price oracle
(perform routing :provide "price-oracle")

;; Node B: discover price oracles
(perform routing :find "price-oracle")
;; → [{:peer-id "12D3KooW..." :addrs ["/ip4/1.2.3.4/tcp/2025" ...]} ...]
```

Names are plain strings — no namespace convention required. Internally, the name
is hashed to a CIDv1 (SHA-256, raw codec) which becomes the DHT key.

## Trust model

```
DHT discovery (untrusted)          Terminal auth (trusted)
─────────────────────────          ──────────────────────
(perform routing :find "oracle")  →  (perform host :connect addr)
  returns peer addresses            establish RPC connection
                                    Terminal.login(signer)
                                    verify identity
                                    receive Membrane
```

The DHT is a **public bulletin board** — any node can announce as a provider for
any name. Discovery tells you *who claims to offer a service*. Terminal
challenge-response tells you *whether you trust them*.

## Key format

Provider keys are CIDv1 hashes (SHA-256, raw codec) of the service name string.
The shell's `(perform routing :provide "name")` and `(perform routing :find "name")` handle hashing
internally. The raw `(perform routing :hash "data")` method is exposed for advanced use
cases (e.g. content-addressed lookups).

## Mutation semantics

Write operations are **CID-transform** operations:

1. Input: base root CID
2. Apply one mutation (`mkdir`, `write-file`, or `remove`)
3. Output: new root CID

No hidden mutable global root is kept in the daemon.

For IPNS updates, `publish` supports compare-and-set semantics:
if `expected-current` is provided and does not match the currently
resolved head, the call fails instead of silently overwriting.

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
