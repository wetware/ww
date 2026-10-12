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
Host logical, foreground-producer, and cleanup-root limits remain finite and
private implementation policy.

Foreground filesystem work submitted from an executor worker runs through the
Host driver captured from the main runtime. Ready-root cleanup uses one
dedicated cleanup thread and a separate cleanup lane, so saturating the main
runtime's blocking pool or all foreground producer permits cannot prevent
mandatory cleanup from progressing. Executor workers retain admitted
transactions through native-resource installation or disposal; a returned
value does not release its work receipt early.

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
- Tracing spans entered with `_span.entered()` only stay attached
  to the thread that entered them. A current-thread runtime keeps
  every task in that one span without manual `Instrument`-ing.
- Shutdown is straightforward: dropping the `Runtime` joins exactly
  what that subsystem started.

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
