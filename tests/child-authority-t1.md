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

T6 deliberately leaves any richer association between arbitrary
`Runtime.load(wasm bytes)` input and an FHS image undefined. Byte-loaded
Executors receive the private empty root; image selection remains a trusted
image-backed construction path.

## Current bootstrap topology

Trusted PID0 receives a process-local `Membrane` and grafts once for its
generation. The Rust PID0 loads `$WW_ROOT/bin/status.wasm`, grants `host`,
registers `/status`, and commits readiness.

Ordinary children receive exactly their requested named grants through
`InitialAuthorityRecord` and `InitialGrants.get()`. They do not receive a
universal host graft. `ExecutorImpl::spawn` validates the wire `caps` list
before process construction. HTTP and stream listeners decode registration
grants once and copy the immutable template into each child.
