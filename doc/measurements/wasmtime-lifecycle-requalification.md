# Wasmtime lifecycle requalification for #777

## Prospective release protocol

Written on 2026-10-31 before collecting new performance measurements.
The source baseline is `49099f53d4b61f6de6c2e9e00dfa01cc8fd025d5`.
Candidate `470a6d90` recovers `298ca3bf` by a conflict-free cherry-pick.
All eight recovered file blobs match the original candidate. The branch name
remains `lthibault/pasted-text-file-v16`.

The [original contract and failed protocol](wasmtime-lifecycle.md) remain
unchanged. The historical verdict remains `STOP_MATERIAL_REGRESSION`.
The original report and raw observations were recovered byte-for-byte in
`.context/lifecycle-recovery/historical/`, with SHA-256 hashes and original
paths in `recovery-manifest.json`. These new criteria do not reclassify those
measurements. Issue #777 authorizes a separate product-oriented release decision.

### Mandatory gates

- Preserve compilation, preparation, executable-instantiation, and command
  boundaries, including success/error/timeout and unobserved dropped futures.
- Preserve #776 span attribution, cache semantics, scheduling, fuel enforcement,
  authority, Store ownership, and Store layout.
- Use fixed labels and histogram buckets, bounded storage, no sensitive labels,
  no per-observation event allocation, and no exporter waits or locks.
- Pass applicable Cell, native P3, live HTTP, workspace, formatting, Clippy,
  lockfile, packaging and required GitHub CI checks. Missing prerequisites are
  reported explicitly; missing checks do not count as passes.
- Obtain independent source, correctness, and qualification review. Resolve any
  disagreement about correctness or material performance risk before landing.

### Representative performance smoke

Build baseline and candidate with the same committed release profile, stable
toolchain, lockfile, compiled Rust P3 fixtures, and qualification harness.
Record source, fixture, protocol, harness, and executable SHA-256 identities.
Use the real compiled `authority-probe no-graft` component, verify its JSON
result, and include filesystem cleanup in complete Cell launch timing.
Read fixture bytes before timing; exclude disk reads and process startup.

Measure uncached canonical compilation, complete launch from component bytes,
and complete launch from a reused compiled component. Report preparation and
executable run phases separately where observable. Report actual candidate
executable-instantiation latency from quiescent lifecycle-duration snapshot
deltas, with exactly one completed instantiation between each pair of snapshots.
Take those snapshots outside comparative timers. If baseline pure-instantiation
latency is unavailable through an equivalent public boundary, state that limit
and use `Proc::run` for the comparative run-phase gate. Do not label an entire
command run as pure instantiation.

Measure process CPU before and after each individual operation, using the same
operation boundary as its wall timer. Report per-operation CPU median and p95;
whole-process CPU divided by operation count cannot substitute for those values.
Process CPU includes any background work within the benchmark process. Record
peak resident memory separately as a process high-water measurement, with case
order identical across processes. RSS does not isolate collector footprint;
the two lifecycle collectors contain 224 bytes of atomic counter payload.
Record whether instrumentation can execute during steady-state guest work
after instantiation.

Each case uses three warmups and 31 retained individual operations. Report
median and nearest-rank p95 of individual operations, not batch averages.
First run the identical baseline executable in two fresh processes to expose
process variance, using the same cases, fixtures, sample counts, and budgets.
Then run three baseline/candidate process pairs in AB, BA,
AB order. Keep every raw sample, warmup, failure, and environment log.

All builds and comparisons acquire an exclusive `fcntl.flock` on
`/tmp/ww-fuel-coordination/build-benchmark.lock`. Timed comparisons run built
executables without concurrent compiler work. Monitor AC power at least once
per second, including before and after each process. Power loss, monitoring
failure, or a 180-second per-process deadline invalidates that run and stops
the campaign. Do not silently replace invalid observations.

### Material-regression criteria

These budgets concern one local Cell operation, not fleet capacity or Linux
production performance. A regression must exceed both the relative and absolute
budget. The absolute budgets limit operational delay; relative budgets detect
large changes when baseline operations already exceed those delays.

| Measure | Median added budget | p95 added budget |
| --- | --- | --- |
| Complete Cell launch, compiled or reused | 10% and 1 ms | 20% and 2 ms |
| Real component compilation | 10% and 1 ms | 20% and 2 ms |
| Isolated instantiation or executable run phase | 10% and 0.25 ms | 20% and 0.5 ms |
| CPU time per complete launch or compilation | 10% and 1 ms | 20% and 2 ms |

A repeated breach in two comparison pairs, or a breach in pooled retained
samples after all three pairs, blocks release. Stop after a repeated breach;
do not extend the campaign or adjust thresholds. If identical-binary controls
breach these same budgets, report an inconclusive environment and stop before
candidate comparisons. Adverse individual-pair values remain in the report.

Peak resident memory blocks release if the candidate exceeds baseline by both
10% and 16 MiB in two pairs. Independently, review collector storage and require
fixed process-wide storage below 1 KiB, with no per-Store or per-observation
heap allocation. No linear growth with completed operation count is permitted.
The unchanged historical Store-only microbenchmark is evidence, not a new veto.

Investigate a breach only enough to identify a concrete blocker or measurement
limitation. Do not start compiler-layout attribution or optimize fuel/Host paths.

### Release and remaining scope

Publish and merge only after all mandatory gates pass. Keep #753 open for P3
operation ownership, cancellation diagnostics, bytes/backpressure, resource
visibility, and deferred runtime profiling. Owner-local snapshots belong to
#779 and are outside this implementation.

## Results

Status: **PASS_LOCAL_SMOKE** and local correctness qualification passed.
Required GitHub CI and final PR review remain pending. This is a prospective
release result; the historical **STOP_MATERIAL_REGRESSION** remains unchanged.
The [original report](wasmtime-lifecycle-original-report.md) preserves that
NO-GO. The [new results and raw numeric samples](wasmtime-lifecycle-requalification-results.json)
record this separate protocol, including all warmups and adverse observations.

### Correctness and mechanical repairs

The first workspace attempt stopped with seven passing and 12 failing PID0
tests: eleven lacked the pinned forge-std submodule; one used the old metrics
schema. The repaired PID0 suite passed all 21 tests, including two parser
regressions. The full workspace then passed 1,358 top-level tests with
30 ignored and no failures. Four nested HTTP subprocess summaries account
for 1,362 aggregate logged passes; they are not additional top-level tests.

Initialized forge-std at its committed revision
`1801b0541f4fda118a10798fd3486bb7051c5dd6`. The parent gitlink is unchanged.
Foundry maps `forge-std/Script.sol` to `lib/forge-std/src/Script.sol`.
The PID0 helper now checks exactly 50 fixed samples, preserves integer values
as `u64`, validates fractional duration sums separately, rejects duplicate,
missing, malformed and unexpected series, and checks histogram structure.
The finite response bound is 8 KiB. The content-type assertion and original
PID0 lifecycle checks remain unchanged.

Strict Clippy initially rejected constant release-mode assertions in the two
qualification harnesses. Explicit conditional panics preserve the release-only
execution contract without lint suppression. Strict Clippy and targeted debug
guard probes then passed. Failed logs remain separate from corrected receipts.

Additional completed local checks:

- Six explicit native P3 tests; nine strict HTTP tests, including the real
  compiled Rust P3 guest; 14 metrics unit tests.
- Formatting, committed lockfiles, architecture-map and release-script tests.
- Canonical package, payload, installation and installed-boot checks.
- Solidity formatting/build and nine tests; standalone guest-crate tests.
- Four real guest-session cancellation tests; fresh first-party P3 builds;
  rebuilt native P3 and package checks; Chess authority proof.
- Deterministic replay of the five existing checked-in fuzz corpora.

Ten rebuilt fixture hashes differed from the recovered artifacts. Their
affected PID0, status, authority, discovery, finite-output, stream, image,
daemon, HTTP and routing checks passed against the rebuilt bytes. The host
was rebuilt with those payloads and installed boot passed again. Unaffected
workspace tests were not repeated solely for fixture changes.

### Prospective performance outcome

Identical-binary controls passed. Three baseline/candidate pairs ran in AB,
BA, AB order. Each case retained three warmups and 31 individual observations
per process. All nine timed processes, including the candidate-only observation
probe, exited zero under the shared lock. AC monitoring succeeded throughout;
the largest probe gap was 0.586 seconds. The longest process took 11.275 seconds.
No run was discarded or replaced.

Pooled wall times, in milliseconds:

| Case | Baseline median | Candidate median | Median change | Baseline p95 | Candidate p95 | p95 change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Uncached compilation | 139.204 | 140.243 | +0.746% | 164.044 | 165.844 | +1.097% |
| Complete launch from bytes | 148.182 | 149.790 | +1.085% | 170.384 | 170.831 | +0.262% |
| Complete launch from compiled component | 0.485625 | 0.487250 | +0.335% | 0.574166 | 0.592125 | +3.128% |
| `Proc::run` | 0.221542 | 0.224500 | +1.335% | 0.255417 | 0.270833 | +6.036% |

Pair 1 breached the launch-from-bytes median budget: +14.398 ms and +10.0497%.
Pairs 2 and 3 had no breaches. No repeated or pooled gate failed. CPU gates
passed. Across comparison processes, peak RSS rose from 348.109 MiB to
368.500 MiB: +20.391 MiB and +5.858%, below the combined RSS criterion.
The JSON report retains individual-pair values, CPU results and preparation
intervals; the table does not replace those observations.

Candidate-only executable-instantiation duration was 31.708 microseconds at
the median and 40.542 microseconds at p95. These collector deltas are
observations, not a baseline comparison. Comparative `Proc::run` includes
command execution and is not labeled pure instantiation.

### Review, identities and limits

Independent review approved recovery fidelity, lifecycle semantics, PID0
schema repair and the mechanical conditional-panic correction. The final
review independently recomputed medians, nearest-rank p95, threshold outcomes,
and monitoring validity from raw samples and confirmed `PASS_LOCAL_SMOKE`.
Required GitHub CI and final PR review still gate merge.

All eight recovered candidate blobs match `298ca3bf`. The baseline includes
merged #776; its span-ownership corrections remain preserved. Baseline and
candidate use identical profile, lockfile, toolchain, harness and fixture
bytes. The JSON report records executable and input hashes. The frozen
prospective protocol section is unchanged; only results were updated after
measurement. Local snapshots retain the exact document used for measurement.

The collectors add 224 bytes of process-wide atomic payload. Recording uses
bounded bucket selection and three relaxed atomic additions, with no event
allocation, exporter wait or lock. No recording call was added to steady-state
guest execution after instantiation. These sequential Mac measurements do not
establish Linux or production-concurrency behavior, zero overhead, or the
cause of the historical Store-control regression.

Raw logs, monitoring records, source reviews, original failed receipts and
corrected receipts remain in `.context/lifecycle-recovery/`, under
`historical/`, `new/`, `pid0-repair/`, `lint-resume/`, `mechanical-resume/`,
`rebuilt-validation/` and `prospective-run/`. Earlier report versions are
preserved separately. No historical failure was reclassified as passing.

Issue #753 remains open for P3 operation ownership, cancellation diagnostics,
bytes/backpressure, resource visibility and deferred runtime profiling.
Owner-local snapshots belong to #779 and were not changed by this work.
