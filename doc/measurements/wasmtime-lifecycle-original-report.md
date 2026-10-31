# Wasmtime lifecycle slice — stopped before landing

Local candidate commit: `298ca3bf` on `lthibault/pasted-text-v30`.
Baseline: `0ee7af3e163981c3c05f5b09d8860faac2e13b71`.
Status: `STOP_MATERIAL_REGRESSION`. No push, PR, merge, or deployment.

The complete contract, correctness evidence, frozen qualification protocol,
results, and remaining #753 scope are in
`doc/measurements/wasmtime-lifecycle.md`.

## Implemented

- Compilation success/error counters and duration histogram around canonical
  `Component::from_binary`, including persistent-cache interactions.
- Instantiation success/error/timeout counters and duration histogram around
  executable `CommandPre::instantiate_async` in `Proc::run`.
- Fixed labels and buckets, process-wide factory ownership, no event allocation
  or mutex. No changes to runtime/fuel semantics or PR #776 files.
- Pending dropped instantiation futures remain unobserved. Preparation and
  command execution are excluded. Existing cache counters retain their meaning.

## Validated

352 Cell unit tests, six compiled native P3 fixture tests, nine HTTP integration
tests (strict compiled Rust P3 path enabled), 14 metrics unit tests, workspace
Clippy with warnings denied, formatting, and committed lockfile checks passed.
Initial missing-fixture/build-prerequisite failures remain in the logs.
Two independent source reviews found no blocking correctness defect.

## Stop evidence

Two release process pairs were collected under the shared lock and continuous
AC monitoring. Store build/drop failed both frozen caps in both pairs:

| Statistic | Pair 1 added | Pair 2 added | Cap |
| --- | ---: | ---: | ---: |
| Median | 8.587 us | 2.659 us | 1 us |
| p95 of sample averages | 6.664 us | 10.935 us | 2 us |

Uncached compilation also exceeded its median cap in pair 1 (+7.77% versus 5%).
The largest paired HTTP p95 increase was 12.542 us; response growth was at most
2,792 bytes. These observations do not establish a completed performance pass.
The Store control executes neither new recording boundary. Cause is unknown;
these results must not be presented as isolated atomic recording cost.

The third pair was not run. No optimization, gate adjustment, or replacement
measurement followed the stop. Required remote CI, full workspace tests, Linux
performance, and landing remain unvalidated. #753 remains open for P3 operation
lifecycle, cancellation ownership, bytes/backpressure, and resource diagnostics.

## Evidence index

- `qualification-plan.md`, `frozen-protocol-hashes.json`: premeasurement contract.
- `source-before-build.json`, `benchmark-binaries.json`, `fixture-provenance.json`:
  source, executable, and fixture identities.
- `baseline-lifecycle-{1,2}.log`, `candidate-lifecycle-{1,2}.log`: raw lifecycle samples.
- `baseline-http-{1,2}.log`, `candidate-http-{1,2}.log`: raw HTTP samples.
- Corresponding `.monitor.jsonl` files: shared-lock and AC monitoring.
- `results-2.json`, `results-2.md`: complete two-pair analysis.
- `correctness-review.md`, `implementation-review.md`: independent source reviews.
- `performance-review.md`, `measurement-audit.md`: independent result and integrity reviews.
- `cell-fixtures.log`, `native-p3.log`, `http-compiled.log`, `metrics-unit.log`,
  `clippy.log`, `fmt.log`, `lockfiles.log`: completed validation.
- `cell-tests.log`, `release-build-candidate.log`: retained prerequisite failures.
