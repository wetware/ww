# Wasmtime compilation and executable instantiation

This slice of #753 measures two lifecycle boundaries. The process-wide
`EngineFactory` owns the collectors. The existing `WasmtimeCacheMetrics` handle
retains their history and exposes snapshots to the HTTP admin service.

## Metrics contract

| Family | Type | Labels | Boundary |
| --- | --- | --- | --- |
| `ww_wasmtime_component_compile_total` | Counter | `outcome="success\|error"` | Returned canonical `Component::from_binary` calls |
| `ww_wasmtime_component_compile_duration_seconds` | Histogram | None | Wall time inside those calls |
| `ww_wasmtime_instantiations_total` | Counter | `outcome="success\|error\|timeout"` | Observed results of executable `CommandPre::instantiate_async` in `Proc::run` |
| `ww_wasmtime_instantiation_duration_seconds` | Histogram | None | Wall time from executable instantiation through its result or timeout |

Both histograms use inclusive finite bounds of 0.0001, 0.001, 0.01, 0.1, 0.5,
1, 2, 5, and 10 seconds, followed by `+Inf`. The exporter emits cumulative
`_bucket`, `_sum` in seconds, and `_count` series. Durations include failures.
No component identities, paths, error text, or guest payloads become labels.

Compilation `success` means `Component::from_binary` returned a component.
Compilation `error` means that call returned an error. Timing includes Wasmtime's
persistent-cache lookup, loading, and compilation work within the call. Cache
background cleanup is outside that boundary. Compilation is synchronous; the
collector does not invent timeout or cancellation outcomes for it.

Instantiation `success` means `CommandPre::instantiate_async` returned a command.
Instantiation `error` means it returned an error, including a guest start-function
trap or generated command-binding error. `timeout` means the existing five-second
Tokio timeout returned `Elapsed`. The timeout remains cooperative: the timeout
does not guarantee interruption of code that fails to yield.

Dropping a pending `Proc::run` future records neither an outcome nor a duration.
The collector does not own cancellation or establish backend completion. A panic
before the operation returns likewise produces no completed observation. Thus
these histograms cover observed terminal results, not every initiated operation.
An error returned by Wasmtime remains `error`, even if cancellation caused it;
this slice does not classify cancellation reasons.

## Excluded phases and preserved behavior

`Builder::build` prepares the Store and calls `instantiate_pre` for linker
validation. These operations are outside executable instantiation. Preparation
can fail after compilation succeeds. `Program::Precompiled` skips compilation.
`Program::ValidatedKernel` reuses its compiled component and validated command;
building from that value does not repeat either observation. Running either
program still executes and records instantiation once.

Initial fuel exhaustion occurs before instantiation and produces no instantiation
observation. Fuel settlement after instantiation and the subsequent
`wasi:cli/run#run` call are outside the instantiation boundary. A guest command
failure does not change a previously recorded instantiation success.

The existing `ww_wasmtime_component_compilations_total` still counts canonical
attempts at entry, including attempts that fail or load from the persistent
cache. Its value can exceed the sum of completed outcomes while work is active.
`ww_wasmtime_cache_hits_total` still means successful cache loads.
`ww_wasmtime_cache_stores_total` still means successful persisted stores, not
lookup misses. Lifecycle outcomes do not infer or replace cache outcomes.

## Recording and snapshot limits

Each observation uses two monotonic clock reads and three relaxed atomic
additions: one disjoint bucket, a nanosecond sum, and one outcome. Bucket selection
has a fixed bound. Recording allocates no event object, acquires no mutex, and
does not wait for the exporter. No recording was added to fuel hooks, epoch
callbacks, scheduler decisions, task polls, or WASI operations.

Snapshots load atomics independently. During concurrent recording, outcomes,
duration sums, and bucket counts need not describe one transaction. Histogram
count is derived from the same bucket loads as `+Inf`; cumulative buckets remain
ordered before counter overflow. After producers stop, totals include every
completed observation. Counters and cumulative nanosecond sums wrap modulo
2^64; a single duration is capped at `u64::MAX` nanoseconds.

The factory retains observations after Store and runtime teardown. Each process
starts fresh totals. Independent Host instances in the same process share these
Wasmtime totals, matching the existing Wasmtime cache collector lifetime.

## Validation

The local validation on 2026-10-31 passed:

- `cargo test -p cell --locked --lib -- --test-threads=2`: 352 passed,
  11 explicitly ignored fixture tests.
- `WW_NATIVE_P3_FIXTURE=... cargo test -p cell --locked p3::tests::p3_ -- --ignored --test-threads=1`:
  six native P3 fixture tests passed.
- `WW_METRICS_HTTP_REQUIRE_COMPILED_P3=1 cargo test --locked --test metrics_http -- --nocapture`:
  nine passed, including isolated cache-policy and lifecycle subprocesses.
- `cargo test --locked --lib -p ww metrics::`: 14 passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo fmt --all -- --check`, and `bash scripts/check_cargo_lockfiles.sh` passed.

The HTTP lifecycle probe runs on two Tokio workers. It verifies compilation,
preparation rejection, precompiled and validated-kernel reuse, successful command
execution, start traps, initial fuel rejection, command failure, concurrent runs,
pending cancellation, a real five-second timeout, and retained history. The
compiled Rust `authority-probe no-graft` run emitted
`{"mode":"no-graft","ok":true}` and added exactly one compilation success and
one instantiation success. The cache-policy probes cover invalid binary errors
with enabled, disabled, and fallback cache configurations.

The first Cell run lacked the authority and routing-key guest artifacts: 346
passed and six failed on missing fixtures. Restoring artifacts whose SHA-256
hashes match a manifest for baseline `0ee7af3e` resolved those prerequisites.
The first release build also stopped on missing embedded kernel/status WASM;
the subsequent build used identical artifacts in both comparison trees.
Both initial logs remain retained. No runtime fix was needed for these failures.

## Performance qualification protocol

The baseline is `0ee7af3e163981c3c05f5b09d8860faac2e13b71`. The protocol was
fixed before timing. Build both sides with the committed release profile and
the same toolchain, lockfile, embedded WASM, and `tests/lifecycle_overhead.rs`.
The lifecycle harness uses three warmups and 31 retained samples per case.
Each sample sums individual operation intervals: 16 uncached compilations,
64 warm-cache compilations, 2,048 invalid compilations, 64 successful or failed
Proc runs, or 32 Store build/drop operations. Report p95 of those sample averages,
not p95 of individual operation latency.

Run three alternating process pairs: baseline/candidate, candidate/baseline,
baseline/candidate. Retain every sample and every paired exception. The existing
`metrics_http::measure_http_scrapes` supplies 200 full-body loopback requests per
cache policy and before/after-compilation phase. Parsing is outside its timer.

| Case | Added median cap | Added p95 cap |
| --- | ---: | ---: |
| Uncached valid compilation | 5% | 10% |
| Invalid or warm-cache compilation | 1 microsecond | 2 microseconds |
| Successful or failed instantiation-dominated Proc run | 1 microsecond | 2 microseconds |
| Store build/drop | 1 microsecond | 2 microseconds |
| HTTP, each policy/phase | Report only | 20 microseconds |

A pooled cap violation or the same cap violation in two pairs stops
qualification. Do not replace adverse samples or extend the campaign. The
prospective lifecycle response-size cap is 4,096 added bytes for 29 new fixed
sample series and four HELP/TYPE pairs. Earlier executor/fuel reports' 1,024-byte
increments apply to those feature slices; their recorded gates remain unchanged.

All builds and measurements use an exclusive `fcntl.flock` on
`/tmp/ww-fuel-coordination/build-benchmark.lock`. Timed runs continuously monitor
AC power and terminate on power loss, failed monitoring, or their 180-second
deadline. Run already-built executables, without concurrent Cargo work.

The Proc measurement includes initial fuel setup, executable instantiation,
the no-op command or start-trap propagation, and Store destruction. It excludes
compilation, preparation, and the later filesystem cleanup wait. The Store control
measures preparation through destruction without executing a guest. These
measurements do not establish steady-state execution throughput or Linux
production performance.

## Qualification result: stopped, not approved for landing

The campaign stopped after two process pairs. Store build/drop exceeded both
its median and p95 caps in both pairs. The third pair was not run. No acceptance
limit changed after collection, and no follow-up optimization or measurement
campaign was started.

Values below are microseconds per operation, averaged within each sample.

| Store build/drop | Baseline | Candidate | Added | Cap |
| --- | ---: | ---: | ---: | ---: |
| Pair 1 median | 136.578 | 145.165 | 8.587 | 1.000 |
| Pair 2 median | 137.195 | 139.854 | 2.659 | 1.000 |
| Pair 1 p95 | 186.638 | 193.302 | 6.664 | 2.000 |
| Pair 2 p95 | 178.198 | 189.133 | 10.935 | 2.000 |

The complete observed two-pair comparison is retained below. These partial
pooled values are descriptive; the stop decision uses repeated paired failures.
Negative differences are measurement variation, not demonstrated improvements.

| Case | Baseline median, us | Candidate median, us | Added median, us | Added p95, us |
| --- | ---: | ---: | ---: | ---: |
| Uncached compilation | 372.151 | 391.134 | 18.983 | 35.508 |
| Invalid compilation, cache disabled | 0.609 | 0.672 | 0.063 | 0.071 |
| Invalid compilation, cache enabled | 15.465 | 15.529 | 0.063 | -2.245 |
| Warm-cache compilation | 65.208 | 65.568 | 0.360 | -1.732 |
| Successful Proc run | 14.572 | 14.480 | -0.092 | 0.756 |
| Start-trap Proc run | 23.921 | 23.767 | -0.154 | -0.523 |
| Store build/drop | 136.984 | 143.005 | 6.021 | 5.324 |

Uncached compilation also exceeded its median cap in pair 1: 371.669 to
400.555 microseconds, or +7.77%, against 5%. Pair 2 was +2.89%. That observation
remains adverse; no compilation acceptance claim follows from the incomplete
campaign. The largest individual paired HTTP p95 increase was 12.542
microseconds, below 20. Responses added at most 2,792 bytes, below 4,096.

The Store control does not execute either instrumented boundary. These timings
establish a qualification failure, not a causal explanation of the difference.
The implementation and raw evidence remain local. No PR, merge, or deployment
was performed. Full workspace tests and required remote CI were not completed
after the performance stop. Earlier correctness and Clippy results remain valid;
they do not override this gate.

Workspace evidence lives in `.context/lifecycle/`: `qualification-plan.md`,
`source-before-build.json`, `benchmark-binaries.json`,
`frozen-protocol-hashes.json`, the eight raw timing logs and their lock/AC
monitor logs, `results-2.json`, `results-2.md`, and the independent reviews.

## Remaining scope

Keep #753 open for WASI P3 operation lifecycle, cancellation ownership,
bytes/backpressure, resource diagnostics, and the other runtime measurements
listed in the issue. This change does not add OTLP export.
