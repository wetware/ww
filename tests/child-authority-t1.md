# T1 child-authority harness

The real-WASM probe lives at `tests/fixtures/authority-probe`. It emits one
small JSON line per focused probe. Ordinary tests run current characterization,
the closed confinement regressions, and the Cap'n Proto fork gate.

The former T4 and T5 expected-red tests are normal green regressions. The probe
receives one `Membrane` and calls `graft()`. Fixed platform authority occupies
typed fields. Application-defined named capabilities occupy `extras`.

## Layering and blocked cases

| Case | T1 state |
|---|---|
| Minimal-Membrane required `peerId`, guest enumeration, and withheld typed-authority calls | Passing T3 regression |
| Missing or empty `peerId` | Rejected by `MembraneServer` and status production decoding |
| Repeated `Membrane.graft()` extra-name set | Passing characterization |
| Same server under two extra names, two grafts | Passing hard gate; exact fork revision asserted |
| Empty/duplicate `extras` names | `NamedCapabilities` and wire-decoder unit validation; not production spawn admission |
| Path-like opaque extra label (`bad/name`) | Passing valid-name regression |
| Arbitrary unexported strings | Passing characterization; strings are not authority |
| Restricted Executor descendant amplification | Passing T3 regression |
| No-epoch/no-stream usable fixed platform authority | Passing T3 regression |
| Args/env/stdio and clock/randomness | Passing characterization |
| Byte-loaded empty root, retained image root, private writable scratch | Passing T6 focused/unit and real-WASM descendant regressions |
| Explicit known-CID read; no fallback, enumeration, or mutation | Passing T6 deterministic real-WASM regression |
| CAS pin/fetch/cache/eviction effects and cancellation cleanup | Passing deterministic cache and real-WASM characterization |
| Exact `Membrane` forwarding and child-lifetime ownership | Passing T3 regression |
| Typed stream/vat/HTTP capability calls | Recording endpoints verify real-WASM calls and returned capabilities |
| One typed bootstrap surface; no legacy child-bootstrap interface | Passing T5 regression |

T6 deliberately leaves any richer association between arbitrary
`Runtime.load(wasm bytes)` input and an FHS image undefined. Byte-loaded
Executors receive the private empty root; image selection remains a trusted
image-backed construction path.

## Current bootstrap topology

Trusted PID0 receives a process-local root `Membrane` and grafts once for its
generation. The Rust PID0 loads `$WW_ROOT/bin/status.wasm`, constructs a narrow
status `Membrane` containing `peerId` and `stat`, registers `/status`, and
commits readiness.

Ordinary children receive the exact `Membrane` supplied to
`Executor.spawn()`. Fixed platform authority uses typed graft fields, and
application-defined capabilities use `extras`. HTTP and stream listeners retain
one registration-time `Membrane` and forward the same capability to each
spawned child.
