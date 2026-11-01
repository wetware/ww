# Structured logging qualification (#778)

Measured on 2026-11-01 with the final Host subscriber and a source-backed native
WASI P3 workload. Raw observations and machine/toolchain metadata are in
[structured-logging-results.json](structured-logging-results.json).

## Workload and method

`tests/logging_volume.rs` runs ordinary native P3 no-op Cells through the real
`ExecutorPool`, Cell builder, `Proc::run`, and filesystem completion. The fixture
uses the same `wasi:cli/run@0.3.0` command shape as `tests/lifecycle_overhead.rs`.
Each operation admits one Cell, constructs its Store, instantiates and runs the
component, waits for filesystem cleanup, and receives the executor result.

The volume test compares 16 sequential Cells with a zero-Cell control. Both runs
include pool startup and shutdown. Components are compiled before the measured
operation loop. The sink is a regular file. This isolates execution logging from
compilation, networking, application payloads, and guest stderr.

The cost experiment uses 256 Cells per sample, two warmups and seven recorded
samples for each format/filter pair. Case order alternates between repetitions.
It measures wall time and process user-plus-system CPU time inside the operation
loop. Compilation and shutdown are excluded. Allocations were not measured.

Both trees used the same test source, toolchain, profile, machine, and sink.
The baseline is master `a85e4c4bf074a1d4f399de9daea92d5476bb9e2e` with only the
measurement test added. Tests used the unoptimized Cargo test profile on an
Apple `Mac17,7` (`arm64`), Rust 1.96.0. Heavy builds and timed experiments held
`/tmp/ww-fuel-coordination/build-benchmark.lock`. All 40 power samples reported
AC power. The baseline run preceded the final run; the observations are not a
randomized cross-version production-throughput study.

## Log volume

Default INFO means no `RUST_LOG` with non-TTY stderr. WARN uses `RUST_LOG=warn`.
TRACE enables the eight internal crate targets named in `src/config.rs`.

| Format | Filter | Events, 16 Cells | Bytes, 16 Cells | Total events/Cell | Total bytes/Cell | Incremental events/Cell | Incremental bytes/Cell |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| JSON | default INFO | 3 | 630 | 0.1875 | 39.375 | 0 | 0 |
| JSON | WARN | 0 | 0 | 0 | 0 | 0 | 0 |
| JSON | TRACE | 99 | 23,232 | 6.1875 | 1,452 | 6 | 1,412.625 |
| compact | default INFO | 3 | 398 | 0.1875 | 24.875 | 0 | 0 |
| compact | WARN | 0 | 0 | 0 | 0 | 0 | 0 |
| compact | TRACE | 99 | 14,520 | 6.1875 | 907.5 | 6 | 882.625 |

The three INFO records are fixed pool lifecycle overhead. Successful Cells add
no default-level events in this workload. TRACE adds six records per Cell in the
recorded run. Epoch timing can change TRACE counts in longer workloads; the test
checks a finite workload-specific ceiling rather than claiming a universal rate.
Timestamp width is fixed; numeric ID width and measured durations can change
bytes per record. The largest recorded event was 267 bytes for JSON and 187 bytes
for compact output.

## CPU and wall cost

CPU medians, microseconds per Cell:

| Format | Filter | Master | Final | Change |
| --- | --- | ---: | ---: | ---: |
| JSON | OFF | 508.36 | 504.93 | −0.7% |
| JSON | default INFO | 517.84 | 519.04 | +0.2% |
| JSON | TRACE | 614.81 | 634.42 | +3.2% |
| compact | OFF | 505.16 | 506.00 | +0.2% |
| compact | default INFO | 513.50 | 520.01 | +1.3% |
| compact | TRACE | 574.58 | 604.32 | +5.2% |

Final wall medians were 518.16 µs/Cell for default JSON and 517.01 µs/Cell for
default compact output. Default-level CPU ranges overlap the baseline ranges.
These observations do not establish a material default-level CPU regression.
They also do not establish equivalence outside this workload. TRACE increased
CPU cost; its extra field collection, serialization, and writes remain opt-in.
This unoptimized result is not a release-build throughput estimate.

## Filtering, truncation, and loss

The final-subscriber tests verify that a disabled event does not evaluate a
panic-on-evaluation field. Rejected Debug/Display values and raw messages are
never formatted, even for enabled records. Final JSON and compact tests use a
real `anyhow` context with an I/O source, a cyclic source chain, large sensitive
strings, span updates, and concurrent tasks.

Source chains stop after eight nodes. Oversized field sets set
`fields_truncated`; records over 16 KiB become a complete truncation marker.
The contract tests exercise both limits. The representative workload emitted no
truncation markers. No async logging queue exists, so there are no queue-overflow
drops to count. The synchronous writer can block, and failed stderr writes can
lose records without a loss counter. Filtered records are not queue drops.

## Reproduction and limits

Run deterministic volume and contract checks:

```sh
cargo test --locked --test logging_contract --test logging_output --test logging_volume -- --nocapture
```

Under the shared build/benchmark lock and continuous AC monitoring, run:

```sh
cargo test --locked --test logging_volume measure_cell_logging_cost -- --exact --ignored --nocapture --test-threads=1
```

The ignored timing test requires the caller to enforce the lock and power check.
Repeat against the pinned baseline with the identical measurement test. Preserve
all raw samples; do not treat one timing observation as a performance threshold.

Not measured: release-build throughput, allocations, slow or broken stderr,
network request mixes, sustained guest output, failure storms, or production
traffic. Existing opt-in fuel and stream-chunk diagnostics can emit more records
for workloads that exercise those paths. No new scheduler or runtime-hook
instrumentation was added.
