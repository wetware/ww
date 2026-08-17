# Capabilities

Wetware has no ambient node authority. This document covers the capability
model an agent receives at birth,
the membrane mechanism that enforces attenuation, and the three
configuration surfaces that determine what the agent can do.

For the host-side architecture (cell layout, epoch pipeline, layer
resolution), see [architecture.md](architecture.md).

## Content as capability

An image-backed cell's read-only filesystem is a `CidTree` rooted at its
image. A CID names content; when known-CID CAS wiring is present, that name is
a copyable bearer locator for readable content, not a confidential or
unforgeable authority reference. It can be shared and can have node resource
effects. The host does not give an ordinary child a general IPFS control
capability merely by making that read path available.

This makes WASI preopens a protocol detail, not a security boundary.
The host preopens `CidTree::staging_dir()` at `/` so the guest's WASI
implementation has a descriptor to anchor lookups against, but the
content the guest sees behind that descriptor is scoped by `CidTree`'s
root, not by the preopen.

## One enforcement substrate, three configuration surfaces

There is exactly one enforcement mechanism for capability policy that
must survive a boundary crossing: the hook-level membrane
(`crates/membrane`), which filters calls by `(interfaceId, ordinal)`
on the capability reference itself and recursively wraps capabilities
found in results and pipelines. `CallGuard`s compose lifecycle decisions
such as epoch expiry and targeted revocation with that method policy.
Everything else is configuration — three surfaces that decide *which
references exist where*:

| Surface | What it controls | How to change it |
|---------|------------------|------------------|
| **Initial grants** | Which RPC capability references enter an ordinary child | Set the `caps` list on `Executor.spawn` or listener registration; respawn |
| **Terminal authority policy** | Which verified login identity receives which method profile over one application capability | Publish with `VatListener.serveAuthenticated`; the listener creates one `Terminal` per stream |
| **Image root / CAS wiring** | The fixed read-only root and optional known-CID reads | Select the execution context; respawn |

Trusted pid0 receives the host graft; each ordinary child receives only its
immutable `InitialAuthorityRecord`, constructed from the parent's explicit
grants and delivered through `InitialGrants.get()`. The root Atom binding flows
through `stem::Atom`. When the Atom value changes, the host first broadcasts an
authoritative epoch whose `root` is `None`. That broadcast invalidates the old
generation's capabilities and causes PID0 teardown before filesystem
preparation. The host composes the new head with the frozen boot overlays,
gates activation on the head and effective-root pins, pre-warms and swaps
`CidTree`, then broadcasts `root: Some(effective)`. The generation loop starts
PID0 only from the rooted broadcast. `WW_ROOT` identifies the guest-visible
effective `CidTree` root for that generation. Explicit `/ipfs/<cid>/...` paths
remain literal content-addressed paths. The process-local graft and readiness
gate use the same captured epoch sequence, so a PID0 generation cannot
activate with a root from one epoch and
authority from another epoch. Interactive `ww run` exits on the authority
broadcast instead of replacing the interactive process.

## Hook-level attenuation

`crates/membrane` wraps a Cap'n Proto client hook with a `Policy`. The wrapped
capability enforces its method allowlist at the capability reference. The
policy therefore crosses process and vat boundaries with the reference. A
caller cannot bypass the policy by casting the reference to another typed
client.

- Rust integrations can construct an `Allowlist` directly or use
  `MethodProfile::allow_method` with generated request methods.
- Re-attenuation with static allowlists intersects the allowed method keys and
  collapses the result into one membrane layer.
- Capabilities returned by allowed methods are wrapped in the same membrane.
  The policy therefore confines the reachable capability graph.
- The schema resolver cannot build an allowlist for a capability without
  associated compiled schema metadata. That case requires the deferred
  schema-association design.
- Trusted configuration can construct a policy-bound `Terminal` from compiled
  `(interfaceId, ordinal)` coordinates. Typed capture avoids accidental
  ordinal mistakes; it is not a security boundary against malicious deployer
  code.

## Capabilities exposed at bootstrap

Trusted pid0 receives the host capabilities below. An ordinary child receives
only the named references its parent supplied in the `Executor.spawn` caps
list or a listener's registration-time grant template. Each `Export` entry
carries an inert name and a capability reference. The reference carries
authority; the name does not resolve authority.

| Capability | What it does |
|------------|--------------|
| **identity** | Host-side Ed25519 signing (private key never enters WASM) |
| **authority** | Construct a policy-bound `Terminal` over one explicit capability |
| **host** | Peer identity, listen addresses, connected peers, network access |
| **runtime** | Load WASM binaries and obtain scoped Executors (with compilation caching) |
| **routing** | Kademlia DHT: provide and find content/services |
| **http-client** | Outbound HTTP requests, gated by `--http-dial` allowlist |
Application-specific entries use their parent-chosen grant-map keys.

The wire-side `StreamListener` / `StreamDialer` / `VatListener` /
`VatClient` interfaces are reached via `host.network()` rather than
appearing as separate initial grants.

Host-issued delegated capabilities are epoch-guarded: they fail with
`staleEpoch` once the on-chain head advances. An ordinary child cannot
re-graft. The Host terminates the old PID0 and starts a fresh PID0 for the new
generation. Epoch guards do not revoke arbitrary capability
references that were not issued by the host.

### Content access (WASI path I/O only)

Cells do not receive an explicit filesystem capability over the membrane.
Filesystem substrate is fixed by the trusted execution context:

- an Executor created from `Runtime.load(wasm bytes)` has a private empty
  read-only root because those bytes have no associated FHS image;
- an image-backed cell retains its actual `CidTree`-rooted read-only image;
- each process has a separate writable `/tmp`, removed with that process and
  inaccessible through sibling filesystem namespaces;
- `/ipfs/<cid>` materializes only when the host explicitly supplies the
  pinset/cache wiring. Without it, the path fails instead of falling back to
  global host services.

Use regular WASI-aware guest file I/O against paths under the image root or
explicit `/ipfs/<cid>/...` paths. The WASI virtual filesystem and its reachable
CID tree govern guest path I/O. The membrane governs RPC capability authority.

Known-CID cache wiring is execution-context state, not a child-visible control
capability. A child cannot replace or widen it, and it provides no
CID enumeration, mutation, pin management, publishing, routing, arbitrary
dialing, ambient network API, or `ipfs` RPC capability.

This does not mean a CAS read has no node effect. A read can fetch over the
node's network, pin and materialize blocks on disk, occupy cache budget, affect
cache timing, and cause eviction/unpin work. Those bounded cache effects are
intentional substrate effects, not node-control authority.

### Content mutation (explicit capability API)

Writes go through the explicit `Routing` capability, not plain filesystem
reads.

- `Routing.mkdir(baseCid, path, parents)` returns a new root CID.
- `Routing.writeFile(baseCid, path, data, createParents)` returns a new root CID.
- `Routing.remove(baseCid, path, recursive)` returns a new root CID.
- `Routing.publish(name, cid, expectedCurrent)` returns the published IPFS path.

Semantics:
- Mutations are **CID-transform operations**: input root CID + operation -> output root CID.
- No hidden mutable global root is kept in the daemon.
- IPNS publish supports compare-and-set conflict checks via `expected-current`.

## Local overrides

Backend virtual mode rejects targeted mounts, so host-local overrides are
currently not part of the backend runtime surface. Publish content to IPFS/IPNS
and mount it as a root layer instead.

`LocalOverride` types remain as internal implementation scaffolding, but `ww
run` backend mount resolution does not use them in this mode.

## Capability lifecycle

1. Trusted pid0 grafts epoch-scoped host capabilities
2. A parent constructs each ordinary child's exact named grant set
3. To gate a remotely published capability, trusted configuration attaches
   an explicit policy and publishes the resulting `Terminal(Session)`
4. An epoch advance stales host-issued guarded capabilities
5. The host prepares and pins the new effective root while readiness is closed
6. The Host starts a fresh PID0 for the rooted generation

## Revocation

A CID already handed to a cell remains copyable knowledge; it is not a
confidential capability. Whether it is readable depends on the applicable
image root and optional known-CID CAS wiring. RPC revocation works two ways:

- **Epoch advance.** `EpochGuard` (`crates/authority/src/epoch.rs`)
  invalidates every RPC capability bound to the old epoch. Method
  calls fail with `staleEpoch`. The Host replaces PID0; ordinary children have
  no ambient refresh path.
- **Targeted recipient revocation.** A per-recipient `RevocationGuard`
  invalidates already-issued RPC sessions for that policy decision without
  advancing the global epoch. Removing the policy binding also denies new
  logins. The guard mechanism is implemented, but
  `VatListener.serveAuthenticated` does not yet return a deployer-facing
  management capability
  that can trigger key-scoped revocation or replace bindings. Until that
  pre-alpha API lands, generic published services use epoch advance for
  operator-triggered invalidation.
- **Kill and respawn under a different root Atom.** New cell, new root
  CID, fresh CID graph. The old cell's content knowledge is gone with
  the old process.

Both lifecycle checks use the same call-guard substrate as method policy.
Atom remains the source and coordinator of global epoch lifecycle.

## Cap'n Proto schemas

Schema definitions live in `capnp/`:

- **`system.capnp`** — Host, Runtime, Executor, Process, ByteStream,
  StreamListener, StreamDialer, VatListener, VatClient, HttpListener
- **`stem.capnp`** — Epoch and provenance metadata
- **`auth.capnp`** — Terminal, Signer, Identity, Authority policy constructor
- **`membrane.capnp`** — trusted-root Membrane, child InitialGrants, Export
- **`routing.capnp`** — Kademlia DHT (provide, findProviders, hash)
- **`http.capnp`** — HttpClient

Build scripts extract canonical `Schema.Node` bytes and schema CIDs. Exported
capabilities cross membranes as bare references in `Export { name, cap }`.
