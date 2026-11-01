# Local observability qualification (#755)

The first local slice correlates a real Cell instantiation timeout through
Prometheus, Loki, and Tempo. Grafana navigation was exercised in both
directions between the structured event and its trace. No production
deployment, GitHub publication, or merge was performed.

The baseline is fetched `origin/master` at `f7881b75`. See the
[foundation audit](../observability-foundation.md),
[setup and incident recipe](../observability.md), and
[recorded results](observability-results.json).

## Recorded incident

`tests/observability_incident.rs` runs two real Cells through `ExecutorPool`:
one ordinary successful P3 command and one component whose start function
waits past the existing instantiation deadline. The harness retains the latter
engine's epoch publisher without ticking it. It uses production `Builder`,
`Proc`, filesystem ownership, metrics collectors, and `AdminService`.

The canonical post-drain-fix run recorded:

| Evidence | Observed value |
| --- | --- |
| Prometheus timeout counter | `0` before the incident, `1` after it |
| Structured event | `event_code=300`, `elapsed_ms=5002`, `timeout_ms=5000` |
| Log owner | Process-local `owner_id=2` |
| Trace ID | `28f68665d728f1062c518d8cc565c4e9` |
| Span ID | `c46aec67ea6a74e0` |
| Cell span duration | Approximately 5.0065 seconds |
| Executor state after both tasks | Two completed factory lifetimes, zero occupied slots |

`scripts/observability-evidence.py` checks the current incident's time window,
the Prometheus transition, the Loki record, and exact matching trace/span IDs.
It also compares exported event durations with the log fields. Browser checks
confirmed **Open trace**, span/event expansion, and **Logs for this span**.

The evidence establishes an instantiation timeout. It does not establish why
epoch publication stopped. The deliberate harness setup is separate knowledge.
There is no instantiation child span or internal wait breakdown. The aggregate
metric has no exemplar; metric-to-log association uses time, outcome, and this
isolated workload. Histogram p95 interpolates coarse buckets and does not equal
the exact five-second event duration with two observations.

## Runtime bounds and failure behavior

Export is absent unless `WW_OTLP_ENDPOINT` is configured. The enabled default
samples 1% of eligible Cell roots. The runtime exports OTLP/HTTP JSON using the
existing `reqwest` dependency, without introducing an OpenTelemetry SDK.
The layer shares the existing redaction visitor instead of allowing an SDK
to capture raw tracing fields.

The qualified Loki input is the incident harness's formatter output. That
harness sends guest stderr to a sink. Full `ww run` stderr is not a safe
substitute: `src/kernel.rs` passes PID0 stderr directly to the process stream,
outside the Host formatter. Arbitrary guest JSON also bypasses a JSON-only
ingestion check. A separate Host-log destination is required before extending
this recipe to mixed daemon stderr; this slice does not add that destination.

| Boundary | Limit |
| --- | --- |
| Retained sampled spans | 256 |
| Completed-span queue | 256 |
| Serialized span | 16 KiB |
| Events / links per span | 16 / 8 |
| Per-field-record serialized map | 4 KiB; larger maps become a truncation marker |
| Export batch | 32 spans |
| Admission rate | Default 100 spans/second; configured range 1–10,000 |
| HTTP request | One-second timeout, no retries |
| Parsed response body | At most 4 KiB retained |
| Drain | Absolute 1.5-second deadline; caller waits at most two seconds |

The queue holds at most 4 MiB of serialized span payload. These payload limits
are not a whole-process RSS bound: active JSON values, allocator metadata,
request construction, HTTP buffers, and the existing tracing registry add
memory. Each exporter-owned collection has a fixed capacity or byte bound.
Tests directly exercise active-span and queue saturation without a consumer.

Collector HTTP work runs on a dedicated thread. Callbacks use a nonblocking
queue send and try-lock admission; the collector never holds runtime span
locks. Existing synchronous log writes still depend on the local filesystem.
The shutdown flag uses Release/Acquire publication before the worker reads the
absolute deadline. An in-flight request can finish within its existing timeout.

Real Cell failure tests produced these results after the drain and ordering corrections:

| Case | Runtime result | Telemetry result |
| --- | --- | --- |
| No endpoint | Successful command and expected timeout both complete | No exporter requests |
| Unavailable at startup | Same Cell outcomes | Two export drops |
| Sampling zero | Same Cell outcomes | Two sampled-out roots; no requests |
| Collector stops after first accepted span | Same Cell outcomes | One accepted span and one export drop |
| Stalled collector, 1,000 ordinary Cells | All Cells complete; active spans return to zero | 679 queue drops, 97 export drops, 224 shutdown drops |
| Healthy collector, 35 ordinary Cells | All Cells complete | 35 acknowledged spans; zero drops |

The stalled run took approximately 3.661 seconds including cleanup, with a
1.5022-second measured exporter drain. Exact drop partitions depend on timing;
the regression requires runtime completion, positive queue/export loss, and a
bounded drain. Counters distinguish local losses from collector acceptance.
Collector acceptance does not prove Tempo storage. Partial OTLP rejection is
counted, and response error text is never emitted.

## Descriptive cost evidence

Environment: macOS 26.6.2, arm64, Rust 1.96.0, unoptimized Cargo test profile.
Heavy builds and workloads used
`/tmp/ww-fuel-coordination/build-benchmark.lock`.

Three campaigns alternate process order. Each process runs five warmups and
30 measured P3 no-op Cells. Timing covers admission, component compilation,
execution, filesystem cleanup, and task retirement. It excludes process
startup and exporter drain. The detached master build uses the same workload;
its post-measurement telemetry-metric writer is absent.

| Mode | Median of three campaign medians |
| --- | ---: |
| Master `f7881b75` | 2.0765 ms |
| Candidate, export disabled | 2.0617 ms |
| Candidate, 100% sampling | 2.2551 ms |

Disabled measured 0.71% below master; this does not establish a speedup or zero
overhead. Enabled measured 9.38% above disabled for this small workload at
100% sampling. The enabled default's 1% cost was not measured. There is no
production throughput, release-profile, or tail-latency acceptance claim.
No historical performance rejection was reclassified.

Each enabled process exported all 35 spans in 24,666 JSON request-body bytes,
approximately 705 bytes per Cell for this workload. Each process produced
630 log bytes of startup/shutdown output. These are not network-byte totals
and do not predict arbitrary guest workloads. One disabled process took
1.195 seconds overall; the raw result is retained, and its cause is unknown.
That process's measured Cell median was 2.1354 ms.

After investigation, a Docker snapshot measured approximately 496 MiB across
the six containers. The configured limits total 2,304 MiB. The snapshot is not
a peak or a capacity guarantee. Storage is disposable, size-limited tmpfs;
the regular runtime log requires rotation for longer investigations.

## Validation and independent review

- `cargo test --workspace --lib --locked -- --test-threads=2`: 1,120 tests
  passed before the final shutdown refinement. The affected exporter tests
  were rerun after that refinement.
- Logging contract/output/volume and live metrics integration tests passed.
- Exporter tests cover task interleaving, explicit parents, causal links,
  redaction, field/event bounds, admission/queue limits, partial responses,
  stopped/slow transport, zero sampling, and healthy bounded shutdown.
- A live Collector test accepted an approved `u64::MAX` diagnostic as a
  decimal string attribute, avoiding OTLP's signed-int64 overflow.
- Strict workspace/all-target Clippy, workspace formatting, and diff whitespace
  checks passed after the recorded mechanical lint repair.
- Pinned Compose, Prometheus, Collector, and Alloy configuration validation
  passed. Backend readiness and Grafana provisioning passed.
- Separate agents reviewed exporter correctness/security and stack/harness
  reproducibility. Browser testing checked actual incident navigation.

Review fixes include a classified redacted configuration failure, repeated-init
exporter ownership, healthy shutdown drain, deadline publication ordering,
Grafana Viewer Explore access, a supported interpretation of absent metrics,
and restriction of Loki ingestion to the qualified harness log.
Initial shutdown-loss, compile, and Clippy evidence remains under
`.context/observability`; later success does not erase those attempts.

Not validated: the complete workspace integration suite, Linux container-to-host
reachability, release builds, production load, sustained memory high-water,
backend storage exhaustion, upstream image security, or cross-host propagation.
The build warned that optional embedded WASM artifacts were absent; the selected
workloads construct and execute their own real P3 components.

## Remaining acceptance work

Keep #755 open. This slice covers finite Cell lifecycle roots, local transport,
and one classified timeout. It does not explain internal wait causes, sample
already-running unsampled Cells, propagate peer context, or provide metric
exemplars. Full daemon log ingestion also needs an isolated Host formatter
destination. Sampled traces can be incomplete after bounded loss.

The next slice can reuse the landed native filesystem ownership diagnostics
to explain delayed cleanup, with the same bounded export and explicit missing
evidence. Integrate #781 only after its API and merge stabilize. Broader network,
RPC, and IPFS instrumentation remains with #754; cancellation correctness remains
with #785. Production Grafana/VPS deployment remains outside this work.
