# Observability foundation audit (#755)

The implementation starts from fetched `origin/master`
`f7881b75`, verified on 2026-11-01. Issue [#755](https://github.com/wetware/ww/issues/755)
defines the exporter slice. [#749](https://github.com/wetware/ww/issues/749)
retains the broader roadmap.

## Landed dependencies

| Work | Verified merge | Reused behavior |
| --- | --- | --- |
| #776 | `49099f53` | Task-owned spans across async polling; detached children use independent roots and causal links. |
| #777 / #782 | `971d23d3` | Canonical compilation and instantiation outcome counters and duration histograms. |
| #779 / #783 | `a85e4c4b` | Owner-local filesystem snapshots and cleanup receipts. |
| #780 / #786 | `24a2ff06` | Bounded CID-directory operation diagnostics with explicit omissions. |
| #787 / #789 | `3937229e` | Cooperative directory construction cancellation and retained native/rollback ownership. |
| #778 / #788 | `f7881b75` | Event vocabulary, typed correlation IDs, bounded output and deny-by-default field redaction. |

Durable-context searches used `gbrain search ... --source default` for Wetware
observability, #755, structured logging, and native filesystem ownership. Search
results did not supply a relevant exporter design. The resource-bounds concept
page provides historical guidance, not evidence of implemented telemetry.
Current source and the issue history establish the integration described here.

## Existing production metrics

`src/metrics.rs` renders 13 pre-existing families from live collectors:

- Fuel decisions and settled units: `ww_fuel_decisions_total`,
  `ww_fuel_settled_units_total`.
- Compilation cache: `ww_wasmtime_cache_state`, `ww_wasmtime_cache_hits_total`,
  `ww_wasmtime_cache_stores_total`, `ww_wasmtime_component_compilations_total`.
- Compilation lifecycle: `ww_wasmtime_component_compile_total`,
  `ww_wasmtime_component_compile_duration_seconds`.
- Instantiation lifecycle: `ww_wasmtime_instantiations_total`,
  `ww_wasmtime_instantiation_duration_seconds`.
- Executor admission and lifetime: `ww_executor_cell_slots`,
  `ww_executor_spawn_admissions_total`, `ww_executor_cell_terminations_total`.

Labels are fixed policy, decision, cache-state, outcome, and histogram-bound
sets. No peer, Cell, CID, or capability labels are needed. Executor `completed`
means the factory future returned; it does not certify guest success. Published
settled fuel excludes unpublished live-Store values and unsettled termination
tails. These meanings remain unchanged.

`AdminService` serves `/metrics` and `/version`. The latter supplies build and
kernel provenance separately from metric labels. There is no pre-existing
OTLP exporter or remote trace propagation in the audited initializer.

## Existing logs and spans

`config::init_tracing` uses `RUST_LOG`, `WW_LOG_FORMAT`, and `NO_COLOR`.
`logging::SafeFields` and `logging::SafeEvent` enforce the
[logging contract](logging.md). JSON records contain one current span. Existing
numeric IDs have distinct process-local namespaces: `owner_id`, `cell_id`,
`rpc_id`, `connection_id`, and `registration_id`.

Executor workers create `executor` spans. Each admitted factory receives a
`cell` span with `owner_id`. Ordinary child Cells create independent `cell`
roots with `cell_id` and a `follows_from` link to the spawning context.
The [async ownership audit](runtimes.md#async-tracing-ownership) describes other
service and RPC boundaries. The exporter must respect those boundaries.

`rpc::observability::contract` supplies event messages and bounded semantic
fields. Relevant lifecycle codes are 200 (process exit), 201 (process failure),
202 (cleanup failure), 203 (termination request), and 300 (instantiation timeout).
Code 300 records `elapsed_ms` and `timeout_ms`. It does not record the root cause
of delayed instantiation.

## First incident and scope

The selected workload reuses the real P3 start-loop fixture from
`tests/metrics_http.rs`. A retained epoch publisher does not advance the next
epoch. The ordinary five-second instantiation deadline then fires. The
production metric records the timeout and event 300 records its duration.
The exported Cell span can locate and time that event. The evidence does not
establish which internal wait caused the timeout; the harness knows how it
induced the condition, but that knowledge is not runtime-recorded causality.

The integration adds selective local export, collector-health counters, and
subscriber-generated trace correlation. It does not alter Store ownership,
fuel authority, scheduling, filesystem cancellation, or `crates/cell/src/proc.rs`.
The unmerged #781 inspector is not a dependency. Network/RPC/IPFS coverage
remains with #754, and further cancellation correctness remains with #785.
