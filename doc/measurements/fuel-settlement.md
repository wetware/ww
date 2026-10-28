# Settled fuel instrumentation measurements

Baseline: `cdf474b4cce0ed99f341d01fd6c3818f64a11d04` (#769).
The baseline includes applied fuel-decision instrumentation. These measurements
compare the additional settlement counter using the same committed release
profile, lockfile, Rust toolchain, machine, and `tests/fuel_overhead.rs` harness.

Current status, 2026-10-28: software diagnostics support separating concurrent
settlement destinations. The selected correction uses 32 bounded, padded atomic
shards. Validation stopped because the local workspace command used the release
profile for a test that requires a debug-only barrier. Final candidate performance
acceptance has not run. Linux ARM64 functional validation passed; Linux x86-64
CI remains pending. The original six failing pairs remain historical evidence.

## Acceptance budget, fixed before measurement

- Scheduled compute and timer I/O: at most 3% added median elapsed time and
  5% added p95 elapsed time for identical guest work, at one and four workers.
- Marked host returns: at most 25 ns added median time per boundary with one
  worker and 100 ns with four concurrent workers. Use per-worker batch time.
  These costs are incremental to #769's measured +6.62 ns single-worker cost.
- HTTP: at most 20 microseconds added p95 scrape time and 512 added bytes.
- One-shot suspension/resumption: report latency and dispersion separately.
  The deterministic preflight must observe at least two suspensions and
  successful completion. Periodic 10 ms waits cannot resolve atomic cost.

Use three alternating baseline/instrumented process pairs, reversing order
between pairs. If a cap exception needs investigation, allow one further block
of three alternating pairs with unchanged code, harness and budget. Retain all
individual exceptions and raw observations. Stop for unresolved persistent
overhead; do not rerun until results pass. Complete builds before timing.

The guest workloads, 31 samples, three warmups, persistent worker threads,
shared collector, and 200-scrape HTTP methodology are defined in
[fuel-decisions.md](fuel-decisions.md). No `FuelObserver` runs during timing.

## Original candidate decision: stopped, not accepted

All six paired runs exceeded the four-worker host-return budget of 100 ns added
median time per boundary. The pooled addition was **159.70 ns (+40.35%)**.
The single permitted confirmation block reproduced the regression. This meets
Landing 3's unresolved hot-path regression stop condition. Do not push or merge
this candidate. No PR, fresh remote CI, merge, deployment, or release promotion
was performed for this candidate.

Those six measurements were consistent with contention on the additional shared
atomic, but did not establish its cause. No profiler ran during that attempt.
The attempt stopped without changing the scheduler, accounting, collector, or
budget. The resumed diagnosis and selected correction are documented below;
the original candidate remains rejected.

## Environment and sample accounting

Measured on 2026-10-26 with an Apple M5 Max (18 logical CPUs), macOS 26.6.2
(build 25G83), Rust 1.96.0 (`ac68faa20`), and LLVM 22.1.2. Both trees used the
committed release profile and lockfile. Both builds finished before timing.
The original candidate is preserved at
`937d74ece14c6e957cddee7af596eb3aa0142860`.

The unchanged `tests/fuel_overhead.rs` SHA-256 was
`3858b61cffc9bdfb8c0d4675e6fa8a5893915d8427416f2113773c5bf078f6b8`.
The lockfile SHA-256 was
`93f6a99a5a6de3885f02b6181aaa2c2eb15e7f159145d82fa062c7baf5fa077f`.

Pairs 1–3 form the initial block; pairs 4–6 form the permitted confirmation
block. Odd pairs ran baseline first; even pairs ran instrumented first. Code,
harness, and budgets stayed unchanged between blocks. All measured samples
were retained. Each build contributed 186 fuel samples per case/worker count
and 1,200 HTTP samples per policy/phase. Every untimed one-shot preflight
observed five suspensions and completed successfully. Timed samples used no
`FuelObserver`.

The report uses nearest-rank p05 and p95 and the median. Four-worker fuel
samples use the slowest worker's batch time, divided by work per worker when
reporting boundary cost. These are complete guest workloads, not isolated
atomic instruction timings. Negative differences do not establish a speedup.

## Pooled fuel results

Compute, timer I/O, and one-shot values are milliseconds per guest command.
Host-return values are nanoseconds per marked boundary. Each range is p05–p95.

| Case | Workers | Unit | Baseline median [p05–p95] | Instrumented median [p05–p95] | Median change | p95 change |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| One-shot | 1 | ms | 68.689 [60.709–74.671] | 68.594 [60.141–74.726] | -0.14% | +0.07% |
| One-shot | 4 | ms | 69.146 [59.087–74.314] | 68.663 [59.157–74.330] | -0.70% | +0.02% |
| Compute | 1 | ms | 28.880 [27.563–30.671] | 28.919 [27.435–30.468] | +0.13% | -0.66% |
| Compute | 4 | ms | 30.939 [30.577–31.842] | 30.907 [30.572–31.610] | -0.10% | -0.73% |
| Host returns | 1 | ns/boundary | 244.695 [234.437–257.569] | 238.611 [231.345–251.271] | -2.49% | -2.44% |
| Host returns | 4 | ns/boundary | 395.824 [336.643–508.264] | 555.522 [502.002–596.015] | +40.35% | +17.26% |
| Timer I/O | 1 | ms | 40.567 [39.380–40.971] | 40.507 [39.402–40.974] | -0.15% | +0.01% |
| Timer I/O | 4 | ms | 40.629 [39.649–41.133] | 40.619 [39.695–41.104] | -0.03% | -0.07% |

One-worker host returns added -6.08 ns at the median and -6.30 ns at p95.
Four-worker host returns added 159.70 ns at the median and 87.75 ns at p95.
The one-worker result does not offset the persistent four-worker failure.
One-shot results include deliberate 10 ms epoch waits and cannot resolve atomic
cost. All pooled compute and timer I/O changes are within their caps.

## Individual pairs and exceptions

Each fuel pair contains 31 samples per case/worker count on each build.
The following table retains all paired median and p95 changes. Host-return
absolute median differences use ns/boundary; other absolute differences use ms
per guest command. A dash means no acceptance cap applies to one-shot latency.

| Pair | Case | Workers | Added median | Median change | p95 change | Cap result |
| ---: | --- | ---: | ---: | ---: | ---: | --- |
| 1 | One-shot | 1 | -0.241 | -0.35% | -0.06% | — |
| 1 | One-shot | 4 | -1.147 | -1.65% | -0.02% | — |
| 1 | Compute | 1 | -0.107 | -0.37% | +5.20% | FAIL p95 |
| 1 | Compute | 4 | +0.006 | +0.02% | +0.20% | Pass |
| 1 | Host returns | 1 | -7.933 | -3.20% | -2.04% | Pass |
| 1 | Host returns | 4 | +152.442 | +38.07% | +29.19% | FAIL |
| 1 | Timer I/O | 1 | +0.127 | +0.31% | +0.01% | Pass |
| 1 | Timer I/O | 4 | +0.233 | +0.57% | +0.03% | Pass |
| 2 | One-shot | 1 | -1.052 | -1.51% | -0.03% | — |
| 2 | One-shot | 4 | -1.641 | -2.37% | -2.24% | — |
| 2 | Compute | 1 | +0.483 | +1.67% | -6.16% | Pass |
| 2 | Compute | 4 | +0.185 | +0.60% | -9.65% | Pass |
| 2 | Host returns | 1 | -5.464 | -2.22% | -1.88% | Pass |
| 2 | Host returns | 4 | +102.093 | +24.91% | +22.88% | FAIL |
| 2 | Timer I/O | 1 | -0.024 | -0.06% | -0.05% | Pass |
| 2 | Timer I/O | 4 | +0.142 | +0.35% | -0.24% | Pass |
| 3 | One-shot | 1 | +1.617 | +2.37% | +0.21% | — |
| 3 | One-shot | 4 | -0.528 | -0.75% | +0.20% | — |
| 3 | Compute | 1 | +0.091 | +0.31% | +5.40% | FAIL p95 |
| 3 | Compute | 4 | -0.150 | -0.49% | -0.23% | Pass |
| 3 | Host returns | 1 | -5.683 | -2.32% | -5.08% | Pass |
| 3 | Host returns | 4 | +153.139 | +40.03% | +20.61% | FAIL |
| 3 | Timer I/O | 1 | -0.145 | -0.36% | -0.07% | Pass |
| 3 | Timer I/O | 4 | +0.210 | +0.52% | -0.15% | Pass |
| 4 | One-shot | 1 | +0.620 | +0.92% | +0.20% | — |
| 4 | One-shot | 4 | -1.409 | -2.08% | +0.07% | — |
| 4 | Compute | 1 | -0.337 | -1.16% | +0.44% | Pass |
| 4 | Compute | 4 | -0.040 | -0.13% | +2.44% | Pass |
| 4 | Host returns | 1 | -7.683 | -3.12% | -6.40% | Pass |
| 4 | Host returns | 4 | +192.833 | +50.19% | +19.64% | FAIL |
| 4 | Timer I/O | 1 | +0.209 | +0.52% | +0.11% | Pass |
| 4 | Timer I/O | 4 | -0.065 | -0.16% | -0.11% | Pass |
| 5 | One-shot | 1 | +1.224 | +1.79% | +0.13% | — |
| 5 | One-shot | 4 | +0.546 | +0.80% | +0.14% | — |
| 5 | Compute | 1 | +0.188 | +0.65% | -4.50% | Pass |
| 5 | Compute | 4 | -0.042 | -0.13% | +1.00% | Pass |
| 5 | Host returns | 1 | -2.651 | -1.11% | -0.97% | Pass |
| 5 | Host returns | 4 | +194.923 | +52.35% | +26.30% | FAIL |
| 5 | Timer I/O | 1 | -0.286 | -0.70% | +0.15% | Pass |
| 5 | Timer I/O | 4 | -0.151 | -0.37% | -0.06% | Pass |
| 6 | One-shot | 1 | -0.926 | -1.35% | +0.06% | — |
| 6 | One-shot | 4 | +0.900 | +1.30% | +0.37% | — |
| 6 | Compute | 1 | -0.877 | -3.00% | +4.69% | Pass |
| 6 | Compute | 4 | -0.078 | -0.25% | -14.20% | Pass |
| 6 | Host returns | 1 | -5.233 | -2.17% | -0.45% | Pass |
| 6 | Host returns | 4 | +132.763 | +30.85% | +4.80% | FAIL |
| 6 | Timer I/O | 1 | -0.161 | -0.40% | +0.05% | Pass |
| 6 | Timer I/O | 4 | -0.231 | -0.57% | +0.13% | Pass |

The one-worker compute p95 exceeded 5% in pairs 1 (+5.20%) and 3 (+5.40%).
Those tail exceptions did not persist in the confirmation block or pooled
result. All six four-worker host-return medians exceeded 100 ns, including
all three confirmation pairs. No other compute, timer I/O, host-return, or
HTTP acceptance cap was exceeded.

## HTTP results

Values are pooled microseconds per complete loopback scrape with connection
reuse. Each range is p05–p95. Parsing is outside the timed interval. Compile
phases and cache modes retain the meanings in the prior measurement report.

| Cache mode | Phase | Baseline median [p05–p95] | Instrumented median [p05–p95] | Added p95 |
| --- | --- | ---: | ---: | ---: |
| disabled | after_compile | 44.000 [38.291–54.834] | 44.583 [39.458–52.792] | -2.042 |
| disabled | before_compile | 48.541 [40.000–90.459] | 47.709 [40.541–85.208] | -5.251 |
| enabled | after_compile | 74.834 [41.000–91.041] | 70.271 [40.583–92.625] | +1.584 |
| enabled | before_compile | 95.334 [80.375–110.625] | 94.354 [73.292–111.000] | +0.375 |
| fallback | after_compile | 44.292 [39.625–54.042] | 43.520 [39.334–54.500] | +0.458 |
| fallback | before_compile | 45.709 [40.000–59.167] | 44.709 [39.916–60.625] | +1.458 |

The response grew from 2,266 to 2,564 bytes (+298) in every mode/phase/pair,
below the 512-byte cap. The endpoint exposes 21 fixed series in nine families.
Every individual HTTP pair stayed below the 20 microsecond added-p95 cap:

| Pair | Largest added p95 across six mode/phase combinations, µs | Added bytes in every combination |
| ---: | ---: | ---: |
| 1 | +1.750 | +298 |
| 2 | +5.292 | +298 |
| 3 | +13.833 | +298 |
| 4 | +7.250 | +298 |
| 5 | +3.625 | +298 |
| 6 | +4.501 | +298 |

## Correctness and validation

`ww_fuel_settled_units_total{policy="scheduled"|"oneshot"}` records physical
fuel units returned by `settle_and_decide`, immediately after settlement and
before epoch opening or decision application. Recording covers marked host
returns, confirmed fuel-return settlement, and successful startup settlement.
The shared Host collector retains counts after Cell and executor teardown.

The existing `physical_installed.saturating_sub(current_fuel)` calculation and
both saturating one-shot ledger debits remain unchanged. Deterministic tests
check exact before/after ledgers, partial grants, reserve overshoot, coalesced
epochs, suspension, exhaustion, cancellation, failed application, zero-budget
rejection, and unobserved termination tails. Overshoot contributes its full
physical delta; a saturated ledger can decrease by fewer units. Grant/reserve
installation, notifications, and intermediate cooperative yields add no units.

Validation evidence is local under `.context/fuel-settlement/`:

- Accounting red: five expected counter failures before recording was enabled;
  zero-consumption exclusion passed (`red-accounting.log`). HTTP red: missing
  settled-family metadata failed the real P3 contract (`red-http.log`).
- Release library validation: 1,076 unique tests passed and 12 were ignored.
  The initial workspace run found one stale exposition-count assertion. After
  changing its expected counts to nine families and 21 series, all 220 `ww`
  library tests passed; the remaining 63 `ww-cache` tests also passed.
  Receipts: `green-libraries.log`, `green-ww-library.log`,
  `green-cache-library.log`.
- Six native P3 fixture tests passed (`green-native-p3.log`). The HTTP suite
  passed seven tests with two opt-in tests ignored. Real scheduled P3 work and
  one-shot suspension/resumption changed the correct policy counters through
  actual `GET /metrics`. Retention assertions compare only fuel families.
- The real Host/PID0 boot and status-route test passed with compiled fixtures
  and a private Kubo instance. Scheduled settled units were zero before boot
  and 931,048 after the status request. The targeted cancellation/closed-clock
  test also passed. Receipts: `qa/evidence.json` and `qa/.qa-evidence/`.
- Release workspace Clippy with all targets and warnings denied passed
  (`clippy.log`). Formatting and whitespace checks passed.
- Independent source review found no substantive correctness defect
  (`adversarial-review.md`). A separate coverage audit found meaningful
  assertions for all 20 mapped paths/flows (`coverage-review.md`). These
  reviews do not establish performance acceptance. The optional Claude CLI
  review was unavailable because OAuth refresh failed.
- Final independent review recomputed all 98 analysis records from the raw logs
  and checked all 62 detailed table rows. It confirmed the performance blocker
  and found no additional source correctness defect (`final-review.md`).

Successful command tails, traps/errors, failed or timed-out instantiation,
cancellation, and Store teardown can leave physical consumption unsettled.
This metric is not total executed fuel, CPU time, or wall-clock time. No new
terminal settlement was added. See the
[settlement contract](../designs/fuel-scheduling.md#settled-fuel-consumption-metrics).

Fresh remote CI was not run for the original candidate because its performance
stop condition blocked publication. The required checks remain Test, Check formatting, Clippy,
Require changelog entry, and Solidity (Foundry). Local Rust validation does not
substitute for those checks or claim that every integration test ran.

## Evidence and reproduction

The local evidence directory preserves `environment.json`, `validated-inputs.json`,
`baseline-{1..6}.log`, `instrumented-{1..6}.log`, `analysis-three.jsonl`,
`analysis-six.jsonl`, `run_pairs.py`, build/test receipts, the baseline checkout,
and copied executables under `bin/`. `.context/` is gitignored; retain the
workspace to preserve raw evidence. The committed tables preserve the measured
results independently of those local files.

To reproduce on a fresh machine, check out the baseline above and the local
candidate commit in separate worktrees. Follow the existing build prerequisites,
including `make std routing-key-probe authority-probe`. Use the same Rust
compiler, lockfile, and machine. Build both release test executables before
starting either timer:

```sh
cargo test --release --locked --test fuel_overhead --test metrics_http --no-run
```

Copy each tree's reported `fuel_overhead` and `metrics_http` executables to
separate paths. Run those binaries from their own checkout with
`WW_FUEL_BENCH_LABEL=baseline` or `instrumented` and these arguments:

```sh
/path/to/fuel_overhead --exact measure_fuel_operations --ignored --nocapture --test-threads=1
/path/to/metrics_http --exact measure_http_scrapes --ignored --nocapture --test-threads=1
```

Use a 180-second subprocess deadline. Alternate baseline/instrumented order
across pairs. Save fuel and HTTP output for each side in the same pair log.
The retained `run_pairs.py` implements these steps without rebuilding or
overwriting logs. Do not run more unchanged-code pairs to erase this failure.

Recompute the reported comparison without running benchmarks:

```sh
python3 scripts/analyze_fuel_overhead.py \
  --pair .context/fuel-settlement/baseline-1.log .context/fuel-settlement/instrumented-1.log \
  --pair .context/fuel-settlement/baseline-2.log .context/fuel-settlement/instrumented-2.log \
  --pair .context/fuel-settlement/baseline-3.log .context/fuel-settlement/instrumented-3.log \
  --pair .context/fuel-settlement/baseline-4.log .context/fuel-settlement/instrumented-4.log \
  --pair .context/fuel-settlement/baseline-5.log .context/fuel-settlement/instrumented-5.log \
  --pair .context/fuel-settlement/baseline-6.log .context/fuel-settlement/instrumented-6.log
```

For correctness checks, rebuild the native P3 fixture with the repository's
pinned WASI SDK and wasm-tools setup before running the native P3 tests. The
Host/PID0 case additionally requires compiled first-party guest fixtures and
Kubo. Preserve the source-input hashes with the resulting validation receipts.

After an accepted consumption landing, the next narrow #752 milestone remains
desired EWMA quantum versus effective authority grant and installed/unspent
physical fuel visibility. Distributions and lifecycle instrumentation remain
outside this candidate.

## Resumed software diagnosis — 2026-10-28

These diagnostics select a correction; they do not replace the final acceptance
comparison against `cdf474b4`. They retain the Apple M5 Max, macOS 26.6.2,
Rust 1.96.0, committed release profile, and the same marked-return guest work.
No PMU controller, privileged counter programming, or system configuration
change ran in the software comparisons described here.

### Earlier recording and bank-separation controls

Two alternating diagnostic pairs disabled only the new settlement recording
path. Four-worker marked-return medians fell by 165.832 and 102.696 ns versus
the original instrumented candidate. The optimizer could remove surrounding
branch/address work with the unused write; this is not an isolated atomic cost.
The first one-worker pair instead added 105.100 ns with recording disabled.
That anomalous result is retained; the write-disabled control alone did not
establish a hardware mechanism.

Separating the single settlement bank from the decision bank recovered only
5.031 and 31.326 ns versus the original candidate. A separate two-pair comparison
against the uninstrumented baseline still added 133.537 and 143.686 ns. Keeping
one shared settlement destination with bank separation was therefore insufficient
for the unchanged 100 ns budget. Raw results remain in
`.context/fuel-settlement-resume/analysis-write-disabled.jsonl`,
`analysis-cache-separated.jsonl`, and `analysis-baseline-vs-cache-separated.jsonl`.

### Initial six-process destination comparison

One frozen release binary compared a shared atomic word, four distinct words
within one observed 128-byte line, and four distinct line-separated words.
Each sample ran 100,000 marked clock-import returns per worker. Destination
selection happened during Store preparation, before timing. The collector
allocation and the `record_settled` instruction sequence were identical across
modes; decision grants remained shared on a separate bank.

Block 1 ran shared word, same line, then separate lines. Block 2 reversed that
order. Each process ran three warmups and retained 31 measured samples per case
at one and four workers. The separate-line mapping rotated among 32 destinations
across samples; each simultaneous four-worker sample used four distinct lines.
The one-process follow-up below removed that reuse difference.

The table reports median [p05–p95] ns per marked return. Four-worker values use
the slowest worker's batch time divided by 100,000, not aggregate throughput.
Each individual row contains 31 samples; each pooled row contains 62.

| Block | Destination | One worker | Four workers |
| --- | --- | ---: | ---: |
| 1 | shared | 242.260 [233.217–251.629] | 607.642 [511.445–733.979] |
| 1 | same-line | 245.806 [239.220–254.220] | 533.435 [507.610–554.602] |
| 1 | separate-lines | 246.263 [239.728–255.356] | 463.702 [420.868–502.062] |
| 2 | shared | 251.722 [239.992–260.533] | 540.077 [509.976–571.468] |
| 2 | same-line | 245.632 [238.373–252.639] | 511.968 [495.250–552.303] |
| 2 | separate-lines | 250.465 [242.785–260.198] | 500.466 [433.646–542.695] |
| Pooled | shared | 247.876 [234.323–259.073] | 547.210 [510.090–718.240] |
| Pooled | same-line | 245.705 [239.220–253.823] | 520.582 [496.637–552.303] |
| Pooled | separate-lines | 248.632 [241.012–258.682] | 481.815 [426.808–535.179] |

All six processes passed the address and exact marked-return count checks.
Each four-worker case, including warmups, produced 13,600,136 grants and
81,599,728 settled units. The one-worker counts were 3,400,034 and 20,399,932.
Scheduled and one-shot address offsets and the separate grant bank were checked.
The initial comparison retained all compute, timer I/O, and one-shot samples,
but compute was not an equal-settlement-count control: epoch timing changes
its settlement count and unsettled tail.

Separate lines recovered 143.940 and 39.611 ns versus the shared word.
The corresponding one-worker changes were +4.003 and -1.257 ns. Distinct words
on one line recovered 74.207 and 28.109 ns versus the shared word. The further
line-separation recovery was 69.733 ns in block 1 and 11.502 ns in block 2.
That inconsistency, including drift in the first shared-word process, left
line separation uncertain. The pooled result alone did not select a design.

Evidence: `.context/fuel-settlement-resume/layout-comparison/` retains all six
logs, `samples.json`, `analysis-host4.jsonl`, process status, and frozen inputs.
The existing binary's historical filename contains `pmu`; these runs removed
all `FUEL_PMU_*` variables and emitted no PMU samples or counter calls.

### Single permitted follow-up: persistent workers, ABBAABBA

One process reused one RuntimeEngine, one collector allocation, and four
persistent OS worker threads with current-thread Tokio runtimes. A meant four
words in one line; B meant four words in four lines. Both modes now reused
exactly four destinations, removing the initial 32-destination rotation.
The fixed sequence was A B B A A B B A. Every batch ran three warmups and
31 measured samples, with 100,000 marked returns per worker per sample.
The 10 ms epoch publisher remained active. No observer or PMU call ran during
timing. Store preparation, address reporting, and snapshots stayed outside timing.

All 1,088 Store assignments matched the intended destination pattern. In the
actual allocation, scheduled destinations were `0x7af004100`, `0x7af004110`,
`0x7af004120`, and `0x7af004130` for A; B used `0x7af004100`, `0x7af004180`,
`0x7af004200`, and `0x7af004280`. The 128-byte spacing is diagnostic placement
on the measured machine, not a portable cache-line-size requirement.
The grant destination stayed shared and separate from settlement destinations.

Batch gates stopped workers before snapshots and before preparation of the
next batch. Every quiescent batch delta was exactly 81,599,728 settled units
and 13,600,136 grants across 136 executions. One-shot totals, suspensions, and
exhaustion remained zero. All 248 measured and 24 warmup rows were retained.

Each row below reports slowest-worker median [p05–p95] ns per marked return,
with 31 measured samples per batch.

| Batch | Destination | Batch latency |
| ---: | --- | ---: |
| 1 | same-line | 530.323 [507.452–710.141] |
| 2 | separate-lines | 446.057 [391.188–482.967] |
| 3 | separate-lines | 418.866 [379.778–450.217] |
| 4 | same-line | 539.850 [496.846–603.811] |
| 5 | same-line | 535.934 [516.365–585.435] |
| 6 | separate-lines | 425.855 [377.521–523.368] |
| 7 | separate-lines | 426.738 [367.289–485.202] |
| 8 | same-line | 550.285 [522.999–583.661] |

The workers remained the same OS threads throughout the process. Their
individual median [p05–p95] ns per marked return were:

| Batch | Worker 0 | Worker 1 | Worker 2 | Worker 3 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 527.352 [502.960–685.249] | 527.442 [504.036–682.628] | 527.716 [504.573–710.141] | 529.028 [507.452–695.965] |
| 2 | 443.085 [387.461–469.410] | 442.765 [387.871–464.429] | 443.716 [391.188–475.992] | 441.017 [388.470–471.536] |
| 3 | 416.965 [375.621–450.041] | 415.983 [375.122–443.354] | 416.806 [378.241–450.217] | 414.903 [375.748–448.751] |
| 4 | 533.705 [496.002–603.811] | 534.921 [496.202–596.418] | 533.270 [496.846–595.962] | 530.092 [495.136–573.818] |
| 5 | 530.348 [504.992–564.755] | 524.582 [500.441–547.872] | 527.357 [506.893–582.582] | 529.852 [505.910–568.273] |
| 6 | 424.289 [376.716–503.115] | 422.836 [367.530–507.049] | 413.989 [368.848–519.470] | 421.918 [377.521–514.513] |
| 7 | 424.197 [367.289–480.767] | 419.328 [367.166–485.202] | 421.224 [366.655–480.198] | 423.317 [365.966–482.520] |
| 8 | 538.893 [520.772–579.977] | 537.013 [513.630–580.742] | 536.398 [517.082–580.970] | 537.257 [514.662–583.661] |

For each ABBA cycle, compare the mean of the two A batch medians with the
mean of the two B batch medians. The A batches bracket B in time.

| Cycle | Mean A batch median, ns | Mean B batch median, ns | Recovery from separation, ns |
| ---: | ---: | ---: | ---: |
| 1 | 535.086665 | 432.461465 | 102.625200 |
| 2 | 543.109585 | 426.296250 | 116.813335 |

Pooled A was 538.753 [507.452–603.811] ns and pooled B was
431.184 [376.168–489.326] ns, with 124 measured samples per mode.
Both cycles supported separation beyond the approximately 4 ns one-worker
control variation in the first experiment. Neither cycle reversed direction.
Distribution overlap, OS scheduling, and possible core migration remain limits;
this was not a statistical claim about every future run or every processor.

The frozen binary's recording function has one mode-independent nonzero check,
index calculation/bounds check, and ordinary AArch64 `ldadd`. Mode selection
is absent from that function. Disassembly, source and binary hashes, raw output,
address/count validation, per-worker distributions, and both cycle contrasts
remain under `.context/fuel-settlement-resume/layout-followup/`.
The raw-log analysis independently reproduced both contrasts. The source review
identified and corrected the 32-versus-four destination reuse confound before
the only timed follow-up. No repeat or favorable subset was selected.

### Supported conclusion and selected correction

Observed: concurrent settlement cost depends on destination sharing in this
workload. Distinct words helped relative to a shared word in both initial
blocks. With four persistent workers and equal destination reuse, separating
those words into different lines reduced cost in both ABBA cycles. This supports
avoiding both shared settlement destinations and avoidable inter-writer line
sharing. It does not identify an undocumented coherence protocol, partition the
original 159.70 ns regression into separate causes, or measure an isolated
atomic instruction. Custom hardware-counter investigation remains concluded.

The selected production correction retains one Host-owned `FuelMetrics`
allocation and adds exactly 32 `CachePadded<[AtomicU64; 2]>` settlement shards.
The existing six decision counters remain aggregate counters in their own
`CachePadded` bank. A separately padded `AtomicUsize` selects writers.
`Proc::new`, called by `Builder::build`, selects a writer once per Store using
`next_writer.fetch_add(1, Relaxed) % 32`. Selection occurs outside settlement
and outside the existing benchmark's timed `Proc::run` interval. The benchmark
therefore does not measure Store construction or writer-assignment overhead.

A nonzero settlement performs one ordinary relaxed atomic addition to the
Store's retained shard and policy index. No lock, allocation, thread-ID lookup,
registration map, or per-boundary writer selection is added. Writer clones and
moves across threads retain the same shard. Multiple Stores can collide on a
shard; atomics preserve exact accounting, while collisions can reduce the
performance benefit. The 32 slots bound memory; they are not exclusive worker
ownership and do not promise collision-free placement for arbitrary lifetimes.
The setup sequence can wrap without producing an out-of-bounds index.

HTTP snapshots sum every retained shard for each policy with `wrapping_add`.
No shard is cleared when a Cell or worker terminates; the admin's shared owner
retains all totals without a drop-time flush. An independent Host starts with
an independent zeroed allocation. Relaxed loads are not a transactional view
across shards or metric families. Totals are exact after writers quiesce, and
repeated reads by one reader are monotonic before overflow. Atomic increments
and aggregation retain modulo-2^64 counter arithmetic; overflow can break
monotonicity. This counter arithmetic does not change saturating authority
ledger debits. No shard identifier appears in Prometheus labels.

`crossbeam_utils::CachePadded` supplies target-dependent alignment estimates;
the production design has no Apple-specific counter APIs or instructions.
The lockfile adds only `cell`'s direct dependency on the already locked
`crossbeam-utils` 0.8.21; no resolved package version changes.
Counting correctness and lifetime do not depend on those estimates matching
a CPU's cache geometry. A different geometry can still affect performance.
Layout tests use `CachePadded` alignment rather than assuming 128-byte lines.
The selected design must pass fresh correctness and performance validation;
these diagnostics establish neither production acceptance nor Linux performance.

## Corrected-candidate validation: stopped before performance acceptance

The focused macOS collector run passed all five tests. The subsequent release
workspace invocation recorded 1,229 passes, one failure, and 23 ignored tests
before Cargo stopped. Counts use the final result of each completed executable
and exclude nested subprocess probes. This is not a complete workspace pass.
The full metrics HTTP executable passed seven tests, with two expected opt-in
entries ignored. The PID0 executable passed 18 of 19 tests. Its real
PID0/status-to-HTTP test observed 930,999 scheduled settled units after status
execution, zero before boot, and unchanged zero one-shot units.

The failing test was
`superseded_pending_generation_converges_to_the_newer_epoch` at
`tests/pid0_e2e.rs:2640`: `host did not observe the superseding epoch: Elapsed(())`.
The validation command used `cargo test --workspace --release --locked` and
explicitly selected `target/release/ww`. That profile was incorrect for this
test. The test always enables `WW_TEST_PID0_RESULT_RACE` and waits for a TCP
barrier connection. `src/deployment/mod.rs` compiles the barrier functions into
empty functions when `debug_assertions` is disabled. These gates already exist
unchanged in the baseline. CI runs this suite without `--release`.

Independent source and binary inspection confirmed the profile mismatch.
The timeout establishes missing test-hook signaling, not failed runtime epoch
convergence. Whether that lifecycle test passes with the matching debug Host
remains unvalidated. No production lifecycle change or automatic test rerun was
made. The requested unexpected-validation-failure stop condition halted the
remaining checks, acceptance benchmarks, and publication.

Evidence is under `.context/fuel-settlement-resume/validation-20261028/`:
`collector-focused-retry.log`, `corrected-workspace.log`,
`workspace-partial-summary.json`, `candidate-source-inputs.json`, and
`profile-mismatch.md`. The original failed runner remains intact.
`run-correctness-ci.py` prepares the profile correction and selects
`target/debug/ww`; that runner has not executed. Private Kubo peers and Linux
validation containers were stopped after collecting evidence.

The separate macOS native-P3 invocation, strict Clippy, remaining workspace
targets, and final acceptance remain incomplete. Source review found no blocking
collector defect, but that review does not replace the missing validation.
Workspace formatting and `git diff --check` passed during local preservation.
No corrected-candidate remote CI, PR, push, or merge occurred.

Final baseline/candidate performance results are absent. The baseline release
binaries were rebuilt and preserved; the candidate workspace binaries were
built for correctness checks. A future acceptance run must build the candidate
with the same target selection as the baseline, then run the original three
alternating pairs. At most one justified unchanged-code confirmation block
remains available. Preserve every exception and the original Apple budgets.
The candidate's absolute remaining overhead is unknown; diagnostic recovery
must not be substituted for this final comparison.

### Linux functional validation and performance limits

### Linux functional validation

On 2026-10-28, selected debug test targets compiled and executed for
`aarch64-unknown-linux-gnu` in an OrbStack Linux ARM64 VM on the Apple host.
The environment used Debian 12, Rust/Cargo 1.96.1, LLVM 22.1.2, GCC 12.2.0,
and Cap'n Proto 1.1.0. Linux Rust 1.96.1 differs from macOS Rust 1.96.0;
no performance comparison between those toolchains is claimed.

All five functional command groups passed:

| Test selector | Passed | Failed | Ignored |
| --- | ---: | ---: | ---: |
| `-p cell --lib fuel` | 58 | 0 | 0 |
| `-p cell --lib metrics::tests::` | 5 | 0 | 0 |
| `-p cell --lib start_function_` | 2 | 0 | 0 |
| `-p cell --lib p3::tests::p3_ -- --ignored` | 6 | 0 | 0 |
| `--test metrics_http` | 7 | 0 | 2 |

These groups executed 78 top-level tests. Three isolated HTTP subprocess probes
also passed, giving 81 passing invocations. The two ignored HTTP entries are
the opt-in scrape measurement and subprocess-only probe; the six normally
ignored native P3 tests were explicitly executed. The expected executor panic
probe passed its enclosing HTTP test and was not an unexpected failure.

Collector tests covered padded separation, concurrent and colliding writers,
monotonic snapshots, clone/thread migration, assignment wrap, and counter wrap.
Fuel and HTTP tests covered settlement, authority accounting, real P3 execution,
policy attribution, runtime child propagation, teardown retention, and existing
exported metrics. The run did not include a release build, Kubo-dependent
Host/PID0 integration, the full workspace suite, or Linux performance workloads.
Both dedicated containers stopped after validation.

Evidence is retained in
`.context/fuel-settlement-resume/linux-20261028/report.md` and `functional.log`.
The source manifest records 474 copied files, including six architecture-independent
Wasm fixtures; all source and fixture hash checks passed. The snapshot includes
the uncommitted correction on `937d74ece14c6e957cddee7af596eb3aa0142860`.

Linux x86-64 functional validation awaits the existing PR CI, which executes
the host workspace. No dedicated representative Linux x86-64 or ARM64
performance host is currently established. OrbStack validates Linux ARM64
functionality on the Apple CPU; it does not establish independent Linux
performance acceptance. Neither cross-compilation nor emulated x86-64 execution
would establish that performance claim.

For a native Linux performance host, prepare isolated baseline and candidate
checkouts with the same Rust compiler, resolved dependency versions, release profile, and unchanged
benchmark harness. Capture kernel, CPU/cache metadata, cgroup CPU allocation,
virtualization, and source hashes. Complete both builds before timing. Run the
release commands in Evidence and reproduction above in alternating order for
three pairs, retaining each checkout's HTTP test and all raw outputs. Analyze
with `scripts/analyze_fuel_overhead.py` using architecture-local pairs; report
relative overhead and absolute deltas. State Linux criteria before timing;
do not transplant Apple's nanosecond thresholds without justification.
The detailed local procedure is
`.context/fuel-settlement-resume/linux-validation-plan.md`.
