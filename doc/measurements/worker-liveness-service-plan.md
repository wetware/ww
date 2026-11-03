# Prospective worker-liveness performance qualification

Recorded before additional timing on 2026-11-01. Independent reviewer: performance_plan.

## Decision and evidence

Run one bounded representative Cell-service comparison before recommending merge.
The existing +10.9–15.9% short-batch cost is adverse evidence that remains valid.
The +1.3–2.4% sustained cost does not erase that result.

`tests/executor_overhead.rs` explicitly measures synthetic futures, without Wasmtime guest execution.
`tests/lifecycle_overhead.rs` measures real P3 `Builder` and `Proc` operations outside `ExecutorPool`.
`tests/fuel_overhead.rs` uses its own worker threads and epoch publisher, also outside `ExecutorPool`.
Consequently, neither existing harness answers whether a real Cell's admission, startup, execution and cleanup amplify or absorb the short-batch cost.

The additional comparison must put existing real P3 fixtures inside `ExecutorPool::spawn`.
Use `pool.runtime_engine()` so worker 0 remains the production epoch publisher.
Use the existing `Builder::ordinary`, `Program::Precompiled`, filesystem ownership and completion patterns.
Include request creation, admission, Store construction, instantiation, guest execution, result delivery and filesystem cleanup in end-to-end completion measurements.
This qualifies a representative in-process Cell service boundary. It does not qualify HTTP ingress, kernel RPC admission, Kubo, production traffic or Linux.
The existing real PID0 smoke remains separate functional evidence.

Durable recall: `gbrain search "ExecutorPool worker liveness performance Cell admission" --source default` returned no relevant performance decision. The narrower `ExecutorPool` search returned August architecture transcripts. Relevant cached transcript content in `.context/liveness-history-aug17.txt` identifies worker-0 epoch publication and distinguishes root admission from child Cell execution. Current `src/services.rs`, `tests/executor_liveness.rs` and the three measurement harnesses confirm the distinctions above. No pre-existing numerical service SLO was found. The budgets below are prospective engineering acceptance limits for this bounded experiment, not a claim about an owner-approved production SLO.

## Frozen harness and workload

Use one byte-identical benchmark source on baseline and candidate. Compile release binaries before timing. No production source changes, runtime instrumentation, fuel policy changes or threshold changes are allowed during qualification.

Precompile all fixtures outside timed intervals against the pool's runtime engine. Use no persistent compilation cache. Do not install `FuelObserver` inside timed work. Use the same Host filesystem authority and limits on both sides. Record exact source, dependency lockfile, binaries and baseline/candidate tree hashes.

Run both 1-worker and 4-worker configurations. Each process executes this fixed workload order:

1. Pool startup and real Cell startup: three warmup pools followed by seven measured pools. Measure pool construction separately. Then measure real Cell admission/startup on a persistent pool using three warmups and seven measured serial commands. Each ExecutorPool creates a distinct Wasmtime Engine, so precompiled Components cannot span pools. This fallback was selected before timing and accepted by the parent reviewer. Compilation remains outside every timed interval.
2. Short-lived batches: persistent pool; three warmup batches followed by seven measured batches of 256 no-op real P3 commands, admission width 32. Record each command's admission-to-built and admission-to-completion latency, each batch's elapsed time, total CPU and completed command count. Build the command fixture from `tests/lifecycle_overhead.rs` without changing its behavior. Await filesystem completion before reporting a command complete.
3. Sustained cooperative Cells: persistent pool; one untimed warmup batch, then six seconds of finite P3 compute Cells, width equal to worker count. Reuse the finite 4,000,000-iteration scheduled compute fixture from `tests/fuel_overhead.rs`. Continue the final admitted batch to completion. Record actual elapsed time, completed commands, process CPU and per-command completion latencies. An untimed preflight must establish that the fixture crosses fuel grants under the production pool epoch publisher. Report completion throughput and CPU per completed command, not fuel grants as work.
4. Ordinary pending I/O: fresh persistent pool after the compute pool has shut down; one P3 timer-wait Cell per worker, each waiting 5.5 seconds through the real WASI clock import. Establish entry into Pending with an exact nine-grant delta after each sequential admission, then assert the aggregate fuel snapshot stays unchanged through the CPU interval. The fixture executes fewer than 100 instructions before its timer wait, below the 10,000-instruction yield interval. Epoch callbacks record transitions without granting or yielding. Record executor thread names and assert distinct worker identities for pending-I/O and cancellation Cells. Do not infer state from sleep. Measure process CPU over the established pending interval and record wakeup/completion latency relative to the requested deadline. Record first Proc::run poll time and established Pending time. Timer installation lies between those timestamps. Gate the conservative upper bound measured from first poll, and retain the lower bound measured from established Pending. The upper bound includes any first-poll setup before timer installation; do not label it an exact timer deadline measurement. Use the pool runtime engine and ordinary scheduled fuel policy. Record actual wait duration.
5. Shutdown: record `shutdown_and_join()` latency for every pool above. Separately shut down a pool with one established pending P3 timer Cell per worker and verify cancellation/completion/cleanup. Seven measured shutdown samples after three warmups; no blocked synchronous worker in this performance experiment.

Candidate worker progress, healthy I/O/cooperative state and stopped state must be checked with the existing real P3 liveness tests or a separate untimed diagnostic execution. Keep candidate-only liveness API reads out of paired timed work. Compare shutdown semantics and completion counts on both binaries. Existing externally observed blocked-worker/recovery HTTP coverage remains mandatory correctness evidence.

All operations have a 30-second independent watchdog; a process has a 120-second watchdog. Bound the entire 12-process campaign at 25 minutes. Any correctness failure stops the campaign.

## Process controls and execution order

Run exactly three four-process blocks, sequentially:

- Block 0: baseline, candidate, candidate, baseline.
- Block 1: candidate, baseline, baseline, candidate.
- Block 2: baseline, candidate, candidate, baseline.

The two executions of each binary within each block are identical-binary controls. Hash each executable before and after the campaign. Do not relink, copy a different executable over a path, or run builds/tests concurrently. Save every process output, including invalid and failed runs. A process is the replication unit; individual Cells are not independent process replications.

For each process, report median and p95 admission/startup and completion latency, batch elapsed cost per Cell, sustained completions/second, CPU per completed Cell, pending CPU divided by observed wall time, and median/p95 shutdown latency. Compute p95 within each process. Do not pool Cell samples across processes to manufacture replication.

For each block and metric, use the geometric mean of the two same-binary process summaries. Compare candidate against baseline as a cost ratio. For throughput, invert throughput to seconds per completed Cell. Report all three block ratios and their median.

Record `pmset -g batt` before the first process, once per second throughout every process and interprocess interval, and after the last process. Any missing AC indication invalidates the campaign. Capture temperature/thermal-pressure and competing build/process evidence where available. Do not silently delete affected samples. A failed environmental validity gate produces INCONCLUSIVE, not a rerun invitation.

## Prospective acceptance limits

These limits apply independently for each worker count. A favorable metric cannot offset an adverse metric.

| Metric | Maximum permitted candidate increase |
| --- | --- |
| Median Cell admission-to-built latency | Greater of 5% of baseline or 10 microseconds |
| Median Cell admission-to-completion latency, short batch cost per Cell | Greater of 5% of baseline or 10 microseconds |
| P95 Cell admission-to-built and admission-to-completion latency | Greater of 10% of baseline or 50 microseconds |
| Sustained seconds per completed compute Cell | 5% |
| Sustained CPU per completed compute Cell | 5% |
| Pool creation median (separate from Cell startup) | Greater of 5% of baseline or 100 microseconds |
| Pending-I/O CPU divided by wall time | +0.005 CPU seconds/second, or +0.5% of one CPU |
| Pending-I/O deadline lateness median/p95 | +1 ms / +5 ms |
| Shutdown latency median/p95 | Greater of 10% of baseline or 1 ms / 5 ms respectively |

The 5% sustained budget permits a small diagnostic cost while rejecting a service-level recurrence of the observed 10.9–15.9% short-batch slowdown. Absolute startup and shutdown allowances avoid interpreting tiny scheduling differences as service regressions. The pending CPU allowance remains below 1% of one CPU and uses an absolute limit because percentage changes near idle are misleading. These budgets do not redefine the five-second stall threshold or 250-ms heartbeat interval.

For each metric, compute the absolute difference between same-binary process summaries within each block. The measurement is stable only when both baseline and candidate control differences are no more than half the applicable acceptance allowance in at least two blocks, and no more than the full allowance in the remaining block.

PASS for a metric requires valid environment/identity/correctness, stable controls, and all three candidate block increases at or below the applicable acceptance allowance.

CONFIRMED REGRESSION requires valid environment/identity/correctness, stable controls, and all three candidate block increases above the applicable acceptance allowance. Stop further qualification and report NO-GO if any metric meets this condition. Report the affected behavior and measured tradeoff. Attribute a specific mechanism only when supported; otherwise label the mechanism unknown.

INCONCLUSIVE applies to any remaining pattern: unstable controls, mixed crossings, invalid power/environment, deadline failure, missing samples, unverified fixture behavior or inability to measure a required case. Do not call INCONCLUSIVE a pass. No additional timing campaign or numerical retuning is authorized by this plan.

Overall representative acceptance requires every required metric to PASS and every correctness gate to pass. Otherwise report NO-GO pending explanation or user-directed follow-up. Preserve all previous short-batch, queue-rejection and idle-CPU findings regardless of this campaign's outcome.
