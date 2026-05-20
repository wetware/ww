# Capabilities

Wetware is capability-secure all the way down: there is no ambient
authority, and content access is gated by which CIDs a cell can reach.
This document covers the capability model an agent sees after grafting,
plus the three attenuation points that determine what the agent can do.

For the host-side architecture (cell layout, epoch pipeline, layer
resolution), see [architecture.md](architecture.md).

## Content as capability

Wetware's filesystem is the IPFS UnixFS DAG, exposed to guests through
the WASI virtual filesystem (`CidTree` in `src/vfs.rs`). Every reachable
path resolves to a CID, and a CID is an unforgeable cryptographic hash
of its content. This collapses two ideas that are usually separate:

- **CIDs are sturdyrefs.** You cannot guess a CID for content you don't
  have. If someone hands you a CID, they have effectively granted you
  the ability to fetch that content. If they don't, you can't discover
  it through the filesystem. This is the classical object-capability
  property: an unforgeable reference IS the grant of access.

- **The "filesystem" is the reachable CID subgraph.** Each cell starts
  rooted at a particular CID (its image root). It can read anything
  walkable from that root, plus any CIDs handed to it over RPC at
  runtime. There's no path-based permission model — there's only
  "what CIDs does this cell know."

This makes WASI preopens a protocol detail, not a security boundary.
The host preopens `CidTree::staging_dir()` at `/` so the guest's WASI
implementation has a descriptor to anchor lookups against, but the
content the guest sees behind that descriptor is scoped by `CidTree`'s
root, not by the preopen.

## Three attenuation points

Every capability a cell holds comes from one of three places. There is
no fourth.

| Layer | What it controls | How to change it |
|-------|------------------|------------------|
| **Membrane graft** | RPC capabilities (`host`, `runtime`, `routing`, `identity`, `http-client`, plus `with`-block grants) | Edit the init.d `with` block; regraft |
| **Root Atom binding** | The cell's root CID — the initial reachable content subgraph | Bind the cell to a different `stem::Atom`; respawn |
| **Glia env bindings** | Which capabilities are callable inside the cell (`fs`, `routing`, `host`, …) and via what names | Edit init.d to bind/unbind names; re-eval |

The membrane graft is the canonical RPC surface
(`src/rpc/membrane.rs:HostGraftBuilder`). The root Atom binding flows
through `stem::Atom` — when the Atom's value changes, `CidTree`'s root
swaps atomically (`src/vfs.rs:CidTree::swap_root`), and old CIDs the
cell had cached in memory still resolve to whatever they pointed to,
but new walks see the new tree. The Glia env layer is where capabilities
like `fs`, `routing`, and `host` are bound — restricting access at this
layer is as simple as not installing the handler.

## Capabilities exposed to grafted agents

After calling `membrane.graft()`, an agent holds references to a list
of named `Export`s (`stem.capnp:Export`). Each entry carries the cap
name, a typed client, and the canonical `Schema.Node` bytes that
describe the cap's interface (so consumers can introspect without
hardcoded fallbacks).

| Capability | What it does |
|------------|--------------|
| **identity** | Host-side Ed25519 signing (private key never enters WASM) |
| **host** | Peer identity, listen addresses, connected peers, network access |
| **runtime** | Load WASM binaries and obtain scoped Executors (with compilation caching) |
| **routing** | Kademlia DHT: provide and find content/services |
| **http-client** | Outbound HTTP requests, gated by `--http-dial` allowlist |
| `with`-scoped extras | Init.d-granted caps (e.g. `auction`, application-specific RPC interfaces). Each carries its own `Schema.Node` so guests can introspect. |

The wire-side `StreamListener` / `StreamDialer` / `VatListener` /
`VatClient` interfaces are reached via `host.network()` rather than
appearing in the top-level graft list.

Every capability is epoch-guarded: it fails with `staleEpoch` once the
on-chain head advances, forcing a re-graft.

### Content access (WASI path I/O only)

Cells do not receive an explicit filesystem capability over the
membrane. Content access flows through the WASI virtual filesystem,
which the host backs with `CidTree`.

Use regular guest file I/O against filesystem paths:
- `(load "path")` for bytes in Glia
- `(perform import "module")` for module loading
- direct guest reads via WASI-aware code under `/ipfs/<cid>/...` and
  `/ipns/<name>/...`

There is no `perform fs` read surface. Keeping a separate `perform`
filesystem API created dual-path semantics; reads now go only through
WASI path I/O.

### Content mutation (explicit capability API)

Writes are effectful and go through `routing`, not plain filesystem reads.

- `routing :mkdir <base-cid> <path> [parents?]` -> `new-root-cid`
- `routing :write-file <base-cid> <path> <bytes-or-string> [create-parents?]` -> `new-root-cid`
- `routing :remove <base-cid> <path> [recursive?]` -> `new-root-cid`
- `routing :publish <ipns-name> <cid> [expected-current]` -> `/ipfs/<cid>`

Semantics:
- Mutations are **CID-transform operations**: input root CID + operation -> output root CID.
- No hidden mutable global root is kept in the daemon.
- IPNS publish supports compare-and-set conflict checks via `expected-current`.

## Local overrides

Backend virtual mode rejects targeted mounts, so host-local overrides are
currently not part of the backend runtime surface. Publish content to IPFS/IPNS
and mount it as a root layer instead.

`LocalOverride` types remain in the codebase as implementation scaffolding for
future shell-local workflows, but they are not used by `ww run` backend mount
resolution in this mode.

## Capability lifecycle

1. Agent calls `membrane.graft()` to receive epoch-scoped capabilities
2. Having a Membrane reference IS authorization (ocap model)
3. To gate access for remote peers, wrap the Membrane in a
   `Terminal(Membrane)` challenge-response auth layer
4. When the on-chain epoch advances, all capabilities are revoked
5. Agents re-graft, picking up the new state automatically

## Revocation

You cannot un-hand a CID. This is classical ocap semantics — once a
cell knows a CID, it can fetch the content. Revocation works two ways:

- **Epoch advance.** `EpochGuard` (`crates/membrane/src/epoch.rs`)
  invalidates every RPC capability bound to the old epoch. Method
  calls fail with `staleEpoch`. The cell must re-graft.
- **Kill and respawn under a different root Atom.** New cell, new root
  CID, fresh CID graph. The old cell's content knowledge is gone with
  the old process.

For RPC caps you can also wrap with a revocable proxy and drop the
proxy's reference. That's a runtime construct, not a property of
the membrane.

## Structured errors

Glia errors are values: `eval` returns `Result<Val, Val>`, and the
error type is itself a `Val::Map` with namespaced keyword keys
(`crates/glia/src/error.rs`). The canonical schema:

```clojure
{ :glia.error/type     <namespaced keyword>   ; e.g. :glia.error/arity-mismatch
  :glia.error/message  <string>               ; human-readable
  :glia.error/hint     <optional string>      ; recovery suggestion
  ;; ...variant-specific fields
  ;; (:glia.error/symbol, :glia.error/function, :glia.error/expected, etc.)
}
```

Variants exist for the cases that show up in real eval failures:
`parse`, `unbound-symbol`, `arity-mismatch`, `type-mismatch`,
`cap-call-failed`, `rpc-error`, `epoch-expired`, `permission-denied`,
`fuel-exhausted`, `internal`. There is no `generic` variant — every
error site picks a real tag.

Inspection accessors mirror Clojure's `ex-data` / `ex-message`:

- `glia::error::data(err) -> Option<&ValMap>`
- `glia::error::message(err) -> Option<&str>`
- `glia::error::type_tag(err) -> Option<&str>`
- `glia::error::hint(err) -> Option<&str>`

Plain-string and unstructured errors return `None` from each accessor,
distinguishing structured errors from foreign / legacy values.

The MCP cell preserves error `Val`s end-to-end and surfaces them to
JSON-RPC as `structuredContent`, so MCP clients can route on
`:glia.error/type` and act on variant-specific fields without parsing
the human-readable message.

### Errors as effects

Errors are an effect with target `:glia.exception`. `(throw err)`
performs the effect; `(try EXPR (catch :tag e BODY) ...)` installs a
handler that dispatches on `:glia.error/type`. With no handler in
scope, an unhandled throw escapes eval as `Err(Val::Effect{
effect_type: "glia.exception", data: <err> })` — outer callers
(kernel REPL, MCP cell, shell) unwrap via `glia::error::unwrap_thrown`.

```clojure
(try (compute-something)
  (catch :glia.error/unbound-symbol e (recover-unbound e))
  (catch :glia.error/cap-call-failed e (retry e))
  (catch _ e (rethrow-as-internal e)))
```

User code constructs structured errors via the `ex-info` builtin:

```clojure
(throw (ex-info "peer unreachable" {:type :network :peer "QmFoo"}))
;; catchable as (catch :network e ...) — `:type` becomes
;; `:glia.error/type` while remaining preserved for back-compat readers.
```

## Introspection

Three Glia builtins return data about caps an agent holds. They are
registered by the kernel after graft (`std/kernel/src/lib.rs`):

- `(schema cap)` returns the cap's canonical `Schema.Node` bytes as
  `Val::Bytes`. An MCP agent can parse this to enumerate methods,
  parameter types, and return types without hardcoded knowledge.
- `(doc cap)` returns a human-readable summary string (cap name,
  schema CID, one-line description).
- `(help cap)` returns a multi-line cap reference (name, schema CID,
  schema byte count, usage hint, pointers to `(schema cap)` /
  `(doc cap)`).

All three reject non-cap arguments via `:glia.error/type-mismatch` and
unknown caps via `:glia.error/permission-denied`, propagating typed
errors end-to-end.

## MCP = Glia eval

The MCP cell exposes `eval` as the universal primitive, plus per-cap
sugar tools (`host`, `routing`, `runtime`, ...) that translate to
internal Glia expressions for client convenience. There is no
`resources/*` or `prompts/*` surface — the attenuation surface should
be one thing, the Glia env, and adding parallel protocols would mean
gating each separately.

An AI agent connects, sees the per-cap tools in `tools/list` (each
backed by accurate descriptions derived from `Schema.Node` bytes),
calls `eval` with a Glia expression, and gets back either a result or
a structured error it can route on. Restrict the agent's capabilities
by editing the env it sees, not by adding ACLs to MCP itself.

## Cap'n Proto schemas

Schema definitions live in `capnp/`:

- **`system.capnp`** — Host, Runtime, Executor, Process, ByteStream,
  StreamListener, StreamDialer, VatListener, VatClient, HttpListener
- **`stem.capnp`** — Terminal, Membrane, Epoch, Signer, Identity,
  Export
- **`routing.capnp`** — Kademlia DHT (provide, findProviders, hash)
- **`http.capnp`** — HttpClient

Compiled schemas (`.capnpc` files) are the binary form consumed at
runtime: `crates/membrane/build.rs` extracts canonical `Schema.Node`
bytes for the core caps and `crates/membrane/src/schema_registry`
exposes them. The bytes flow into `Export.schema` at graft time (see
`src/rpc/membrane.rs:write_schema_for_core_cap`), so guests can
introspect every cap without hardcoded fallbacks.
