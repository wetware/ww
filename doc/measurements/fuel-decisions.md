# Applied fuel-decision instrumentation measurements

Baseline: `400ec876` (master including ExecutorPool instrumentation). Use the
same committed release profile, lockfile, Rust toolchain and machine for both
trees. The benchmark harness deliberately uses only baseline APIs.

## Acceptance budget, fixed before measurement

- Scheduled compute and scheduled I/O: at most 3% added median elapsed time
  and 5% added p95 elapsed time for identical guest work.
- Repeated marked host-return boundaries: at most 50 ns added median time per
  boundary with one worker, and 150 ns with four concurrent workers. For four
  workers, use per-worker time, not aggregate throughput divided by four.
- HTTP: at most 20 microseconds added p95 scrape time and 1,024 added bytes.
- One-shot suspension/resumption: report latency and dispersion separately.
  Wall time contains deliberate 10 ms epoch waits; it cannot resolve the
  incremental atomic cost. The deterministic preflight must demonstrate at
  least two suspensions and successful completion after resumption.

These are investigation/acceptance caps, not test assertions. A result outside
a cap requires investigation and a stated conclusion; an aggregate median
must not hide consistent paired regressions. Negative deltas are measurement
variation, not evidence that instrumentation speeds execution up.

## Method

`tests/fuel_overhead.rs` runs real native P3 commands through `Builder` and
`Proc::run`, with the production engine configuration and fuel hooks. One or
four persistent OS threads each run a current-thread Tokio runtime. All workers
share one `RuntimeEngine`, hence the same instrumented collector. This tests
collector contention without adding ExecutorPool admission or dispatch cost.

Each process precompiles WAT fixtures and runs an untimed one-shot preflight.
Each case has three warmups and 31 measured samples. Each sample prepares its
process and filesystem before a worker barrier. The measured interval includes
initial fuel installation, Wasmtime instantiation, execution and process drop;
it excludes compilation, thread/runtime startup, filesystem setup and the wait
for filesystem cleanup. Each guest performs a steady batch inside one command,
so repeated construction cannot dominate per-boundary measurements. Cleanup
finishes before the next sample. No `FuelObserver` is present during timing.

| Case | Work per worker per sample | Interpretation |
| --- | --- | --- |
| `scheduled_compute` | 4,000,000 integer loop iterations | Compute throughput with real async fuel yields and scheduled grants |
| `scheduled_io` | 32 P3 `monotonic-clock.wait-for(1 ns)` calls | Actual asynchronous timer I/O; includes runtime timer granularity and wake scheduling |
| `scheduled_host_boundaries` | 100,000 P3 `monotonic-clock.now` calls | One marked host-return boundary per clock call; stresses successful grant installation |
| `oneshot_suspend_resume` | 2,000 loop iterations, 2,000 fuel per epoch | Actual bounded epoch authority, suspension and resumed grants; total authority exceeds required work |

A publisher advances the shared engine every 10 ms, matching the production
epoch interval. Barrier synchronization aligns worker starts; each worker emits
its own elapsed time in the JSON record. `elapsed_ns` is the slowest worker's
time for the batch. `operations_per_second` is aggregate worker throughput.
For per-worker latency or boundary cost, divide `elapsed_ns` by
`operations / workers`. Do not treat loop iterations as fuel decisions or claim
the timer workload measures filesystem or network I/O.

The one-shot preflight uses the unchanged deterministic `FuelObserver`, waits
for two distinct suspensions, publishes authority and verifies completion.
Timed samples omit that observer and use periodic publication. Tick phase and
OS wake latency make this case useful for gross regressions and functional
coverage, not nanosecond attribution. Compute and host-return cases are the
sensitive measurements of hot-path cost.

The existing `metrics_http::measure_http_scrapes` measures 200 loopback HTTP
scrapes per cache policy/phase, with connection reuse and complete body transfer.
It excludes Prometheus parsing. Keep each tree's own HTTP test because the
instrumented endpoint adds a required collector field to `AdminService`.

## Reproduction

Copy `tests/fuel_overhead.rs` unchanged into an isolated baseline checkout at
`400ec876`. `tests/support` already exists there. Build both trees fully before
timing; never benchmark while either build is running:

```sh
cargo test --release --locked --test fuel_overhead --test metrics_http --no-run
```

Run at least three alternating baseline/instrumented process pairs on an idle
machine, reversing the first side on alternating pairs. Save full raw output:

```sh
WW_FUEL_BENCH_LABEL=baseline cargo test --release --locked \
  --test fuel_overhead measure_fuel_operations -- --ignored --exact --nocapture
cargo test --release --locked --test metrics_http measure_http_scrapes \
  -- --ignored --exact --nocapture
```

Set `WW_FUEL_BENCH_LABEL=instrumented` in the other tree. Running the already
built executable directly with the same test arguments also avoids Cargo work.
The wrapper gives each subprocess a hard 180-second deadline. The fixture
execution deadline is ten seconds; no threshold asserts performance.

Use `scripts/analyze_fuel_overhead.py --pair baseline-1.log instrumented-1.log
--pair baseline-2.log instrumented-2.log --pair baseline-3.log instrumented-3.log`
to report per-pair changes and pooled distributions. Fuel and HTTP JSON may be
in the same log. Keep the raw logs with compiler version, hardware, OS, tree
SHAs and harness checksum. Report median and nearest-rank p95, within-tree
dispersion and individual paired changes before interpreting pooled values.

## Results

Measured on 2026-10-26 using an Apple M5 Max (18 logical CPUs), macOS 26.6.2
and Rust 1.96.0. Both trees used the committed release profile and lockfile;
release builds finished before timing. The baseline was
`400ec8768acee7ee3cfcbd1e8dca2bd0b0b91cca`. Both checkouts' harness SHA-256 was
`3858b61cffc9bdfb8c0d4675e6fa8a5893915d8427416f2113773c5bf078f6b8`.

Six alternating process pairs provide 186 fuel samples per case/worker count
and 1,200 HTTP samples per cache policy/phase per tree. The first three pairs
were followed by three balanced confirmation pairs after individual exceptions
triggered investigation. Code, harness and budgets remained unchanged. Smoke
output is excluded; all measured samples, including outliers, are retained.
All 12 processes reported `debug_assertions=false` and `timed_observer=false`;
each untimed preflight observed five suspensions and successful completion.

Raw logs are `.context/fuel-metrics/{baseline,instrumented}-{1,2,3,4,5,6}.log`.
The report uses `.context/fuel-metrics/analysis-six.jsonl`, generated with:

```sh
python3 scripts/analyze_fuel_overhead.py \
  --pair .context/fuel-metrics/baseline-1.log .context/fuel-metrics/instrumented-1.log \
  --pair .context/fuel-metrics/baseline-2.log .context/fuel-metrics/instrumented-2.log \
  --pair .context/fuel-metrics/baseline-3.log .context/fuel-metrics/instrumented-3.log \
  --pair .context/fuel-metrics/baseline-4.log .context/fuel-metrics/instrumented-4.log \
  --pair .context/fuel-metrics/baseline-5.log .context/fuel-metrics/instrumented-5.log \
  --pair .context/fuel-metrics/baseline-6.log .context/fuel-metrics/instrumented-6.log
```

### Scheduled execution

Times are pooled median milliseconds per worker's complete guest command;
four-worker samples use the slowest worker. Changes compare identical work.

| Case | Workers | Baseline median ms | Instrumented median ms | Median change | p95 change |
| --- | ---: | ---: | ---: | ---: | ---: |
| Compute | 1 | 28.663 | 28.797 | +0.47% | -1.57% |
| Compute | 4 | 30.912 | 30.978 | +0.21% | -3.93% |
| Timer I/O | 1 | 40.536 | 40.485 | -0.13% | -0.03% |
| Timer I/O | 4 | 40.385 | 40.654 | +0.67% | +0.18% |

Every pooled measure is within the 3% median and 5% p95 caps. Individual
exceptions are retained and examined below; pooled compliance alone was not
treated as sufficient evidence.

### Repeated host-return boundaries

Values are nanoseconds per marked host-return boundary, using per-worker time
even with four workers. The batch includes the clock call and real fuel
settlement/installation; it does not isolate an atomic instruction.

| Workers | Baseline median | Instrumented median | Added median | Paired added medians, runs 1–6 |
| --- | ---: | ---: | ---: | --- |
| 1 | 234.67 | 241.30 | +6.62 | +4.11, +4.07, +8.79, +7.06, +3.92, +9.82 |
| 4 | 381.50 | 373.66 | -7.84 | -88.96, +12.20, -11.81, -61.57, +48.71, -1.66 |

The positive one-worker cost is consistent across all six pairs and well below
50 ns. Four-worker results have broad, overlapping distributions: pooled
baseline p05–p95 spans 319.98–494.73 ns and instrumented spans
330.15–469.97 ns. The negative aggregate difference is consistent with scheduling
variation; no performance benefit is inferred. Every paired median is below
the 150 ns contention cap; the largest positive paired difference is 48.71 ns.

### One-shot suspension/resumption

Pooled median command latency changed from 68.334 to 69.426 ms with one worker
(+1.60%) and from 67.413 to 67.604 ms with four (+0.28%). Pooled p95 changed
-0.09% and +0.11%. First-pair median changes were +16.14% and +15.60%, or
+9.741 and +9.289 ms, close to one 10 ms epoch. In the remaining five pairs,
one-worker median changes ranged from -1.57% to +2.21%; four-worker changes
ranged from -2.68% to -0.59%.

The initial shift is consistent with epoch-phase and wake scheduling variation.
This periodic experiment cannot attribute nanosecond collector cost or prove
phase alignment from wall time. It demonstrates functioning suspension and
resumption without a persistent cross-pair latency increase of one epoch.
No percentage acceptance cap was added after observing these results.

### HTTP scrape cost

Values are pooled microseconds. Each cache policy runs in a new subprocess;
the phases precede and follow compilation, not deployment.

| Cache / phase | Baseline p50 | Instrumented p50 | Baseline p95 | Instrumented p95 | Added p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Disabled / before | 46.65 | 48.50 | 90.79 | 79.63 | -11.17 |
| Disabled / after | 44.08 | 46.33 | 55.92 | 55.96 | +0.04 |
| Enabled / before | 94.17 | 88.88 | 117.83 | 111.88 | -5.96 |
| Enabled / after | 76.56 | 67.58 | 92.67 | 91.54 | -1.12 |
| Fallback / before | 44.96 | 46.42 | 57.13 | 63.63 | +6.50 |
| Fallback / after | 43.31 | 44.83 | 51.25 | 54.58 | +3.33 |

The largest positive pooled p95 delta is 6.50 microseconds, below 20. The
HTTP test makes one primer scrape before its 200 timed requests, so its
results include early runtime/CPU scheduling effects. Enabled-cache traces
show strong within-run drift; negative aggregate deltas must not be described
as faster metrics rendering. No warmup samples were retrospectively discarded.

The six new fixed-label series and descriptors add **502 bytes**: responses
grew from 1,764 to 2,266 bytes in every measured policy/phase. This is below
the 1,024-byte budget. Larger counter values add only bounded decimal digits.

### Every individual cap exception

| Pair | Case | Observed change | Fixed cap |
| --- | --- | ---: | ---: |
| 1 | Compute, one worker, median | +3.33% | +3% |
| 1 | Timer I/O, four workers, median | +3.31% | +3% |
| 2 | HTTP disabled/before, p95 | +27.333 microseconds | +20 microseconds |
| 6 | HTTP disabled/before, p95 | +20.208 microseconds | +20 microseconds |
| 6 | Compute, one worker, p95 | +22.92% | +5% |

For first-pair compute, baseline medians moved from 27.956 to 28.547 to
29.213 ms over the first three runs, while instrumented medians were 28.887,
28.499 and 28.688 ms. The remaining five paired median changes were
-0.17%, -1.80%, -0.94%, +2.05% and -0.27%.
Four-worker I/O's subsequent paired changes were +0.16%, +0.52%, +0.12%,
+0.91% and +0.72%. The initial median exceptions did not persist.

The final compute p95 exception comes from two adjacent instrumented samples
(indices 18 and 19): 36.661 and 37.505 ms. All other 29 samples in that run
were at most 29.655 ms; baseline p95 was 29.825 ms. Nearest-rank p95 of 31
samples selects the second-largest observation, so those two excursions set
instrumented p95 to 36.661 ms. Median changed -0.27%. The other five pairs'
one-worker compute p95 changes ranged from -2.31% to +1.79%, and the final
pair's four-worker compute p95 decreased 5.07%. This localized two-sample
excursion, without a median or repeated tail shift, supports transient
execution/scheduling variation. There is no scheduler trace identifying its
cause, so it is not presented as proven OS descheduling or removed from the
analysis.

HTTP disabled/before paired p95 deltas were +0.833, +27.333, -3.958, -30.876,
-35.125 and +20.208 microseconds. The slow early mode appears on both trees.
In instrumented pair 2, 50-scrape-quarter medians were 81.62, 52.83, 48.67
and 46.96 microseconds, versus baseline 49.21, 47.69, 45.73 and 45.79.
Its 41 responses above 80 microseconds occurred at indices 13–58. Conversely,
baseline pairs 4 and 5 had first-quarter medians of 90.40 and 92.10
microseconds, while instrumented medians were 54.02 and 53.71; baseline
last-quarter medians returned to 45.23 and 45.42. In pair 6, the first-quarter
medians were 55.00 baseline and 61.27 instrumented, converging to 41.92 and
42.46 in the last quarter. These crossovers support transient startup/scheduling
variation rather than a stable added 20-microsecond scrape cost.

### Acceptance judgment

Accept the instrumentation within the stated measurement scope. The sensitive
single-worker boundary case shows a small repeatable cost, every paired
boundary result is within budget, compute/I/O pooled and typical paired
results are within budget, and HTTP payload growth and pooled p95 deltas are
within budget. The individual exceptions above were investigated using sample
order and both-tree crossover evidence; they do not establish a meaningful
unresolved instrumentation regression. This is a reasoned acceptance judgment,
not a claim that every run passed or a statistical upper bound on tail latency.

The six-pair investigation stops here rather than resampling until every run
is green. No further control is required for this bounded counter landing.
If a later workload reproduces persistent tail growth, the useful next control
is paired wall-time/thread-CPU-time or scheduler tracing for the compute case,
and separate startup versus warmed HTTP measurements. Another set of identical
aggregate runs would not identify the cause. These measurements do not cover
network/filesystem guest I/O, every CPU architecture, or production load.
