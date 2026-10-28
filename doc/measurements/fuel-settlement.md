# Settled fuel instrumentation measurements

Baseline: `cdf474b4cce0ed99f341d01fd6c3818f64a11d04` (#769).
The baseline includes applied fuel-decision instrumentation. These measurements
compare the additional settlement counter using the same committed release
profile, resolved dependency versions, Rust toolchain, machine, and
`tests/fuel_overhead.rs` harness.

Current status, 2026-10-28: software diagnostics support separating concurrent
settlement destinations. The selected correction uses 32 bounded, padded atomic
shards. Corrected macOS CI-profile validation passed, including the full workspace,
the previously failing PID0 test, focused tests, and strict Clippy. The earlier
release-profile failure remains recorded below. Final acceptance stopped:
the corrected candidate adds 56.533955 ns per four-worker marked return in the
six-pair pool, above the 25 ns quality target. No further runs or merge follow.
Linux ARM64 functional validation passed. Linux x86-64 CI was not run because
publication is blocked. All earlier measurements remain historical evidence.

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

For the resumed correction, the user set a four-worker quality target of at
most **25 ns added median time per marked host-return boundary**. The historical
100 ns rejection threshold and all results measured against it remain unchanged.
A material residual above 25 ns requires investigation and reporting before
merge, even when the result remains below 100 ns. The final results below miss this target.

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

Historical stage snapshot: the following account records the earlier stopped
run. The subsequent CI-profile run is recorded in the next section.

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

## Corrected CI-profile validation: passed — 2026-10-28

The resumed run built and launched `target/debug/ww`, matching the correctness
profile in `.github/workflows/rust.yml`. `cargo test --workspace --locked --
--test-threads=2 --nocapture` passed 1,335 tests across 42 executables, with
23 ignored. Doc-tests added two passes and one ignored test. The workspace
total is **1,337 passed, zero failed, 24 ignored**. Counts use each executable's
final result and exclude three passing HTTP subprocess probes.

All 19 PID0 tests passed, including
`superseded_pending_generation_converges_to_the_newer_epoch`. The real
PID0/status-to-HTTP test observed 981,512 scheduled settled units after status
execution, zero before boot, 26 installed scheduled grants, and zero one-shot
settled units. The HTTP executable passed seven tests with two opt-in entries
ignored. No prerequisite-skip diagnostic appeared in the workspace log.

Separate receipts record 58 fuel tests, five collector tests, and six explicitly
enabled native-P3 tests passing. The fuel and collector runs repeat workspace
coverage; the native-P3 cases were ignored in the workspace invocation.
Strict `cargo clippy --workspace --all-targets --locked -- -D warnings`, both
repository lockfile checks, workspace and fixture formatting checks, and
`git diff --check` passed. These local checks do not cover every CI job.

All 231 source-input hashes match the corrected candidate manifest. Relative
to the earlier manifest, only `fuzz/Cargo.lock` changed: `cell` gained its direct
`crossbeam-utils` dependency entry. No resolved package version changed; the
fuzz workspace retains 0.8.23 and the host workspace retains 0.8.21. The first
CI-profile attempt stopped at that stale lockfile before building. The next
attempt used a fresh cache and verified both isolated Kubo identities and peers.

Evidence is under
`.context/fuel-settlement-resume/validation-ci-20261028/attempt-2/`:
`report.md`, `progress.jsonl`, all command logs, `source-inputs.json`,
`debug-host.json`, and `kubo-preflight.json`. The run used Rust 1.96.0 and
LLVM 22.1.2 on `aarch64-apple-darwin`. The debug Host SHA-256 is
`4be2d7e7f962e11ff37e40908320d9d09e6c4dfe65153dcd48ab75a9c2409be4`.

The subsequent final comparison below missed the four-worker quality target.
Correctness remains validated; the candidate is not accepted for publication.

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

Linux x86-64 functional validation was not run: publication stopped before
PR CI, which executes the host workspace. No dedicated representative Linux x86-64 or ARM64
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

## Final corrected-candidate acceptance: stopped — 2026-10-28

The six-pair four-worker marked-return pool adds **56.533955 ns per boundary
(+14.158147%)**, exceeding the prospective 25 ns quality target. Four of six
pairs miss that target. Pair 4 also exceeds the historical 100 ns threshold.
Both three-pair blocks miss the quality target when pooled. The single permitted
unchanged-code confirmation is exhausted. No further acceptance runs, push,
PR, remote CI, or merge follow this result.

### Build identity and complete sample accounting

The detached baseline is `cdf474b4cce0ed99f341d01fd6c3818f64a11d04`; the detached
candidate is `b1a21b9e590e41ea441873c51ea5742b96df3cd6`. The collector and other
runtime code are unchanged from `1941b6f64dc8d80692994d1bec0f87635e2b7c83`.
The only subsequent source-input correction adds the existing
`crossbeam-utils` dependency edge to `fuzz/Cargo.lock`. No package version
changed. Both host lockfiles resolve the same 787 package identities; the
intentional direct `cell` dependency edge differs.

Both builds used `cargo test --release --locked --test fuel_overhead
--test metrics_http --no-run`, Rust 1.96.0, LLVM 22.1.2, and the repository's
unchanged release profile on `aarch64-apple-darwin`. The machine was an Apple
M5 Max with 18 logical CPUs and 128 GiB RAM, running Darwin 25.6.0.
The operating system reported a 128-byte cache line. No thermal or performance
warning was recorded before either block. These observations do not establish
that desktop activity was absent or explain the measured variation.

All 31 frozen source, fixture, protocol, analyzer, runner, and binary hashes
still match. Kernel and status Wasm bytes match between builds. The fuel
harness SHA-256 is
`3858b61cffc9bdfb8c0d4675e6fa8a5893915d8427416f2113773c5bf078f6b8`
on both sides. The analyzer SHA-256 is
`65e4dd9a5df181aefc50b4a128c2b723e7bbc9f05e203c6d484fed90ccfefcae`.
HTTP contract assertions differ as required by the new metric; the measured
HTTP functions and workloads remain byte-identical.

| Retained executable | SHA-256 |
| --- | --- |
| `baseline-fuel_overhead` | `8446a7b90d3a281a6c484af3481f0c3e7ff89c13197eeaa70a3bb0ebea127481` |
| `baseline-metrics_http` | `6754f715c426e5d966146f71ed933ec34a27c291ea30abcd7e14c9e5020a3209` |
| `instrumented-fuel_overhead` | `e07de86608fe50386f88cc80bcaabf82414ad5424e29153aebb8bdd92b913ad6` |
| `instrumented-metrics_http` | `72ad5703da62b37e8ccc5a08f670a8758d8126e5fdbd9911237b22313dcfcfc3` |

The initial pair order was baseline/candidate, candidate/baseline,
baseline/candidate. Confirmation pairs 4–6 used candidate/baseline,
baseline/candidate, candidate/baseline. The confirmation reason was recorded
before pair 4: sign-changing marked-return deltas, a pooled quality-target
miss, compute-tail exceptions, and one HTTP exception required the permitted
repeatability check. Source, binaries, workload, normalization, and budgets
remained unchanged. No PMU operations or timed `FuelObserver` ran.

Each of the 12 logs contains all 248 fuel observations: four cases, one and
four workers, and 31 samples after three predefined warmups. Each log also
contains six HTTP mode/phase records with 200 scrapes each. All 24 benchmark
commands exited successfully. Every untimed one-shot preflight observed five
suspensions, above the required two. Coverage receipts retain these checks.
All medians and nearest-rank p05/p95 values below were independently reproduced
from the raw logs without filtering measured observations.

For fuel, elapsed time is the slowest worker's duration divided by that
worker's operation count. Marked returns therefore use elapsed/100,000 at
both worker counts, not aggregate throughput. Store preparation and writer
assignment remain outside timing. Pools contain 93 samples per case/worker
for each block and 186 across six pairs. HTTP pools contain 600 scrapes per
mode/phase per block and 1,200 across six pairs, per build.

### Marked-return decision and exceptions

| Pair/block | Order | Four-worker added median ns | Classification |
| --- | --- | ---: | --- |
| 1 | B/C | -28.965830 | Quality target met |
| 2 | C/B | +68.288340 | Quality target missed |
| 3 | B/C | +45.915010 | Quality target missed |
| 4 | C/B | +100.917500 | Quality target missed; historical threshold exceeded |
| 5 | B/C | -20.307920 | Quality target met |
| 6 | C/B | +82.833740 | Quality target missed |
| Initial 1–3 | Pooled | +46.868330 | Quality target missed |
| Confirmation 4–6 | Pooled | +59.360420 | Quality target missed |
| All 1–6 | Pooled | +56.533955 | Quality target missed |

The single-worker six-pair pool adds 2.223960 ns per marked return. All six
single-worker medians stay below the 25 ns single-worker limit. The remaining
four-worker cost is therefore material to the requested quality target.
Earlier destination diagnostics support reducing address sharing; they do not
partition this residual cost among atomic instructions, cache/coherence effects,
scheduling, or other hardware mechanisms. The sign changes do not justify
attributing unfavorable results to desktop noise or discarding those samples.

Four-worker compute exceeds its p95 limit in pair 1 (+9.866863%) and pair 3
(+24.514886%). Pair 3 also exceeds the median limit (+4.676554%). The initial
pool has a +9.828186% p95 change. The confirmation pool has a +0.477752% p95
change, but the complete six-pair pool remains above the 5% limit at
+7.018622%. No other compute or timer-I/O per-pair median/p95 limit is exceeded.

HTTP p95 exceeds its 20 microsecond allowance in pair 2, fallback after
compilation (+24.500 microseconds), and pair 5, disabled before compilation
(+24.666 microseconds). Neither exception is omitted when pooled HTTP results
fall below the limit. Every measured response grew from 2,266 to 2,564 bytes:
**298 added bytes**, below the 512-byte cap in all 36 comparisons.

### Complete per-pair fuel distributions

Values are median [p05, p95] in ns per worker operation. Added median is in
ns; percentage columns compare the corresponding baseline statistic. For
`oneshot_suspend_resume`, each worker executes 2,000 operations; multiply a
reported value by 0.002 to obtain the worker completion time in milliseconds.
The periodic 10 ms waits dominate that case and do not isolate atomic cost.

| Pair | Case | Workers | Baseline median [p05, p95] | Candidate median [p05, p95] | Added median ns | Median change % | p95 change % |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | `oneshot_suspend_resume` | 1 | 30401.145500 [27657.645500, 31114.208500] | 30415.291500 [28702.645500, 31100.562500] | +14.146000 | +0.046531 | -0.043858 |
| 1 | `oneshot_suspend_resume` | 4 | 29673.000000 [26377.937500, 30973.333500] | 29948.396000 [27297.937500, 30947.541500] | +275.396000 | +0.928103 | -0.083272 |
| 1 | `scheduled_compute` | 1 | 7.241969 [6.980375, 7.408542] | 6.770417 [6.595313, 7.310812] | -0.471552 | -6.511379 | -1.319143 |
| 1 | `scheduled_compute` | 4 | 7.787177 [7.694990, 7.994458] | 7.871781 [7.555833, 8.783261] | +0.084604 | +1.086456 | +9.866863 |
| 1 | `scheduled_host_boundaries` | 1 | 240.842920 [233.777910, 252.875420] | 253.172500 [245.450000, 264.842090] | +12.329580 | +5.119345 | +4.732239 |
| 1 | `scheduled_host_boundaries` | 4 | 448.512500 [331.951250, 559.314580] | 419.546670 [366.340830, 542.467080] | -28.965830 | -6.458199 | -3.012169 |
| 1 | `scheduled_io` | 1 | 1147641.937500 [1133617.187500, 1156563.812500] | 1136119.781250 [1103552.093750, 1145097.656250] | -11522.156250 | -1.003985 | -0.991398 |
| 1 | `scheduled_io` | 4 | 1147736.968750 [1127140.625000, 1164092.437500] | 1134608.093750 [1123337.250000, 1139897.125000] | -13128.875000 | -1.143892 | -2.078470 |
| 2 | `oneshot_suspend_resume` | 1 | 30078.291500 [28613.041500, 31094.396000] | 30117.041500 [28520.479000, 31109.729000] | +38.750000 | +0.128830 | +0.049311 |
| 2 | `oneshot_suspend_resume` | 4 | 29470.625000 [27604.792000, 30919.520500] | 29534.854000 [27952.729000, 30936.583500] | +64.229000 | +0.217942 | +0.055185 |
| 2 | `scheduled_compute` | 1 | 6.837167 [6.557136, 7.726813] | 6.911375 [6.563937, 7.451688] | +0.074208 | +1.085366 | -3.560653 |
| 2 | `scheduled_compute` | 4 | 7.818260 [7.420875, 8.388208] | 7.674750 [7.389354, 8.380250] | -0.143510 | -1.835578 | -0.094874 |
| 2 | `scheduled_host_boundaries` | 1 | 248.515000 [242.568330, 254.576670] | 254.567500 [247.783330, 278.237920] | +6.052500 | +2.435467 | +9.294351 |
| 2 | `scheduled_host_boundaries` | 4 | 416.391660 [339.798750, 567.102080] | 484.680000 [379.937910, 578.908750] | +68.288340 | +16.400026 | +2.081930 |
| 2 | `scheduled_io` | 1 | 1135509.093750 [1100145.843750, 1142575.531250] | 1136845.031250 [1101265.625000, 1143848.968750] | +1335.937500 | +0.117651 | +0.111453 |
| 2 | `scheduled_io` | 4 | 1136951.812500 [1126501.312500, 1142714.843750] | 1134272.125000 [1127072.906250, 1141300.781250] | -2679.687500 | -0.235691 | -0.123746 |
| 3 | `oneshot_suspend_resume` | 1 | 30018.896000 [27919.167000, 31120.312500] | 29869.979000 [27857.104500, 31117.979500] | -148.917000 | -0.496078 | -0.007497 |
| 3 | `oneshot_suspend_resume` | 4 | 29710.104500 [28049.166500, 30802.833000] | 29744.104500 [27441.812500, 30945.729000] | +34.000000 | +0.114439 | +0.463905 |
| 3 | `scheduled_compute` | 1 | 7.173188 [6.700708, 7.615448] | 7.266875 [7.028646, 7.422552] | +0.093687 | +1.306079 | -2.532953 |
| 3 | `scheduled_compute` | 4 | 7.830729 [7.741708, 8.147729] | 8.196938 [7.658156, 10.145136] | +0.366208 | +4.676554 | +24.514886 |
| 3 | `scheduled_host_boundaries` | 1 | 250.733340 [242.976250, 258.606670] | 249.227500 [242.311250, 259.345420] | -1.505840 | -0.600574 | +0.285665 |
| 3 | `scheduled_host_boundaries` | 4 | 449.172080 [372.487910, 571.270420] | 495.087090 [382.432080, 558.505000] | +45.915010 | +10.222142 | -2.234567 |
| 3 | `scheduled_io` | 1 | 1142723.968750 [1130571.593750, 1154055.968750] | 1141287.781250 [1106610.656250, 1152059.906250] | -1436.187500 | -0.125681 | -0.172961 |
| 3 | `scheduled_io` | 4 | 1142468.750000 [1129511.718750, 1232936.187500] | 1141307.281250 [1129026.031250, 1157101.562500] | -1161.468750 | -0.101663 | -6.150734 |
| 4 | `oneshot_suspend_resume` | 1 | 30077.646000 [27933.583500, 31045.333500] | 29885.021000 [28077.687500, 31081.458500] | -192.625000 | -0.640426 | +0.116362 |
| 4 | `oneshot_suspend_resume` | 4 | 29709.333500 [27428.062500, 30943.812500] | 29358.625000 [27427.270500, 30910.896000] | -350.708500 | -1.180466 | -0.106375 |
| 4 | `scheduled_compute` | 1 | 7.226166 [7.050625, 7.821021] | 7.198375 [6.948948, 7.361115] | -0.027791 | -0.384595 | -5.880386 |
| 4 | `scheduled_compute` | 4 | 7.783000 [7.686510, 7.979500] | 7.782177 [7.726813, 7.916354] | -0.000823 | -0.010574 | -0.791350 |
| 4 | `scheduled_host_boundaries` | 1 | 242.485000 [236.065410, 254.570000] | 240.060410 [233.879580, 253.148330] | -2.424590 | -0.999893 | -0.558459 |
| 4 | `scheduled_host_boundaries` | 4 | 362.591250 [335.049580, 450.931250] | 463.508750 [370.843750, 540.287500] | +100.917500 | +27.832304 | +19.815936 |
| 4 | `scheduled_io` | 1 | 1142294.281250 [1121248.687500, 1154468.750000] | 1145779.937500 [1101674.500000, 1154104.156250] | +3485.656250 | +0.305145 | -0.031581 |
| 4 | `scheduled_io` | 4 | 1150493.500000 [1124226.562500, 1157455.718750] | 1148800.781250 [1130158.843750, 1161348.937500] | -1692.718750 | -0.147130 | +0.336360 |
| 5 | `oneshot_suspend_resume` | 1 | 29425.020500 [27752.500000, 31079.083500] | 29728.208000 [27086.750000, 31088.666500] | +303.187500 | +1.030373 | +0.030834 |
| 5 | `oneshot_suspend_resume` | 4 | 30106.437500 [28476.271000, 30940.104000] | 29884.291500 [28486.521000, 30924.583500] | -222.146000 | -0.737869 | -0.050163 |
| 5 | `scheduled_compute` | 1 | 7.226969 [6.952031, 7.381417] | 7.197739 [6.972594, 7.677417] | -0.029229 | -0.404447 | +4.010070 |
| 5 | `scheduled_compute` | 4 | 7.784906 [7.737563, 7.999156] | 7.813490 [7.732875, 8.031344] | +0.028583 | +0.367166 | +0.402386 |
| 5 | `scheduled_host_boundaries` | 1 | 239.220420 [234.987910, 249.545000] | 239.011670 [233.207910, 250.209170] | -0.208750 | -0.087263 | +0.266152 |
| 5 | `scheduled_host_boundaries` | 4 | 435.746250 [386.440840, 498.709160] | 415.438330 [361.407910, 451.583330] | -20.307920 | -4.660492 | -9.449562 |
| 5 | `scheduled_io` | 1 | 1148126.312500 [1115253.906250, 1155286.468750] | 1143683.593750 [1116231.781250, 1155954.406250] | -4442.718750 | -0.386954 | +0.057816 |
| 5 | `scheduled_io` | 4 | 1150798.156250 [1126089.843750, 1161040.375000] | 1147433.593750 [1132082.031250, 1161050.781250] | -3364.562500 | -0.292368 | +0.000896 |
| 6 | `oneshot_suspend_resume` | 1 | 30164.020500 [28017.916500, 31095.145500] | 29752.458500 [28076.145500, 31099.875000] | -411.562000 | -1.364414 | +0.015210 |
| 6 | `oneshot_suspend_resume` | 4 | 29729.521000 [28277.583500, 30911.854000] | 29792.667000 [27137.833000, 30912.375000] | +63.146000 | +0.212402 | +0.001685 |
| 6 | `scheduled_compute` | 1 | 7.232740 [7.048021, 7.534948] | 7.226063 [6.995271, 7.428094] | -0.006677 | -0.092316 | -1.418115 |
| 6 | `scheduled_compute` | 4 | 7.786083 [7.714000, 7.993156] | 7.786625 [7.710615, 8.038094] | +0.000542 | +0.006958 | +0.562200 |
| 6 | `scheduled_host_boundaries` | 1 | 240.013330 [234.343330, 251.180000] | 238.559590 [232.087080, 252.182920] | -1.453740 | -0.605691 | +0.399283 |
| 6 | `scheduled_host_boundaries` | 4 | 360.325420 [339.055830, 438.393340] | 443.159160 [380.523330, 527.034170] | +82.833740 | +22.988592 | +20.219475 |
| 6 | `scheduled_io` | 1 | 1147812.500000 [1118076.812500, 1156181.000000] | 1149369.781250 [1101050.781250, 1156976.562500] | +1557.281250 | +0.135674 | +0.068810 |
| 6 | `scheduled_io` | 4 | 1148138.031250 [1131579.437500, 1158550.781250] | 1151714.843750 [1127776.031250, 1159725.281250] | +3576.812500 | +0.311532 | +0.101377 |

### Pooled fuel distributions, including one-shot latency

Units and normalization match the per-pair table. Initial and confirmation
pools remain separate; the complete pool retains both blocks.

| Block | Case | Workers | Baseline median [p05, p95] | Candidate median [p05, p95] | Added median ns | Median change % | p95 change % |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Initial 1–3 | `oneshot_suspend_resume` | 1 | 30052.979000 [27919.167000, 31114.208500] | 30117.041500 [28006.687500, 31110.791500] | +64.062500 | +0.213165 | -0.010982 |
| Initial 1–3 | `oneshot_suspend_resume` | 4 | 29672.896000 [27604.792000, 30948.187500] | 29764.187500 [27441.812500, 30945.833500] | +91.291500 | +0.307660 | -0.007606 |
| Initial 1–3 | `scheduled_compute` | 1 | 7.113437 [6.605614, 7.615448] | 6.993167 [6.597000, 7.416760] | -0.120271 | -1.690758 | -2.609006 |
| Initial 1–3 | `scheduled_compute` | 4 | 7.820927 [7.526396, 8.364229] | 7.871781 [7.412687, 9.186281] | +0.050854 | +0.650230 | +9.828186 |
| Initial 1–3 | `scheduled_host_boundaries` | 1 | 247.395420 [235.492090, 256.721670] | 252.755830 [243.844160, 266.200000] | +5.360410 | +2.166738 | +3.692065 |
| Initial 1–3 | `scheduled_host_boundaries` | 4 | 428.949590 [338.471250, 561.227090] | 475.817920 [373.655840, 558.505000] | +46.868330 | +10.926303 | -0.485025 |
| Initial 1–3 | `scheduled_io` | 1 | 1138868.468750 [1123513.031250, 1155649.750000] | 1137087.250000 [1103552.093750, 1151472.656250] | -1781.218750 | -0.156402 | -0.361450 |
| Initial 1–3 | `scheduled_io` | 4 | 1139565.093750 [1127859.375000, 1164527.343750] | 1136612.000000 [1126869.781250, 1154016.937500] | -2953.093750 | -0.259142 | -0.902547 |
| Confirmation 4–6 | `oneshot_suspend_resume` | 1 | 29884.396000 [27933.583500, 31079.083500] | 29826.562500 [27912.792000, 31088.666500] | -57.833500 | -0.193524 | +0.030834 |
| Confirmation 4–6 | `oneshot_suspend_resume` | 4 | 29839.500000 [28071.104000, 30936.000000] | 29758.833000 [27461.062500, 30920.604000] | -80.667000 | -0.270336 | -0.049767 |
| Confirmation 4–6 | `scheduled_compute` | 1 | 7.226969 [6.985646, 7.425021] | 7.199729 [6.972594, 7.416854] | -0.027240 | -0.376915 | -0.109990 |
| Confirmation 4–6 | `scheduled_compute` | 4 | 7.784906 [7.713375, 7.993156] | 7.798615 [7.717458, 8.031344] | +0.013708 | +0.176091 | +0.477752 |
| Confirmation 4–6 | `scheduled_host_boundaries` | 1 | 240.013330 [234.987910, 252.890830] | 239.318330 [233.404580, 252.182920] | -0.695000 | -0.289567 | -0.279927 |
| Confirmation 4–6 | `scheduled_host_boundaries` | 4 | 378.436250 [338.306250, 482.304170] | 437.796670 [372.305420, 522.142500] | +59.360420 | +15.685712 | +8.260001 |
| Confirmation 4–6 | `scheduled_io` | 1 | 1144506.500000 [1118076.812500, 1155343.750000] | 1146298.187500 [1102510.406250, 1155072.906250] | +1791.687500 | +0.156547 | -0.023443 |
| Confirmation 4–6 | `scheduled_io` | 4 | 1150373.687500 [1127050.781250, 1159347.656250] | 1148240.875000 [1130158.843750, 1160545.593750] | -2132.812500 | -0.185402 | +0.103329 |
| All 1–6 | `oneshot_suspend_resume` | 1 | 30029.698000 [27933.583500, 31093.020500] | 29946.666750 [28006.687500, 31099.875000] | -83.031250 | -0.276497 | +0.022045 |
| All 1–6 | `oneshot_suspend_resume` | 4 | 29728.489750 [27993.104000, 30936.000000] | 29761.510250 [27461.062500, 30928.583500] | +33.020500 | +0.111074 | -0.023974 |
| All 1–6 | `scheduled_compute` | 1 | 7.197760 [6.650521, 7.425021] | 7.157703 [6.647052, 7.416760] | -0.040057 | -0.556526 | -0.111252 |
| All 1–6 | `scheduled_compute` | 4 | 7.804245 [7.671771, 8.025229] | 7.815031 [7.494344, 8.588490] | +0.010787 | +0.138215 | +7.018622 |
| All 1–6 | `scheduled_host_boundaries` | 1 | 244.462500 [235.263750, 254.570000] | 246.686460 [233.879580, 262.287920] | +2.223960 | +0.909735 | +3.031748 |
| All 1–6 | `scheduled_host_boundaries` | 4 | 399.303335 [338.471250, 528.053330] | 455.837290 [373.655840, 549.720000] | +56.533955 | +14.158147 | +4.103122 |
| All 1–6 | `scheduled_io` | 1 | 1142434.906250 [1119832.031250, 1155486.968750] | 1139757.812500 [1103552.093750, 1154328.125000] | -2677.093750 | -0.234332 | -0.100291 |
| All 1–6 | `scheduled_io` | 4 | 1142966.140625 [1127859.375000, 1160777.343750] | 1138718.750000 [1127799.468750, 1159725.281250] | -4247.390625 | -0.371611 | -0.090634 |

### Complete per-pair HTTP distributions

Values are median [p05, p95] in microseconds. Every row adds 298 body bytes.

| Pair | Cache mode/phase | Baseline median [p05, p95] | Candidate median [p05, p95] | Added median us | Added p95 us |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | `disabled:after_compile` | 41.395500 [36.583000, 47.292000] | 39.875000 [37.292000, 45.416000] | -1.520500 | -1.876000 |
| 1 | `disabled:before_compile` | 71.083000 [41.125000, 83.333000] | 41.125000 [38.916000, 47.416000] | -29.958000 | -35.917000 |
| 1 | `enabled:after_compile` | 72.229000 [63.416000, 80.875000] | 68.229500 [45.500000, 79.542000] | -3.999500 | -1.333000 |
| 1 | `enabled:before_compile` | 80.916500 [70.791000, 96.416000] | 81.166500 [74.875000, 102.750000] | +0.250000 | +6.334000 |
| 1 | `fallback:after_compile` | 44.729500 [38.083000, 82.375000] | 40.354500 [37.125000, 46.709000] | -4.375000 | -35.666000 |
| 1 | `fallback:before_compile` | 41.729000 [37.667000, 55.583000] | 41.875000 [38.417000, 53.833000] | +0.146000 | -1.750000 |
| 2 | `disabled:after_compile` | 40.145500 [36.666000, 48.750000] | 42.208500 [38.542000, 60.875000] | +2.063000 | +12.125000 |
| 2 | `disabled:before_compile` | 42.145500 [38.125000, 55.791000] | 41.791500 [38.416000, 54.792000] | -0.354000 | -0.999000 |
| 2 | `enabled:after_compile` | 71.208500 [39.417000, 104.833000] | 73.645500 [39.959000, 84.625000] | +2.437000 | -20.208000 |
| 2 | `enabled:before_compile` | 80.875000 [64.708000, 108.459000] | 88.604000 [76.958000, 99.625000] | +7.729000 | -8.834000 |
| 2 | `fallback:after_compile` | 43.104000 [39.291000, 57.708000] | 45.500000 [38.708000, 82.208000] | +2.396000 | +24.500000 |
| 2 | `fallback:before_compile` | 42.459000 [39.833000, 57.291000] | 45.374500 [39.542000, 68.000000] | +2.915500 | +10.709000 |
| 3 | `disabled:after_compile` | 42.708500 [38.583000, 53.459000] | 42.729000 [39.417000, 54.000000] | +0.020500 | +0.541000 |
| 3 | `disabled:before_compile` | 46.312500 [40.084000, 63.792000] | 47.000000 [39.500000, 67.709000] | +0.687500 | +3.917000 |
| 3 | `enabled:after_compile` | 51.770500 [41.000000, 89.917000] | 62.625000 [43.708000, 93.416000] | +10.854500 | +3.499000 |
| 3 | `enabled:before_compile` | 87.750000 [79.792000, 102.833000] | 95.833500 [84.500000, 117.333000] | +8.083500 | +14.500000 |
| 3 | `fallback:after_compile` | 46.625500 [39.625000, 62.916000] | 46.542000 [40.292000, 64.834000] | -0.083500 | +1.918000 |
| 3 | `fallback:before_compile` | 46.084000 [38.541000, 71.125000] | 47.792000 [40.208000, 65.000000] | +1.708000 | -6.125000 |
| 4 | `disabled:after_compile` | 46.875000 [42.458000, 55.667000] | 44.833500 [39.541000, 53.500000] | -2.041500 | -2.167000 |
| 4 | `disabled:before_compile` | 50.041000 [43.708000, 62.875000] | 45.125000 [40.333000, 60.333000] | -4.916000 | -2.542000 |
| 4 | `enabled:after_compile` | 72.999500 [40.375000, 95.291000] | 69.854000 [41.625000, 85.625000] | -3.145500 | -9.666000 |
| 4 | `enabled:before_compile` | 85.979500 [75.625000, 113.042000] | 89.833500 [72.375000, 119.541000] | +3.854000 | +6.499000 |
| 4 | `fallback:after_compile` | 46.188000 [42.208000, 52.666000] | 46.667000 [43.334000, 53.334000] | +0.479000 | +0.668000 |
| 4 | `fallback:before_compile` | 48.875000 [43.500000, 60.625000] | 45.688000 [40.292000, 60.750000] | -3.187000 | +0.125000 |
| 5 | `disabled:after_compile` | 42.708500 [38.417000, 53.041000] | 47.375000 [43.041000, 54.084000] | +4.666500 | +1.043000 |
| 5 | `disabled:before_compile` | 46.062000 [39.750000, 60.542000] | 50.729000 [44.375000, 85.208000] | +4.667000 | +24.666000 |
| 5 | `enabled:after_compile` | 72.604500 [38.541000, 82.583000] | 82.646000 [43.959000, 89.875000] | +10.041500 | +7.292000 |
| 5 | `enabled:before_compile` | 89.250000 [79.125000, 107.291000] | 90.771000 [80.500000, 104.125000] | +1.521000 | -3.166000 |
| 5 | `fallback:after_compile` | 47.666500 [41.375000, 70.333000] | 46.646000 [40.917000, 64.334000] | -1.020500 | -5.999000 |
| 5 | `fallback:before_compile` | 48.291500 [42.334000, 68.375000] | 48.395500 [42.958000, 64.125000] | +0.104000 | -4.250000 |
| 6 | `disabled:after_compile` | 41.916500 [38.791000, 52.667000] | 45.145500 [39.458000, 58.333000] | +3.229000 | +5.666000 |
| 6 | `disabled:before_compile` | 45.354500 [39.625000, 61.292000] | 42.687500 [39.334000, 54.375000] | -2.667000 | -6.917000 |
| 6 | `enabled:after_compile` | 65.458000 [43.542000, 92.583000] | 78.542000 [39.750000, 91.167000] | +13.084000 | -1.416000 |
| 6 | `enabled:before_compile` | 89.999500 [74.750000, 115.833000] | 94.604000 [84.167000, 107.416000] | +4.604500 | -8.417000 |
| 6 | `fallback:after_compile` | 43.145500 [39.583000, 51.667000] | 46.979500 [41.750000, 55.709000] | +3.834000 | +4.042000 |
| 6 | `fallback:before_compile` | 43.500000 [40.000000, 54.208000] | 43.812500 [40.166000, 56.250000] | +0.312500 | +2.042000 |

### Pooled HTTP distributions

Values remain in microseconds. All individual exceptions remain in the
preceding table even when a pooled delta falls below the limit.

| Block | Cache mode/phase | Baseline median [p05, p95] | Candidate median [p05, p95] | Added median us | Added p95 us |
| --- | --- | ---: | ---: | ---: | ---: |
| Initial 1–3 | `disabled:after_compile` | 41.458000 [36.750000, 50.375000] | 41.333000 [37.709000, 55.875000] | -0.125000 | +5.500000 |
| Initial 1–3 | `disabled:before_compile` | 46.146000 [39.541000, 79.667000] | 42.584000 [38.917000, 58.542000] | -3.562000 | -21.125000 |
| Initial 1–3 | `enabled:after_compile` | 71.521000 [40.167000, 91.417000] | 69.145500 [41.084000, 92.084000] | -2.375500 | +0.667000 |
| Initial 1–3 | `enabled:before_compile` | 84.312500 [70.125000, 102.833000] | 88.917000 [76.208000, 105.167000] | +4.604500 | +2.334000 |
| Initial 1–3 | `fallback:after_compile` | 44.687500 [39.250000, 68.375000] | 43.354000 [38.500000, 66.333000] | -1.333500 | -2.042000 |
| Initial 1–3 | `fallback:before_compile` | 43.145500 [38.583000, 61.792000] | 44.729500 [39.125000, 63.250000] | +1.584000 | +1.458000 |
| Confirmation 4–6 | `disabled:after_compile` | 43.896000 [38.833000, 54.542000] | 45.916000 [39.708000, 54.750000] | +2.020000 | +0.208000 |
| Confirmation 4–6 | `disabled:before_compile` | 47.312500 [39.958000, 62.458000] | 45.896000 [40.125000, 80.667000] | -1.416500 | +18.209000 |
| Confirmation 4–6 | `enabled:after_compile` | 72.875000 [40.000000, 91.583000] | 76.792000 [40.709000, 90.375000] | +3.917000 | -1.208000 |
| Confirmation 4–6 | `enabled:before_compile` | 89.229500 [76.375000, 111.667000] | 92.292000 [79.500000, 109.084000] | +3.062500 | -2.583000 |
| Confirmation 4–6 | `fallback:after_compile` | 45.729000 [39.917000, 59.375000] | 46.792000 [42.375000, 57.500000] | +1.063000 | -1.875000 |
| Confirmation 4–6 | `fallback:before_compile` | 47.166500 [40.833000, 62.583000] | 45.709000 [40.334000, 60.750000] | -1.457500 | -1.833000 |
| All 1–6 | `disabled:after_compile` | 42.666000 [37.333000, 52.625000] | 43.708500 [39.000000, 54.958000] | +1.042500 | +2.333000 |
| All 1–6 | `disabled:before_compile` | 46.917000 [39.750000, 77.583000] | 44.292000 [39.083000, 68.916000] | -2.625000 | -8.667000 |
| All 1–6 | `enabled:after_compile` | 71.729500 [40.125000, 91.583000] | 71.500000 [40.958000, 91.166000] | -0.229500 | -0.417000 |
| All 1–6 | `enabled:before_compile` | 85.959000 [71.375000, 109.166000] | 90.834000 [77.250000, 107.875000] | +4.875000 | -1.291000 |
| All 1–6 | `fallback:after_compile` | 45.354000 [39.625000, 63.167000] | 45.833000 [39.166000, 60.750000] | +0.479000 | -2.417000 |
| All 1–6 | `fallback:before_compile` | 45.375000 [39.625000, 61.917000] | 45.375000 [39.917000, 62.834000] | +0.000000 | +0.917000 |

### Evidence and stopped publication

Independent review reproduced all 210 analysis rows and checked all 126 final
distribution table rows against the raw logs. The review found no new accounting
defect or evidence arithmetic defect. Review records are `final-review.md` and
`residual-investigation.md` in the evidence directory below.

The residual investigation found that all four worker-slot median deltas agree
in sign within each pair. Six-pair pooled slot deltas range from +50.99021 to
+52.86396 ns per boundary. The positive overhead is therefore not solely one
slow worker inflating the maximum. Worker slots do not identify pinned CPUs,
and pooled samples do not remove process-level variation.

The unchanged harness gives the four concurrent Stores consecutive writer
ordinals, so each Store uses a distinct settlement shard in every sample.
Allocation, writer selection, and Arc retention occur before timing; snapshot
folding does not occur in the measured fuel loop. These facts exclude shard
collisions and direct setup costs in these samples, but not indirect cache or
scheduling effects from setup. The retained production assembly contains one
relaxed 64-bit `LDADD`, plus a zero check, shard bound check, and address work.
The shared decision counters remain active in both builds. Current evidence
does not quantify the cost of these instructions or identify a specific
hardware mechanism for the residual. No further experiment was executed.

All evidence is under
`.context/fuel-settlement-resume/acceptance-final-20261028/`: `protocol.md`,
`confirmation-reason.md`, `frozen-inputs.json`, `source-manifest.json`, both
build logs and binary manifests, both environment receipts, the 12 raw logs,
and their coverage receipts. `analysis-three.jsonl` covers pairs 1–3.
`analysis-confirmation.jsonl` numbers its pairs locally 1–3, corresponding to
pairs 4–6 in this report. `analysis-six.jsonl` preserves the global numbering.

The candidate passed local correctness but did not pass final performance
acceptance. The single permitted confirmation block is complete; further runs
are not authorized by this protocol. Publication remains blocked. Linux x86-64
CI was not executed, and no independent Linux performance claim is made.
OrbStack ARM64 functional results remain valid within their stated limits.
