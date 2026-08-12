# Capabilities

Wetware has no ambient node authority. This document covers the capability
model an agent receives at birth,
the membrane mechanism that enforces attenuation, and the four
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

## One enforcement substrate, four configuration surfaces

There is exactly one enforcement mechanism for capability policy that
must survive a boundary crossing: the hook-level membrane
(`crates/membrane`), which filters calls by `(interfaceId, ordinal)`
on the capability reference itself and recursively wraps capabilities
found in results and pipelines. `CallGuard`s compose lifecycle decisions
such as epoch expiry and targeted revocation with that method policy.
Everything else is configuration — four surfaces that decide *which
references exist where*:

| Surface | What it controls | How to change it |
|---------|------------------|------------------|
| **Initial grants** | Which RPC capability references enter an ordinary child | Edit the spawning `cell :grants` map; respawn |
| **Terminal authority policy** | Which verified login identity receives which method profile over one application capability | Publish with `host :serve-vat ... :auth policy`; the listener creates one Terminal per stream |
| **Image root / CAS wiring** | The fixed read-only root and optional known-CID reads | Select the execution context; respawn |
| **Glia bindings** | Names available while trusted Glia composes an authority graph | Edit init; they do not cross a child boundary by lexical capture |

Trusted pid0 receives the host graft; each ordinary child receives only its
immutable `InitialAuthorityRecord`, constructed from the parent’s explicit
grants and delivered through `InitialGrants.get()`. The root Atom binding flows
through `stem::Atom`. When the Atom value changes, the host first broadcasts an
authoritative epoch whose `root` is `None`. That broadcast invalidates the old
generation's capabilities and causes PID0 teardown before filesystem
preparation. The host composes the new head with the frozen boot overlays,
gates activation on the head and effective-root pins, pre-warms and swaps
`CidTree`, then broadcasts `root: Some(effective)`. The generation loop starts
PID0 only from the rooted broadcast. `WW_ROOT` is fixed when that generation
starts. The process-local graft and readiness gate use the same captured epoch
sequence, so a PID0 generation cannot activate with a root from one epoch and
authority from another epoch. Interactive `ww run` exits on the authority
broadcast instead of replacing the interactive process.

The Glia env layer binds capabilities such as `fs`, `routing`, and `host`.
Omitting a handler restricts access inside that cell. Env bindings and effect
handlers are not load-bearing across a process boundary. See
[designs/single-authority-capability-model.md](designs/single-authority-capability-model.md).

## Attenuation: `(attenuate cap [:method ...])`

Attenuating a capnp-backed capability constructs a hook-level membrane
around it. The returned cap enforces its method allowlist at the
capability reference itself, so the policy travels with the cap across
boundaries — insert it in a `cell :grants` map, publish it explicitly with
`(perform host :serve-raw-vat cap "svc")`, use it as the session template
for authenticated `host :serve-vat`, or hand it to another cell, and
callers on the far side are filtered even if they cast the reference to
a typed client. Denied methods fail closed with
`:glia.error/permission-denied`, locally and remotely.

- Method keywords are resolved against the cap's compiled schema
  (kebab-case keyword ↔ camelCase capnp name); unknown methods fail at
  attenuation time.
- Re-attenuation intersects: `(attenuate (attenuate c [:a :b]) [:b :c])`
  allows exactly `:b`, in a single membrane layer.
- Attenuation confines the entire reachable graph: capabilities returned
  by allowed methods come back wrapped in the same membrane, and the
  attenuate surface can only name methods on the attenuated cap's own
  interface — so transitively-reached caps (e.g. an `Executor` obtained
  through an attenuated `runtime`) are fully confined, fail-closed.
  Granting authority on transitive interfaces is a future surface
  extension.
- Caps with no compiled schema (e.g. obtained from a vat dial) cannot be
  attenuated yet — that requires the deferred schema-association design.
- Trusted FHS configuration can construct a policy-bound `Terminal` from
  compiled `(interfaceId, ordinal)` coordinates. Rust integrations should
  prefer typed `MethodProfile::allow_method` selectors. Typed capture prevents
  accidental ordinal mistakes; it is not a security boundary against
  malicious deployer code.
- `defcap` caps (pure Glia, cell-local) keep evaluator-local allowlist
  semantics: they cannot cross a boundary, so the local check is
  interposition within one trust domain, not boundary enforcement.

## Capabilities exposed at bootstrap

Trusted pid0 receives the host capabilities below. An ordinary child receives
only the named references its parent supplied in `cell :grants` (or the
equivalent `Executor.spawn` caps list). Each `Export` entry carries an inert
name and a capability reference; authority is carried by the reference, never
looked up from the name.

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
re-graft; pid0 reruns affected init and explicitly re-delegates fresh
references or respawns it. Epoch guards do not revoke arbitrary capability
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

Use regular guest file I/O against filesystem paths:
- `(perform :load "path")` for bytes in Glia
- `(perform import "module")` for module loading
- direct guest reads via WASI-aware code under `/ipfs/<cid>/...` and
  `/ipns/<name>/...`

There is no `perform fs` capability. `:load` is a named Glia effect whose
embedding handler performs the read; it makes the language-level host boundary
explicit without becoming an authority grant. The WASI virtual filesystem and
its reachable CID tree still govern guest path I/O, while the membrane governs
RPC capability authority.

Known-CID cache wiring is execution-context state, not a child-visible control
capability. A child cannot replace or widen it, and it provides no
CID enumeration, mutation, pin management, publishing, routing, arbitrary
dialing, ambient network API, or `ipfs` RPC capability.

This does not mean a CAS read has no node effect. A read can fetch over the
node's network, pin and materialize blocks on disk, occupy cache budget, affect
cache timing, and cause eviction/unpin work. Those bounded cache effects are
intentional substrate effects, not node-control authority.

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

1. Trusted pid0 grafts epoch-scoped host capabilities
2. A parent constructs each ordinary child’s exact named grant set
3. To gate a remotely published capability, trusted configuration attaches
   an explicit policy and publishes the resulting `Terminal(Session)`
4. An epoch advance stales host-issued guarded capabilities
5. The host prepares and pins the new effective root while readiness is closed
6. pid0 re-grafts, reruns affected init, and explicitly re-delegates fresh
   references or replaces affected children

## Revocation

A CID already handed to a cell remains copyable knowledge; it is not a
confidential capability. Whether it is readable depends on the applicable
image root and optional known-CID CAS wiring. RPC revocation works two ways:

- **Epoch advance.** `EpochGuard` (`crates/authority/src/epoch.rs`)
  invalidates every RPC capability bound to the old epoch. Method
  calls fail with `staleEpoch`. pid0 explicitly delegates fresh references or
  respawns the child; ordinary children have no ambient refresh path.
- **Targeted recipient revocation.** A per-recipient `RevocationGuard`
  invalidates already-issued RPC sessions for that policy decision without
  advancing the global epoch. Removing the policy binding also denies new
  logins. The guard mechanism is implemented, but generic `serve-vat`
  publication does not yet return a deployer-facing management capability
  that can trigger key-scoped revocation or replace bindings. Until that
  pre-alpha API lands, generic published services use epoch advance for
  operator-triggered invalidation.
- **Kill and respawn under a different root Atom.** New cell, new root
  CID, fresh CID graph. The old cell's content knowledge is gone with
  the old process.

Both lifecycle checks use the same call-guard substrate as method policy.
Atom remains the source and coordinator of global epoch lifecycle.

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

MCP mode preserves JSON-RPC stdout: `(perform :stdout value)` and
`(perform :exit nil)` are rejected with the typed
`:glia.error/protocol-mode-unavailable` error instead of writing or exiting.
`(perform :load path)` remains available when the embedding supplies an
appropriate loader.

## Cap'n Proto schemas

Schema definitions live in `capnp/`:

- **`system.capnp`** — Host, Runtime, Executor, Process, ByteStream,
  StreamListener, StreamDialer, VatListener, VatClient, HttpListener
- **`stem.capnp`** — Epoch and provenance metadata
- **`auth.capnp`** — Terminal, Signer, Identity, Authority policy constructor
- **`membrane.capnp`** — trusted-root Membrane, child InitialGrants, Export
- **`routing.capnp`** — Kademlia DHT (provide, findProviders, hash)
- **`http.capnp`** — HttpClient

Build scripts extract canonical `Schema.Node` bytes for the
`schema`/`doc`/`help` introspection builtins and schema CIDs. These bytes
are introspection inputs, not runtime sidecars: exported capabilities cross
membranes as bare capability references in `Export { name, cap }`.
