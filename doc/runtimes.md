# Runtimes

This document covers the tokio runtime layout inside `ww`. For the
overall thread-per-subsystem rationale, see
[`src/services.rs`](../src/services.rs); for the on-the-wire RPC
plumbing, see [rpc-transport.md](rpc-transport.md); for the P3 guest
execution model, see [guest-runtime.md](guest-runtime.md). The pinned Wasmtime
filesystem hook and its removal gates are documented in
[dependencies/wasmtime.md](dependencies/wasmtime.md).

## Thread-per-subsystem

`ww` follows a Pingora-style "thread per subsystem" model. The
[`Host`](../src/services.rs) supervisor spawns one OS thread per long-
running subsystem, and each subsystem owns a tokio `Runtime`
co-located on that thread. Subsystems coordinate via channels, not
shared runtimes.

| Subsystem | Thread name | Runtime | Why |
|---|---|---|---|
| Deployment coordinator and Stem follower | main `ww` runtime workers | `multi_thread` | Root preparation, source polling, and kernel teardown overlap without a separate service thread |
| Libp2p swarm | `swarm` (+ `ww-swarm-worker-*`) | `multi_thread` | TLS handshake parallelism — see below |
| Executor pool worker (×N) | `executor-N` | `current_thread` + `LocalSet` | `wasmtime::Store` is `!Send` |

## Filesystem lifecycle runtime

The CLI creates one `HostFsOwner` from the main Tokio runtime before it creates
private filesystem state. Its `HostResources` is the sole authority propagated
through the executor pool, runtime clients, deployment staging, `CidTree`, the
shared pin cache, and every kernel or ordinary Cell. Each Cell reserves one
cleanup root before process construction and has its own logical-I/O limit;
Host logical, foreground-producer, and cleanup-root limits remain finite.

The production `HostLimits` policy admits 128 logical filesystem operations
(`H=128`) and 33 backend producers: 32 foreground lanes (`F=32`) plus one
dedicated cleanup-active lane (`C=1`). Each Cell has a 16-operation logical
limit (`L=16`). The Host also provides 128 cleanup roots (`K=128`) as a finite
beta limit on simultaneously live cleanup obligations, not disk bytes. The
pool is shared by the deployment root, the shared `PinsetCache` root, live
Cell/process roots, and staged CID generations. It has no reserved staging
partition, so any of these consumers can exhaust it. `H=128` does not itself
bound idle content owners: a Cell root or staged generation can remain live
after its logical operation and foreground work have finished.

Current production construction has two long-lived non-Cell roots: deployment
and the shared `PinsetCache`. With `C_live` live Cell/process roots, this leaves
`126 - C_live` slots for staged or retiring generations, absent any additional
roots. For example, one full foreground wave of 32 active generations can
overlap 32 retiring generations while `C_live <= 62` because
`2 + C_live + 32 + 32 <= 128`. This is an illustrative supported allocation,
not an enforced reservation or partition and not an independent maximum-Cell
guarantee. Idle Cells, native content owners, or retiring generations can use
the shared slots in a different mix. When the pool is exhausted, new staging
is rejected through the bounded admission path and surfaces as `Busy`.

Each staged CID generation reserves one cleanup root before creating its
private directory and holds that slot from generation creation through terminal
reclamation. Cleanup-root exhaustion can therefore reject staging before the
ARC byte target is reached. The rejection occurs before generation-private
filesystem effects and follows the existing `CleanupObligation` admission path,
which guest-visible filesystem work maps to `Busy`; Wetware never falls back to
untracked staging.

Staged subpaths are retained at CID-generation granularity. A live
`StagedContentLease`, descriptor, or native handle for any path in a generation
retains the entire staged generation, including materialized siblings and
ancestors, until the final owner closes. This avoids unsafe ancestor deletion
and a path-level ownership graph. Every generation has a physically unique,
nonreused directory, so delayed cleanup captures only the retired generation
and cannot delete a later admission of the same CID.

ARC residency is a separate claim. The production 128 MiB ARC target is a
`PinEntry` weight target; it is neither resident staging bytes nor a hard disk
quota. ARC eviction drops only the `PinEntry` generation claim. Accepted
publishers and Wasmtime native handles retain staged content independently, so
oversized ARC-unresident content can still be materialized and used. Releasing
the final content owner makes exact-generation cleanup eligible, providing
eventual safe reclamation rather than a strict staging-byte bound.

Wasmtime's `FilesystemOperation` owns active native work and admission, while
`FilesystemHandle` carries native `File`/`Dir` authority plus the opaque
`StagedContentLease` owner. An idle descriptor retains its generation without
holding active-operation capacity. See
[dependencies/wasmtime.md](dependencies/wasmtime.md) for the exact v48 backport
and propagation contract.

Foreground filesystem work submitted from an executor worker runs through the
Host driver captured from the main runtime. Ready-root cleanup uses one
dedicated cleanup thread and a separate cleanup lane, so saturating the main
runtime's blocking pool or all foreground producer permits cannot prevent
mandatory cleanup from progressing. Executor workers retain admitted
transactions through native-resource installation or disposal; a returned
value does not release its work receipt early.

Cell-owned CID directory construction observes its exact filesystem root closing.
It stops at native checkpoints and rolls back private staging. Commit acceptance
is ordered with closure under the root lifecycle mutex; an accepted non-replacing
rename finishes without holding that mutex. Already-started native work and
mandatory rollback retain their transaction until return and result disposal.
The descriptor tail skips work after observed cancellation. This does not cancel
preceding listings, shared pin producers, or every P3 operation; see
[filesystem snapshots](filesystem-snapshots.md#native-p3-directory-build-slice).

Shutdown preserves that scheduling topology. The Host first closes new
filesystem admission, asks deployment generations to stop, and asynchronously
joins executor workers while the main runtime remains available. It then drops
cache and staging owners, waits for process and deployment completion proofs,
drains retained work and cleanup roots through `HostFsOwner`, joins the cleanup
thread, and only then broadcasts shutdown to the remaining service threads.
Startup failures after Host creation use the same epilogue. Cleanup failures are
reported alongside the original startup error rather than replacing it.

The completion proof covers Wetware-owned admission, local producers, result
disposal, and registered local cleanup. Cancelling or dropping a Kubo HTTP
request does not prove that daemon-internal work stopped, so these limits do
not claim to bound Kubo's internal queues or operations.

The default is `current_thread`. We pick it because:

- A single-threaded runtime composes naturally with `LocalSet`, which is
  required for cell drivers (`wasmtime::Store`) and Cap'n Proto RPC
  systems (the generated client types are `!Send`).
- A service span entered outside `Runtime::block_on` can label the whole
  current thread. This ambient label does not provide task-specific attribution:
  concurrent futures still interleave on a `current_thread` runtime or `LocalSet`.
  Attach task-specific spans to futures with `tracing::Instrument`; do not hold
  an entered guard across `.await`. Separately spawned tasks require explicit
  propagation; the ownership rules below describe the boundaries in this runtime.
- Each subsystem owns its runtime shutdown. Executor workers explicitly
  cancel and drain managed local tasks before destroying the `LocalSet` and
  runtime. Dropping a runtime alone does not certify asynchronous cleanup.

## Async tracing ownership

`tracing::Instrument` enters a future's span during each poll and during drop.
It exits before the executor polls another task. An entered-span guard is valid
for the synchronous VAT/stream accept branches, but must not cross an await.

| Boundary | Context |
| --- | --- |
| `services::worker_loop` → Cell factory | Existing `cell` span with numeric `owner_id`, parented to the worker's `executor` span. `cell_name` and legacy `name` remain available to filters but are redacted in output. |
| `Generation::run` → PID0 process and filesystem finalizer | Explicitly retain the generation's current Cell span across the separate tasks. |
| `ExecutorImpl::spawn` → ordinary child | New root `cell` span with numeric `cell_id`. Construction, process lifecycle, and filesystem finalization share this span. A `follows_from` link records the spawning context. |
| `ManagedRpc::spawn` → private RPC worker | New root `rpc` span with numeric `rpc_id`. Dispatch, driver failure, and managed teardown share this span. A `follows_from` link records the opening context. |
| VAT/stream accept → handler/supervisor | Handler span has the synchronous accept span as parent. Both contain the same numeric `connection_id`. If the handler span is disabled, explicitly retain the enabled current accept span. |
| Stream supervisor → input/output pump | Each inline pump future has its own task-aware span, nested under the supervisor. |

RPC connections can outlive their opening call and carry capabilities used by
many callers. The RPC worker therefore does not retain the opening caller as
its parent. An event in the RPC driver identifies the connection, not the remote
caller or the owner of every capability used over that connection. Ordinary
children likewise have their own identity, rather than the spawning Cell's
identity. These changes do not change RPC authority, task ownership, cancellation,
or scheduling.

`owner_id`, `cell_id`, `rpc_id`, and `connection_id` distinguish operations within
one Host process. Each field uses a separate identity namespace. They are log/span
fields, not Prometheus labels or cross-process trace IDs. The current
JSON formatter includes only the current span; it does not serialize ancestry
or `follows_from` links. Nested events can therefore identify a pump or RPC
without repeating the enclosing Cell's fields. Filtering may remove context;
an absent span must not be interpreted as a new or shared identity.
The [operational logging contract](logging.md) specifies redaction and event codes
without changing these task/span ownership boundaries.

Other independently spawned work does not implicitly inherit caller context.
Service/control loops, executor monitors, cache/filesystem producers, and
libp2p tasks retain their existing service or operation context, where present.
The Swarm service's ambient span labels its owning thread, not Tokio worker
threads. Child stderr identity and final drain remain under #155; this change
does not attach the child lifecycle span to the stderr reader.

## SwarmService is the exception

The libp2p swarm runs on a `multi_thread` runtime. The reason is
**Ed25519 verification in the QUIC handshake** (#456).

Every incoming or outgoing QUIC connection goes through
`libp2p_tls::Libp2pCertificateVerifier`, which calls
`libp2p_identity::PublicKey::verify` → `ed25519_dalek::verify`. That
verifier is `fn`, not `async fn` — `rustls 0.23`'s
`ServerCertVerifier` trait has no async escape hatch, no `Deferred`
return variant, and `libp2p-tls 0.6` does not expose a seam to swap
the verifier from a downstream crate. The verification call is
strictly synchronous, on whatever thread is driving the connection.

On a single-threaded swarm runtime, every concurrent handshake
serializes through that one thread. A `sample` taken during a kad
bootstrap storm showed ~50% of busy time in
`curve25519_dalek::scalar_mul::spec_avx2::mul` on the swarm thread,
even with idle cores on the machine.

### Why `multi_thread` works without any other changes

`libp2p-swarm` already spawns each connection upgrade as a separate
`tokio::spawn` task — see
[`libp2p-swarm/src/connection/pool.rs:482`](https://docs.rs/libp2p-swarm/0.46.0/src/libp2p_swarm/connection/pool.rs.html).
The TLS handshake (including Ed25519 verification) runs inside that
spawned `Connecting` future. On `current_thread`, all those spawned
tasks queue onto the one thread. On `multi_thread`, tokio's
scheduler distributes them across worker threads automatically.

No fork of libp2p-tls, no custom verifier, no `spawn_blocking`
wrapping — the parallelism path is already there; we just stop
funneling it through one thread.

### What parallelizes, and what doesn't

**Parallel on workers:**
- Per-connection upgrade tasks (TLS handshake, Noise handshake)
- Kad query subtasks spawned by `libp2p-kad`
- Connection-keepalive timer tasks

**Still serial on the `swarm` OS thread:**
- The Swarm's own `select_next_some().await` event loop — this is the
  outer future passed to `Runtime::block_on`, and `block_on` drives
  it on the calling thread, not on a worker. The Swarm is structured
  as a single task by libp2p design; there is no way to split its
  poll across threads without forking libp2p-swarm.

For TLS-verification bottlenecks this is exactly the split we want:
the heavy work moves to workers, the event loop stays free to advance.

## Why not `spawn_blocking`

A natural first instinct is "wrap the synchronous verifier call in
`tokio::task::spawn_blocking`". This does not work:

- The rustls trait method returns `Result<HandshakeSignatureValid,
  Error>` synchronously. To get a value out of `spawn_blocking`'s
  `JoinHandle` from a sync function, the caller must `block_on` or
  `block_in_place`. `block_on` re-blocks the swarm thread (defeating
  the purpose); `block_in_place` requires a multi-thread runtime
  anyway — at which point you already have what `multi_thread` gives
  you for free, with extra steps.
- libp2p-tls hard-codes its verifier in `make_client_config` /
  `make_server_config`. libp2p-quic stores the resulting rustls
  config in private fields. `libp2p::SwarmBuilder::with_quic_config`
  surfaces only non-TLS knobs. There is no seam to install a custom
  verifier without forking at least two crates.

`multi_thread` for `SwarmService` is the answer at the right layer.

## Worker count

`Builder::new_multi_thread()` defaults to
`std::thread::available_parallelism()`. We don't override it. The
total thread budget is:

- 1 `swarm` OS thread (the supervisor's child)
- N `ww-swarm-worker-*` workers (tokio default)
- M `executor-*` (also `available_parallelism()`)

The main `ww` runtime also uses Tokio's default worker count. Most runtime
workers remain parked outside concurrent lifecycle or network work. If a
future profile shows worker
contention with the executor pool, we tune `with_quic_config` or set
an explicit `worker_threads(2)` on the swarm runtime.

## Tracing legibility

Workers are named `ww-swarm-worker` via
`Builder::thread_name(...)`. Default tokio names workers
`tokio-runtime-worker`, which is indistinguishable from any other
runtime in the process (notably the test harness). Named workers
let `sample`, `ps`, and tracing's per-thread filters pin verification
load to its actual source.
