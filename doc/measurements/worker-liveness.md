# Executor worker liveness qualification

Baseline: `792729859e1019feb9a01f0c64f7fdce0b92933e` (`origin/master` verified at
work start). Policy and limits: [worker liveness](../worker-liveness.md).

## Correctness evidence

| Case | Evidence |
| --- | --- |
| Idle workers | Real `ExecutorPool` publishes progress before Cell admission; concurrent external readers sample fixed worker records. |
| First blocking poll | Root publishes, then blocks on a gate before yielding to the heartbeat task. The external reader already sees running state and confirmed age. |
| Cooperative compute | Real spinning WASI P3 component advances applied fuel grants while heartbeat age repeatedly resets over 5.5 seconds. |
| Pending I/O | Real P3 clock fixture completes eight clock calls and polls pending on its 60-second async wait. Unticked engine and nine grants establish the wait; heartbeats renew beyond the threshold. |
| Fuel suspension | Unticked engine and applied suspend counter establish actual suspension. Heartbeats renew beyond the threshold; explicit epoch publication resumes execution to a second applied suspension. |
| Blocked worker | Synchronous receive blocks one worker. The independent admin thread serves a real HTTP stalled sample after five seconds; another worker remains responsive. |
| Recovery | Releasing the gate permits a new heartbeat. The next HTTP observation clears stalled and reduces age. |
| Shutdown | Shutdown request stays visible before blocked-worker acknowledgement. Shutdown does not clear a stale age or stall bit. Runtime teardown produces stopped state. |
| Restart/identity | Overlapping pools sharing one collector are rejected. Replacement uses fresh state with indices bounded by the replacement worker count. |
| Memory/concurrency | Registry replacement tests cover changing counts; four concurrent readers exercise 4,000 observations. Storage contains one current pool and one fixed record per worker. |
| Epoch publisher | Blocking worker 0 freezes its epoch while worker 1 publishes progress. Existing epoch publication/closure test also runs. |
| Probe contracts | Real HTTP test commits kernel readiness with route checking disabled. Both `/healthz` and `/readyz` stay successful during the worker stall. |

Threshold boundary tests inject elapsed durations immediately before and at five
seconds, then inject recovery. They do not wait for wall time. Real blocked-worker
HTTP coverage uses the production monotonic clock. Gate release uses RAII and
bounded channel waits. Real P3 cases run in child processes with independent
45-second watchdogs. Polling delays sample a condition; they do not establish
ordering. Metrics unit and parser tests verify bounded labels and complete
Prometheus families.

Independent static concurrency review found two defects during implementation:
publication initially began only when the spawned heartbeat task ran, and
shutdown initially cleared stalled without progress. Root-first publication and
orthogonal shutdown/stall state fixed both. The final review found no remaining
material defect. A separate test review checked the real P3 ordering evidence
and prompted an applied-counter check for the second suspension.

## Validation results

- Workspace library tests: 1,136 passed, 14 ignored (`--test-threads=4`).
- New real P3 liveness tests: three passed; subprocess entry remains ignored.
- Admin HTTP suite: ten passed, three ignored.
- Metrics unit tests: fourteen passed. PID0 schema parser: two passed.
- Native P3 fixture lane: six explicitly enabled artifact tests passed.
- Real `ww` PID0/TTY/status/metrics smoke: passed in 14.78 seconds using a
  temporary offline Kubo 0.33.0 repository with no bootstrap peers or swarm
  listeners; the daemon stopped after the test.
- Workspace all-target strict Clippy, formatting and diff checks passed.

The first workspace run lacked SDK configuration and two compiled guest
fixtures. Building the pinned fixtures resolved those failures. A later fully
parallel run timed out in the existing IPNS recovery test during concurrent
compilation. That test passed alone and in the final four-thread suite; no IPNS
code changed. The timeout's cause was not independently established.

The 14 ignored library tests include opt-in measurements and artifact tests.
The six native P3 artifact tests were subsequently enabled in their dedicated
lane. This qualification does not claim every repository integration test ran.

## Reproduction

```sh
# Use the repository's pinned WASI SDK/tool paths.
make std routing-key-probe authority-probe
cargo test --workspace --locked --lib --no-fail-fast
cargo test --locked --test executor_liveness --test metrics_http
make test-p3-fixture
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check

# Requires an isolated Kubo API at WW_TEST_KUBO_ADDR.
cargo test --locked --test pid0_e2e \
  rust_pid0_tty_input_resumes_guest_status_route_remains_live_and_eof_exits_zero

cargo test --release --locked --lib measure_worker_ -- --ignored --nocapture
WW_EXECUTOR_BENCH_LABEL=candidate cargo test --release --locked \
  --test executor_overhead measure_executor_operations -- --ignored --exact --nocapture
```

Copy the changed `tests/executor_overhead.rs` unchanged to the baseline tree.
The harness uses APIs available on both sides. It reports immediate/mixed task
batches, sustained cooperative tasks for one second per sample, cancellation,
queue rejection, and three-second idle CPU samples for 1, 4 and 32 workers.
Idle CPU uses process `getrusage`, so it includes the existing epoch publisher
and runtime wakeups. It is not a per-thread CPU attribution measurement.

## Measured costs

Apple M5 Max, macOS 26.6.2, Rust 1.96.0, AC power. Each campaign used three
alternating baseline/candidate process pairs after compilation finished. The
baseline and candidate used identical workloads and release profiles. Three
process pairs are the independent replications; within-process samples are not
additional independent runs. [Raw samples and binary/source hashes](worker-liveness-results.json)
retain both campaigns.

Initial campaign, median nanoseconds per operation:

| Workload | Workers | Baseline | Candidate | Change |
| --- | ---: | ---: | ---: | ---: |
| Sustained cooperative tasks | 1 | 3,278.60 | 3,298.38 | +0.60% |
| Sustained cooperative tasks | 4 | 2,804.21 | 2,836.26 | +1.14% |
| Immediate task batch | 1 | 1,317.26 | 1,352.04 | +2.64% |
| Immediate task batch | 4 | 833.42 | 923.91 | +10.86% |
| Full queue rejection | 1 | 4.28 | 5.58 | +30.56% |

The four-worker immediate-batch paired medians changed +12.29%, +27.18%, and
−2.14%. Its pooled p95 increased from 932.53 to 1,168.59 ns/task. These adverse
results remain part of qualification; they were not discarded.

A separate follow-up added one-second sustained immediate-task samples to
check whether the short-batch effect persists across repeated heartbeats. To
avoid relinking the unused Host CLI, the identical benchmark source was built
as a release example: remove `#[test]`/`#[ignore]`, convert module doc comments to
ordinary comments, and append a `main` that sets `WW_EXECUTOR_OVERHEAD_PROBE=1`
and invokes `executor_overhead_probe()`. An independent parent process enforced
a 150-second deadline. Both sides used that same entrypoint. This is a separate
experiment, not a replacement acceptance gate.

Follow-up median nanoseconds per operation:

| Workload | Workers | Baseline | Candidate | Change |
| --- | ---: | ---: | ---: | ---: |
| Sustained immediate tasks | 1 | 1,288.88 | 1,305.78 | +1.31% |
| Sustained immediate tasks | 4 | 939.37 | 962.28 | +2.44% |
| Sustained cooperative tasks | 1 | 3,233.00 | 3,209.11 | −0.74% |
| Sustained cooperative tasks | 4 | 2,922.32 | 2,898.25 | −0.82% |
| Immediate task batch | 4 | 891.33 | 1,033.22 | +15.92% |
| Full queue rejection | 1 | 4.35 | 5.79 | +33.18% |

The queue-rejection regression repeats across all six process pairs: roughly
1.3–1.4 ns added per rejected attempt. Its cause is not established. The large
short-batch slowdown did not reproduce as sustained throughput loss, but the
follow-up does not erase the short-batch cost or establish a service-level
performance pass. No production code was tuned after observing these results.

Initial median CPU consumed during three idle seconds:

| Workers | Baseline CPU | Candidate CPU | Difference |
| ---: | ---: | ---: | ---: |
| 1 | 9.234 ms | 9.001 ms | −0.233 ms |
| 4 | 9.213 ms | 10.293 ms | +1.080 ms |
| 32 | 10.980 ms | 19.987 ms | +9.007 ms |

The follow-up measured +8.629 ms at 32 workers. The added CPU is approximately
0.3% of one CPU for this idle 32-worker case. It includes timer wakeups and
runtime scheduling, not only the atomic publication.

Four idle workers each renewed their heartbeat 11 times in a 3.003-second
observation window, excluding initial publication. An isolated copy of the
production publication function, compiled with `rustc -C opt-level=2 -C lto`,
measured 20.51 ns median across five million-iteration loops (20.20–23.34 ns).
The loop passes the record reference through `black_box` and performs an extra
black-box atomic read. This measures publication code, not timer/wakeup cost;
the real-pool CPU measurements above cover those costs.

Independent review found no demonstrated material end-to-end regression that
requires rollback. Performance acceptance remains limited to these synthetic
measurements and functional service smoke. Representative service latency,
production traffic, Linux performance, and scrape-contention overhead were not
qualified. Do not report this evidence as a zero-overhead bound or production
performance pass.

## Qualification limits

This change adds one timer task per worker and approximately four publications
per second per worker. Each publication reads the monotonic clock and loads and
stores one atomic word. The progress record is 24 bytes on the qualification
machine, excluding Arc metadata and the timer task. Startup allocates O(workers);
heartbeat publication does not allocate, lock, or log. Metrics rendering remains
O(workers) and allocates a bounded snapshot per scrape. No Wasmtime Host-return,
fuel-decision, or epoch-publication instrumentation was added.

Tests establish worker responsiveness independently of waiting Cells. They do
not prove fairness for each Cell, distinguish permanent deadlock from OS
scheduling delays, or qualify production load and whole-process failure.
No deployment, release promotion, OTLP integration, or PR #791 reconciliation
was performed. The existing Host collector already connects the CLI to the
admin endpoint; no CLI wiring change was needed.

## Changed files

| Files | Change |
| --- | --- |
| `src/services.rs`, `src/services/worker_liveness.rs` | Bounded worker records, heartbeat, external sampling, shutdown and replacement lifecycle. |
| `src/metrics.rs` | Five worker-liveness metric families on the existing Host collector. |
| `tests/executor_liveness.rs` | Real P3 compute, I/O and fuel-suspension evidence with independent watchdogs. |
| `tests/metrics_http.rs`, `tests/pid0_e2e.rs`, `tests/fixtures/host-metrics.prom` | HTTP stall/recovery coverage and complete metric schema checks. |
| `src/services_tracing_tests.rs` | Supply the worker lifecycle guard to the existing direct-loop fixture. |
| `tests/executor_overhead.rs` | Sustained workloads and idle CPU measurements on the real pool. |
| `doc/worker-liveness.md`, `doc/deployment.md`, `CHANGELOG.md` | Operator semantics, limits and release note. |
| `doc/measurements/worker-liveness.md`, `worker-liveness-results.json` | Qualification method, evidence and recorded measurements. |
