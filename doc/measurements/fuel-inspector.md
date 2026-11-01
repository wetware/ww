# Fuel inspector local qualification

Date: 2026-11-01. Base: `24a2ff064e88d7c9f953ff10818b23a78ef7c923`.
Integrated base after #789: `3937229ef5ba973281e13df8a16a50cf42662aff`.
Environment: macOS arm64, pinned Wasmtime fork `fe367749`, stable host Rust,
native P3 producers using `nightly-2026-08-30` and WASI SDK 34.0.

The [design](../designs/fuel-inspector.md) defines snapshot ownership, freshness,
unavailable values, request limits and the local operator consumer.

## Correctness evidence

The inspector tests use configured budgets, `Store::get_fuel`, direct ledger
arithmetic and actual guest completion as witnesses. They do not infer fuel
conservation from diagnostic agreement alone. Coverage includes:

- Scheduled EWMA extremes, desired versus applied quantum and physical reserve.
- One-shot partial grants, zero-budget rejection and reserve overshoot.
- Coalesced epochs and rejection of superseded provisional suspension.
- Failed Wasmtime installation: a queued request receives no snapshot and
  returns `OwnerStopped` after Store destruction.
- Nested Host frames and skipped unflushed epoch returns.
- A compiled routing-key P3 guest, live suspension, next-epoch rearming and
  physical-fuel reads after resumption.
- Default versus inspected `Proc` outcomes through actual start-function
  suspension, completion and cancellation.
- An independent requester runtime timing out while the Store remains idle.
- Read-only capture with unsettled physical consumption and a Host epoch ahead
  of the Store's opened allowance.
- Generation isolation, cancelled/expired requests, unread response ownership,
  and 64 simultaneous readers sharing one admission permit.

The standalone reporter captured the compiled native P3 fixture with total
authority 1,337 and epoch authority 10,000. The actual grant was 1,337 against
desired quantum 1,000,000. Physical fuel was 10,001,337, including the 10,000,000
reserve. The fixture subsequently returned the expected authority-exhaustion
error. The reporter's nonzero exit therefore reflects guest execution after a
successful snapshot, not an inspection failure.

Local logs are retained under `.context/fuel-inspector/`. They include the
inspector suite, workspace suites, strict Clippy, native P3 tests, reporter
output and the independent ownership review.

On the integrated base, `cargo test --workspace --locked --no-fail-fast`
passed 1,419 tests across 63 suites, with 30 ignored. This includes all 20
inspector tests and all 21 PID0 E2E tests against disposable offline Kubo and
local Anvil. The six native P3 tests passed separately with their required
fixture. Strict `cargo clippy --workspace --all-targets --locked -- -D warnings`,
`cargo fmt --all --check`, explicit formatting of the included inspector tests,
and `git diff --check` passed. Ignored platform/tooling tests outside the
explicit runs remain unvalidated.

## Performance smoke

The existing `tests/fuel_overhead.rs` harness ran without a `FuelObserver` in
timed work. It completed 31 samples after three warmups in each of eight
combinations: scheduled compute, I/O, host returns and one-shot suspension,
with one and four workers. The untimed preflight confirmed real suspension.

Command:

```sh
cargo test --locked --test fuel_overhead measure_fuel_operations -- \
  --ignored --nocapture --test-threads=1
```

These are **debug-build smoke observations**, not a baseline comparison or
release-build regression acceptance. Elapsed time is the slowest worker's
execution time for each sample, excluding preparation and cleanup.

| Workload | Workers | Median ms | Maximum ms |
|---|---:|---:|---:|
| Scheduled compute | 1 | 34.251 | 35.236 |
| Scheduled compute | 4 | 36.050 | 49.471 |
| Scheduled I/O | 1 | 37.676 | 41.999 |
| Scheduled I/O | 4 | 36.812 | 37.292 |
| Scheduled host returns | 1 | 215.147 | 233.976 |
| Scheduled host returns | 4 | 240.676 | 265.909 |
| One-shot suspension/resumption | 1 | 59.458 | 61.916 |
| One-shot suspension/resumption | 4 | 57.117 | 60.386 |

All 58 power observations in the final smoke reported AC power. The earlier
smoke and its 44 AC observations remain in separate logs; these tables report
only the final integrated implementation. The shared
`/tmp/ww-fuel-coordination/build-benchmark.lock` serialized heavy builds and this
smoke. No release throughput, tail-latency improvement or performance parity is
claimed. Store layout and async dispatch changed; their release impact remains
unmeasured. The selected inspector has additional owner-local writes and
channel operations. The const-false default excludes those boundary operations.

The historical Curitiba sampler rejection, Osaka TLS rejection, and rejected
settlement batching campaigns remain separate evidence. This smoke does not
reinterpret or override those results.

## Retained qualification failures and repairs

The first inspector compile exposed old test calls that omitted the new const
argument on `FuelCallHook`. Specifying `<false>` repaired those calls without
changing their enforcement path. The subsequent inspector suite passed.

The first workspace run lacked `WASI_SDK_PATH`; the authority fixture could not
build. The next run used the existing pinned SDK and all 37 authority-confinement
tests passed. That run then found missing canonical kernel/status payloads in
the new workspace. `make std routing-key-probe` rebuilt those producers locally;
no runtime change was made to bypass either prerequisite. Both failed workspace
logs remain available alongside the subsequent run.

The full integration run also required the example guests, an isolated Kubo
daemon and the pinned `forge-std` submodule. The local producers and repository
fixture helper supplied those prerequisites. PID0 failures from missing Kubo
and the missing Solidity import remain in the earlier logs; no enforcement
change was made to bypass them.

Removing the temporary baseline worktree exposed a cached authority build
script containing that worktree's absolute schema path. Cleaning only the local
`wetware-authority` build artifacts repaired the cache. The affected smoke did
not execute and is not performance evidence. Its build failure remains recorded.

## Async allocation audit

`cargo rustc -p cell --lib -- -Zprint-type-sizes` with `RUSTC_BOOTSTRAP=1`
measured coroutine layouts using the stable host compiler. A detached worktree
at the verified base supplied the comparison. This compiler diagnostic does
not change production build flags or dependencies.

The initial implementation enlarged the existing boxed hook future from 368
to 432 bytes, including when inspection was disabled. Const specialization
removed execution of the diagnostic branch but retained its coroutine storage.
That version was rejected. Boxing only the enabled epoch-wait future reduced
the hook to 376 bytes. Passing the suspension flag by value preserved semantics
but did not remove the remaining eight bytes. Those intermediate measurements
remain in `type-sizes-candidate.log`, `type-sizes-corrected.log` and
`type-sizes-owned-wait.log`.

Reordering the awaits also retained the eight-byte increase. The final design
uses a private static trait to select distinct application futures. The default
application function matches the base after installer-name normalization.
The synchronous selector constructs that future directly without an additional
coroutine, dynamic dispatch or allocation. The enabled wait no longer needs a
separate box.

| Coroutine | Base bytes | Final default bytes | Final enabled bytes |
|---|---:|---:|---:|
| Fuel hook | 368 | 368 | 416 |
| Settlement | 320 | 320 | 368 |
| Decision application | 240 | 240 | 288 |

`type-sizes-static-dispatch-final.log` records these results. Independent review
accepted the final dispatch and checked both application loops. Future changes
must preserve their authority/epoch ordering; the default-versus-inspected
execution test checks actual outcomes through suspension and cancellation.

The optional field changes `ComponentRunStates` from 832 to 848 bytes because
of alignment. The `Proc::run` wrapper changes its future from 1,480 to 1,600
bytes; the inner execution future remains 1,480 bytes. The snapshot occupies
312 bytes on this target. These lifecycle and snapshot sizes are separate from
per-boundary hook allocation size. They are not portable ABI guarantees.

## Review and remaining limits

Separate agents reviewed runtime ownership and authority application, and the
request transport's security/resource contract. Neither review found a blocking
defect. The runtime reviewer authored the transport, so an additional reviewer
independently checked transport admission and guest isolation. Review identified
the timeout-delivery caveat and prompted the failed-installation public-request
test. Both changes were incorporated.

The operator must retain a handle attached before execution. Idle Cells can time
out; successful termination and cancellation have no retained terminal fuel
snapshot. Host publication is not transactionally coherent with Store fields.
Returned snapshots remain historical after owner progress or termination.
Continuous event tracing, worker utilization, and individual yield history are
not provided. Remote authorization and endpoint design remain out of scope.

CI on other platforms, production deployment and release-build performance
comparisons are not established by this local qualification. Publication remains
subject to the user's External Communications Gate.
