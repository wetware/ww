# Settled fuel instrumentation measurements

Baseline: `cdf474b4cce0ed99f341d01fd6c3818f64a11d04` (#769).
The baseline includes applied fuel-decision instrumentation. These measurements
compare the additional settlement counter using the same committed release
profile, lockfile, Rust toolchain, machine, and `tests/fuel_overhead.rs` harness.

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

## Decision: stopped, candidate not accepted

All six paired runs exceeded the four-worker host-return budget of 100 ns added
median time per boundary. The pooled addition was **159.70 ns (+40.35%)**.
The single permitted confirmation block reproduced the regression. This meets
Landing 3's unresolved hot-path regression stop condition. Do not push or merge
this candidate. No PR, fresh remote CI, merge, deployment, or release promotion
was performed for this candidate.

The regression is consistent with contention on the additional shared atomic,
but these measurements do not establish its cause. No profiler was run. No
scheduler, accounting, collector optimization, or budget change was attempted
after the failure. Any follow-up must address the hot-path cost and repeat the
acceptance process before publication.

## Environment and sample accounting

Measured on 2026-10-26 with an Apple M5 Max (18 logical CPUs), macOS 26.6.2
(build 25G83), Rust 1.96.0 (`ac68faa20`), and LLVM 22.1.2. Both trees used the
committed release profile and lockfile. Both builds finished before timing.
The candidate is preserved in the local commit containing this report.

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

Fresh remote CI was not run because the performance stop condition blocks
publication. The required checks remain Test, Check formatting, Clippy,
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
