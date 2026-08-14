# T1 child-authority harness

The real-WASM probe lives at `tests/fixtures/authority-probe`. It emits one
small JSON line per focused probe. Ordinary tests run current characterization,
the closed confinement regressions, and the Cap'n Proto fork gate.

The former T4 and T5 expected-red tests are normal green regressions. The probe
reads exact named grants through `InitialGrants.get()`; no ordinary-child server
implements the graft-capable `Membrane`.

## Layering and blocked cases

| Case | T1 state |
|---|---|
| Empty-grant guest enumeration and concrete core-cap calls | Passing T3 regression |
| Repeated `InitialGrants.get()` name set | Passing characterization |
| Same server under two names, two deliveries | Passing hard gate; exact fork revision asserted |
| Empty/duplicate `caps` wire names | Passing T3 regression |
| Path-like opaque wire label (`bad/name`) | Passing valid-name regression |
| Arbitrary unexported strings | Passing characterization; strings are not authority |
| Restricted Executor descendant amplification | Passing T3 regression |
| No-epoch/no-stream usable raw `Host` | Passing T3 regression |
| Args/env/stdio and clock/randomness | Passing characterization |
| Byte-loaded empty root, retained image root, private writable scratch | Passing T6 focused/unit and real-WASM descendant regressions |
| Explicit known-CID read; no fallback, enumeration, or mutation | Passing T6 deterministic real-WASM regression |
| CAS pin/fetch/cache/eviction effects and cancellation cleanup | Passing deterministic cache and real-WASM characterization |
| `InitialAuthorityRecord`, exact record delivery, shared encoder | Passing T3 regression |
| Grants-only bootstrap surface/no `graft()` | Passing T5 regression |
| Glia `:grants`, source duplicate diagnostics, lexical-capture removal | Passing T4 regression |

The wire duplicate test deliberately says nothing about Glia map literals:
Glia's ordinary map evaluation normalizes through `im::HashMap`, so source
duplicate detection belongs at parse/analysis time in T4.

T6 deliberately leaves any richer association between arbitrary
`Runtime.load(wasm bytes)` input and an FHS image undefined. Byte-loaded
Executors receive the private empty root; image selection remains a trusted
image-backed construction path.

## Post-T3 migration inventory

Host/pid0 surfaces that retain grafting and must **not** be mechanically
migrated as ordinary children:

- `std/kernel/src/lib.rs`: `KernelBootstrap` and pid0's initial graft. The
  corrected Autoplan addendum explicitly keeps this pid0 export surface.
- `src/executor.rs` and `src/cli/main.rs`: pid0 and daemon bootstrap consumers.

Ordinary children receive exactly their requested named grants through
`InitialAuthorityRecord` and `InitialGrants.get()`; they do not receive a
universal host graft. Migrated consumers include:

- `std/status/src/lib.rs`: status cell obtains `host`.
- `examples/oracle/src/lib.rs`: HTTP cell obtains `http-client`; serve and
  consume modes obtain `host`/routing/network capabilities.
- `examples/discovery/src/lib.rs`: service mode obtains `host` and `routing`.
- `examples/chess/src/lib.rs`: service mode obtains host/routing/network
  authority.
- Their directly stale README snippets use the grants-only interface.

Spawner/listener state after T3:

- `src/launcher.rs`: `ExecutorImpl::spawn` validates the wire `caps` list into
  `InitialAuthorityRecord` before process construction and builds only the
  bounded child bootstrap.
- `crates/rpc/src/graft.rs`: `HostGraftBuilder` assembles ordinary
  epoch-guarded host/runtime/routing/authority/identity/IPFS/HTTP provisioning.
  Trusted pid0 receives a process-local root wrapper that also binds its graft
  generation and hands off a distinct network-exportable `Membrane`; only the
  private PID0 host import can commit readiness.
- `std/status/etc/init.d/05-status.glia` and
  `tests/status_cell_e2e.rs` / `tests/status_cell_http_listener_e2e.rs`
  explicitly pass the `host` grant needed by status children.
- `examples/oracle/glia/register.glia`, `serve.glia`, and `consume.glia`.
- `examples/discovery/glia/serve.glia`.
- `examples/chess/glia/serve.glia`.
- `examples/counter/glia/register.glia`,
  `examples/snap-hello-rs/glia/register.glia`, and
  `examples/echo/glia/register.glia` currently omit grants; these should be
  reviewed and made explicitly zero-grant if they need only substrate.
- Direct `Executor.spawn` flows in the chess, discovery, and oracle registration
  scripts should explicitly pass zero grants unless their default cell mode
  grows an authority dependency.
- `crates/glia/src/eval.rs` now routes both `cell` paths through one explicit
  grant validator; neither path captures lexical capabilities.
- `std/kernel/src/lib.rs` has both `cell` spawn encoders and the direct
  `runtime :run` spawn path, plus HTTP- and stream-listener grant encoders.
- `crates/rpc/src/http_listener.rs` and
  `crates/rpc/src/stream_listener.rs` decode registration grants once and replay
  immutable templates per child.
- `src/dispatcher/mod.rs` is the WAGI dispatcher spawn site and receives the
  bounded executor rather than a universal child graft.
- Direct zero-cap spawn sites in `tests/discovery_integration.rs`,
  `tests/stdin_shutdown_integration.rs`, `tests/runtime_spike_test.rs`, and
  `examples/echo_handler_e2e.rs` do not need node authority for their stated
  behavior and now exercise explicit zero-grant construction.
- HTTP and stream listener unit tests assert that repeated children receive the
  same fixed registration template.

The remaining production migrations in this inventory belong to T4/T5 and are
deliberately outside T3.
