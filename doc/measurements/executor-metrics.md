# ExecutorPool instrumentation measurements

Measured on 2026-10-23 against uninstrumented `0cc71d8f` (#767).

## Method

- Apple M5 Max, 18 logical CPUs, macOS 26.6.2, Rust 1.96.0.
- Both trees used the committed release profile and lockfile. Host code uses
  optimization level 2; the release profile enables LTO.
- Three alternating baseline/instrumented process runs after compilation ended.
- Executor cases use two warmups and seven samples per run: 21 measured samples
  per case. Each sample measures a batch, not an individual task latency.
- HTTP cases use 200 scrapes per cache policy and phase per run: 600 samples
  per policy/phase, 3,600 scrapes per tree. One reusable loopback client transfers
  the complete response. Timings exclude Prometheus parsing.
- Executor futures use immediate completion or bounded compute with four yields.
  The benchmark has one producer and one or four workers. It does not measure
  Wasmtime guest execution, real guest I/O, fuel hooks, or many-producer contention.
- Cancellation cost includes worker/runtime shutdown and thread joins, divided
  by 32 cancelled tasks. Negative differences are scheduling variation, not
  evidence that instrumentation improves performance.

The budget recorded before measurements was at most 1 microsecond added median
cost per immediate/mixed task, 25 ns per full rejection, 20 microseconds added
HTTP p95, and 1,024 added response bytes. All measured cases meet those bounds.
This budget applies only to this admission-boundary slice.

## Executor results

Values are pooled sample medians in nanoseconds per operation.

| Case | Workers | Baseline | Instrumented | Difference |
| --- | ---: | ---: | ---: | ---: |
| `cancel_and_join` | 1 | 2105.5 | 2001.3 | -104.2 |
| `cancel_and_join` | 4 | 2450.5 | 2058.6 | -391.9 |
| `full_queue_rejection` | 1 | 4.1 | 5.6 | +1.5 |
| `immediate_batch` | 1 | 1316.4 | 1368.2 | +51.8 |
| `immediate_batch` | 4 | 818.5 | 847.1 | +28.6 |
| `immediate_serial` | 1 | 14058.6 | 14057.5 | -1.1 |
| `immediate_serial` | 4 | 14016.9 | 13664.2 | -352.7 |
| `mixed_batch` | 1 | 3220.7 | 3257.0 | +36.3 |
| `mixed_batch` | 4 | 2750.0 | 2755.7 | +5.7 |

Immediate batched completion increased by 3.94% with one worker and 3.49% with
four workers. Mixed work increased by 1.13% and 0.21%. The four-worker immediate
batch p95 rose from 886.5 to 1,311.0 ns/operation; tail scheduling variation is
visible even when median differences are small. Full rejection increased by
35.1%, or 1.45 ns per call.

## HTTP scrape results

Values are microseconds. Each phase has 600 samples per tree.

| Cache / phase | Baseline p50 | Instrumented p50 | Baseline p95 | Instrumented p95 |
| --- | ---: | ---: | ---: | ---: |
| disabled after compile | 43.19 | 42.10 | 60.17 | 54.04 |
| disabled before compile | 45.52 | 46.23 | 84.29 | 94.62 |
| enabled after compile | 72.00 | 69.69 | 82.46 | 81.96 |
| enabled before compile | 75.50 | 74.71 | 93.29 | 95.71 |
| fallback after compile | 41.71 | 42.29 | 52.62 | 55.83 |
| fallback before compile | 44.08 | 43.75 | 62.92 | 63.71 |

The largest positive p50 difference was 0.71 microseconds. The largest positive
p95 difference was 10.33 microseconds. Response size increased from 872 to 1,764
bytes: 892 bytes for seven executor series. Fixed strings, fixed labels, and
integer widths bound the response independently of Cell churn.

## Reproduction

Copy `tests/executor_overhead.rs` unchanged to a checkout of `0cc71d8f`.
Build both checkouts before measuring. Run these commands in each checkout,
alternating the resulting executables to avoid rebuilding between samples:

```sh
WW_EXECUTOR_BENCH_LABEL=baseline cargo test --release --locked \
  --test executor_overhead measure_executor_operations -- --ignored --exact --nocapture
cargo test --release --locked --test metrics_http measure_http_scrapes \
  -- --ignored --exact --nocapture
```

Use `WW_EXECUTOR_BENCH_LABEL=instrumented` for the instrumented checkout.
The tests print JSON containing all raw timing samples. The existing metrics
test runs each cache policy in a separate subprocess. No benchmark asserts a
machine-dependent timing threshold.

## Operation-to-HTTP validation

`tests/metrics_http.rs` drives real pools through admission, completion, panic,
full/closed rejection, queued cancellation, and shutdown. One observed sequence
had two occupied slots and two admissions; after further operations and teardown
the same endpoint exposed zero slots, four accepted admissions, one closed
rejection, one completion, two cancellations, and one panic. Another case filled
the queue, observed one full rejection, and retired every admitted slot.

`tests/pid0_e2e.rs` runs the actual `ww run` binary with Kubo and native P3
kernel/status artifacts. Its admin endpoint exposes seven zero executor samples
before boot, then one occupied slot and one accepted admission after PID0
readiness and a successful status request. Child Cells do not add pool slots.

These are local production-path tests. No manual deployment or live-production
load test was performed.

## Remaining #752 work

The next fuel slice must count actual applied decisions with fixed
`policy={scheduled,oneshot}` and `decision={grant,suspend,exhausted}` labels.
Include the initial decision in `Proc::run` and grants after epoch suspension;
the existing observer settlement sites alone omit those events.

Keep desired EWMA `quantum`, effective `authority_grant`, and installed physical
fuel separate. Record physical installation only after successful `set_fuel`.
`YIELD_RESERVE` is physical runtime machinery and must not increase reported
guest authority. Consumption and one-shot total/per-epoch ledgers require their
own definitions and conservation checks at fuel-flushed boundaries. Epoch
callbacks remain notification-only. Preserve the test-only `FuelObserver`.

Later slices cover bounded quantum/ratio distributions, consumption and ledger
conservation, yield/suspend/resume/exhaustion outcomes, spawn latency, and sampled
diagnostics. Benchmark compute-bound, guest I/O, mixed, and one-shot stress
workloads against an uninstrumented baseline before instrumenting fuel hooks.
Worker-stall detection remains separate under #390.
