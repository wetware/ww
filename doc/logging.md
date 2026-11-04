# Operational logging contract

Metrics detect problems. Typed state explains current conditions. Traces provide
causal details. Logs report significant events. Normal successful Cell and RPC
lifecycle detail uses TRACE and remains disabled by default.

`config::init_tracing` installs the Host subscriber. Both JSON and compact output
use the same field policy in `src/logging.rs`. The existing `RUST_LOG`, stderr TTY,
`WW_LOG_FORMAT`, and `NO_COLOR` rules are described in [CLI logging](cli.md#host-logging).
An application that installs its own subscriber owns that subscriber's policy.

## Event vocabulary

`rpc::observability::contract` defines the vocabulary. Recognized `event_code`
values select literal `category`, `operation`, `outcome`, `reason`, and `message`
fields. The subscriber supplies these values; recorded strings cannot override
them. Every recognized event includes numeric `contract_version=1`.

| Code | Category | Operation | Outcome | Reason | Emission level |
| --- | --- | --- | --- | --- | --- |
| 1 | kernel | initialize | failed | initial_init_failed | ERROR |
| 2 | kernel | initialize | failed | epoch_init_failed | ERROR |
| 3 | kernel | replace | completed | generation_replaced | INFO |
| 4 | kernel | graft | superseded | epoch_advanced | INFO or WARN |
| 5 | kernel | replace | completed | interactive_replaced | INFO |
| 6 | kernel | teardown | timed_out | teardown_deadline | ERROR |
| 100 | host | start | completed | started | INFO |
| 101 | host | stop | completed | stopped | INFO |
| 102 | host | run | failed | service_failed | ERROR |
| 103 | host | configure | fallback | invalid_format | WARN |
| 111 | host | configure_telemetry | disabled | configuration_or_start_failed | WARN |
| 104 | host | join | failed | join_failed | ERROR |
| 105 | host | run | failed | worker_failed | ERROR |
| 106 | host | run | failed | worker_panicked | ERROR |
| 107 | host | compile | failed | queue_poisoned | ERROR |
| 108 | host | start | failed | worker_start_failed | ERROR |
| 109 | host | compile | failed | worker_panicked | ERROR |
| 110 | kernel | run | completed | process_exited | INFO |
| 200 | cell | run | completed | process_exited | TRACE |
| 201 | cell | run | failed | process_failed | WARN or ERROR |
| 202 | cell | teardown | failed | cleanup_failed | ERROR |
| 203 | cell | terminate | requested | termination_request | TRACE |
| 204 | cell | drain_stderr | completed | eof | TRACE |
| 205 | cell | drain_stderr | truncated | drain_deadline | WARN |
| 206 | cell | drain_stderr | cancelled | owner_dropped | WARN |
| 207 | cell | drain_stderr | failed | read_failed | WARN |
| 208 | cell | drain_stderr | failed | worker_lost | WARN |
| 300 | p3 | instantiate | timed_out | instantiation_deadline | ERROR |
| 301 | p3 | compile | fallback | cache_unavailable | WARN |
| 302 | p3 | compile | fallback | cache_configuration_invalid | WARN |
| 400 | rpc | connection | failed | connection_failed | WARN or ERROR |
| 401 | rpc | teardown | failed | cleanup_failed | WARN or ERROR |
| 402 | rpc | accept | rejected | connection_budget_exhausted | WARN |
| 403 | rpc | accept | failed | listener_ended | WARN |
| 404 | rpc | authenticate | timed_out | login_deadline | WARN |
| 405 | rpc | teardown | timed_out | cleanup_deadline | WARN |
| 500 | rpc | register_route | completed | route_registered | TRACE |
| 501 | rpc | remove_route | completed | route_unregistered | TRACE |
| 600 | host | advance_epoch | completed | authority_revoked | INFO |
| 601 | host | activate_deployment | completed | root_activated | INFO |
| 602 | host | prepare_deployment | retrying | transient_failure | WARN |
| 603 | host | prepare_deployment | rejected | invalid_head | WARN |

Codes 1–6 retain the values in `KernelEventCode`. Code 4 covers both rejected stale
grafts and superseded generation results. Code 200 reports process termination;
`exit_code` distinguishes normal completion from termination or failure. Code 203
reports a request, not completed cleanup. Code 6 now uses `timeout_ms` for
the configured deadline; `teardown_ms` remains the measured duration on code 3. Code 110 similarly requires its `code`
field to distinguish successful and failed exits.

The callsite determines severity. Consumers must use `event_code` and typed
fields, not message wording or source line numbers. New codes and optional fields
can be added within version 1. Existing codes must never be reused or renumbered.
Changing a code's meaning or a field's type requires a new code or contract
version. Consumers must tolerate unknown codes and absent optional fields.
Cell-local codes 300–302 use the same documented numeric values without adding a
Cell dependency on RPC. The subscriber resolves these codes through the same catalog.

## Fields and correlation

Each JSON record contains `timestamp`, `level`, `target`, `line`, and `fields`.
`timestamp` is UTC wall time. `line` and `target` identify the source callsite;
neither is a stable event identifier. `span`, when present, describes only the
current event span. Compact output retains `field=value` pairs, including numeric
`event_code=N`, and renders the same safe human message.

| Optional field | Type | Meaning |
| --- | --- | --- |
| `owner_id` | integer | Process-local ExecutorPool owner identity |
| `cell_id` | integer | Process-local ordinary child Cell identity |
| `rpc_id` | integer | Process-local managed RPC connection identity |
| `connection_id` | integer | Process-local accepted VAT or stream connection identity |
| `registration_id` | integer | Process-local HTTP route registration identity |
| `epoch`, `seq`, `issued_epoch` | integer | Relevant authority/deployment generation |
| `old_epoch`, `new_epoch`, `intended_seq`, `live_seq` | integer | Explicit generation transition or comparison |
| `exit_code`, `code` | integer | Observed process exit status |
| `elapsed_ms`, `teardown_ms`, `duration_us` | integer | Authoritative measured duration, with units in the name |
| `timeout_ms`, `retry_ms` | integer | Configured deadline or retry delay; not measured elapsed time |
| `capacity`, `active` | integer | Connection budget capacity and observed active connections |
| `workers`, `worker` | integer | Executor count or process-local worker identity |
| `count`, `bytes`, `grant`, `current_epoch`, `generation` | integer | Explicitly permitted bounded numeric diagnostics |
| `active`, `completed` | boolean | Explicitly permitted boolean diagnostics |
| `error`, `first_error` | object | Structured source chain when recorded as `dyn Error` |

The complete numeric allowlist lives in `src/logging.rs`; field names do not
authorize string representations. For example, `cell_id="secret"` is redacted.
Producer code must never encode secrets into an approved numeric field.
Approved 128-bit integer fields are clamped to the corresponding 64-bit range;
clamping sets `fields_truncated=true`. Ordinary elapsed-time values retain their
integer type. Floating-point fields are not approved.

Identity namespaces are distinct. An `owner_id` and `cell_id` with the same value
do not identify the same object. IDs reset across Host processes and must be
scoped by the collecting process. IDs are not Prometheus labels.

The subscriber preserves the async ownership rules from PR #776. It does not
invent correlation when a span is filtered or a task lacks propagated context.
JSON does not include ancestor spans or `follows_from` links. See
[async tracing ownership](runtimes.md#async-tracing-ownership).

## Sensitive fields and error sources

The default policy denies free text. String fields, `Display`, `Debug`, and
interpolated messages become `[redacted]`. The formatter does not invoke their
formatters. Unknown events retain severity, source location, safe typed fields,
and available span context; their message is `unclassified event (text redacted)`.
Enabling TRACE does not disable redaction.

This policy covers credentials, capability payloads, guest content, filesystem
paths, protocol names, route prefixes, peer names, CIDs, arbitrary identifiers,
panic payloads, and error messages. It does not attempt to recognize secrets by
regular expression. Host-owned literal contract messages remain readable.

Record real errors with `error = &error as &dyn std::error::Error`. For `anyhow`,
use `error = error.as_ref() as &dyn std::error::Error`. `%error`, `?error`, and
interpolation erase source relationships before the subscriber receives them;
the subscriber redacts those values instead of fabricating a source chain.

A structured error contains `chain`, `truncated`, and `messages_redacted`.
`chain` stores the root first, followed by successive `Error::source()` values.
Each entry contains `depth`, the next entry's `source` index or null, `category`,
and a redacted `message`. Recognized `std::io::Error` nodes use category `io`;
other nodes use `error`. I/O nodes also include a fixed `kind` reason, such as
`permission_denied` or `timed_out`; unmapped kinds use `other`. These fields do
not expose Rust type names, OS error text, or raw messages.

The chain includes at most eight nodes. Further sources, including cycles, set
`truncated=true`; traversal stops. A null source on the final entry does not prove
the original error had no source when `truncated=true`. Error-message content has
zero permitted bytes; `[redacted]` is a fixed replacement, not a truncated secret.

## Output bounds and cost

The subscriber admits at most 32 recorded fields per event or span. Field names
must contain only ASCII letters, digits, underscores, or periods and fit within
64 bytes. Omitted fields set `fields_truncated=true`.
Contract fields and truncation markers are subscriber-generated additions.
Metadata is bounded and control characters are removed. Span updates apply the
same policy and limits as initial span fields.

A complete record is limited to 16 KiB, including its newline. If serialization
exceeds the limit, the subscriber replaces event fields with `record_truncated=true`
and a fixed message, and removes the span. JSON always contains a complete object;
compact output remains one line.

`EnvFilter` selects events before the event formatter runs. Formatting-disabled
events produce no output bytes. Span-field filters retain their existing matching
semantics and can evaluate fields independently of output formatting.

The stderr writer is synchronous. There is no asynchronous logging queue, queue
capacity, or queue-overflow event loss counter. Slow stderr can block an emitting
thread; write failure can lose output. Filtering and record truncation must not be
reported as queue drops. This change adds no per-Host-return, scheduler-tick, or
fuel-grant events. Existing fuel diagnostics remain opt-in.

Representative volume and CPU measurements are recorded in
[structured logging qualification](measurements/structured-logging.md).

## Source audit and consumers

The audit started at fetched `origin/master` commit
`a85e4c4bf074a1d4f399de9daea92d5476bb9e2e`, including merged PR #776.

| Source or consumer | Observed behavior and contract change |
| --- | --- |
| `src/config.rs` | One initializer selects JSON or compact stderr and preserves `RUST_LOG` and an already-installed subscriber. |
| `src/services.rs` | ExecutorPool owner spans previously relied on arbitrary `cell_name`; numeric `owner_id` provides output correlation. |
| `src/launcher.rs` | Ordinary children retain independent `cell_id` spans. Process failures record typed errors; normal exit detail uses TRACE. |
| `crates/rpc/src/managed_rpc.rs` | RPC workers retain independent `rpc_id` spans. Secondary teardown errors preserve both source chains. |
| VAT and stream listeners | Guest-chosen protocol/service names remain unavailable in output. Failures expose event codes, connection budgets, and structured errors. |
| `crates/cell/src/proc.rs` | Native P3 instantiation timeout uses code 300 with measured `elapsed_ms` and configured `timeout_ms=5000`. Existing lifecycle/fuel diagnostics remain disabled at default levels. |
| `crates/cell/src/engine.rs` | Codes 301–302 report persistent-cache fallback without exposing cache paths or raw configuration values. |
| `crates/cell/src/p3.rs` | The native transport emits no additional tracing events. Existing Process and owner boundaries report significant failures. |
| `tests/logging_output.rs` | Exercises the installed subscriber in isolated subprocesses, including TTY selection, filtering, and concurrent task attribution. |
| `tests/pid0_e2e.rs` | Preserves numeric PID0 event-code checks. Route lifecycle checks use registration IDs; effective image identity comes from existing `/version` typed state. |

## Ordinary child stderr

The Host consumes ordinary child stderr as untrusted bytes. It retains no raw
messages, parses no guest levels or context, and emits no per-line events.
Structured-looking guest JSON has no authority to create Host events. This is
the metadata-only slice of #155; an authorized raw-output channel is separate work.
`Process.stderr` retains its existing empty stream behavior.

Each child has a 64 KiB duplex and an 8 KiB reader buffer. Newlines, partial
lines, and invalid UTF-8 have no special meaning: every received byte contributes
to `bytes`. There is no text decoder and therefore no encoding-error outcome.
The reader retains constant memory and yields after each chunk. It discards raw
content deliberately; the summary describes consumption, not message retention.

After guest execution ends, the lifecycle owner drops the Store and applies one
one-second deadline to the reader join. Individual reads do not reset the deadline.
The stderr and RPC teardown deadlines run concurrently.
Normal exit, guest failure, kill, and cooperative Host shutdown use this same
drain path. EOF certifies consumption of all bytes accepted by the duplex. It
does not certify guest writes cancelled before acceptance or delivery to a log
collector. A kill can interrupt a guest write even when the subsequent drain
reaches EOF.

The owner emits one summary before publishing backend cleanup. Code 204 means
EOF was observed and the reader joined. On deadline expiry, the owner aborts and
joins the reader before reporting code 205. A completed read failure reports
code 207. These two outcomes can still certify resource teardown, but neither
certifies complete output consumption. An unexpected reader task failure reports
code 208 and withholds cleanup certification. Dropping the lifecycle owner aborts
the reader and reports code 206 without claiming that asynchronous cleanup
finished. Existing `Process.wait` cleanup-loss behavior remains in effect.

Every summary includes the Host-assigned `cell_id`, `bytes`, and `completed`.
`cell_id` remains available when filters disable the child span. `completed` is
true only for code 204. `bytes` counts consumed bytes, including delimiters and
invalid UTF-8, and saturates at `u64::MAX` (then a lower bound). Incomplete summaries
also include the configured `timeout_ms=1000`; this is not elapsed time or proof
that cancellation waited for the deadline. The unread tail size is unknown;
zero consumed bytes with `completed=false` does not mean the child wrote nothing.

Successful summaries remain TRACE-only. Incomplete summaries use WARN. The Host
subscriber and trace exporter apply their existing bounds and filters; neither
provides durable delivery of these summaries.

The deadline is cooperative, not a hard wall-clock limit on full teardown.
Executor scheduling, abort/join processing, filesystem cleanup, and the existing
synchronous log writer can add latency. The fixed-size reader yields between
chunks so continuously ready output does not starve its deadline.

Issue #155 remains open. Its original per-record output requirements are not
implemented here: operators cannot read guest messages, guest levels and context
are not preserved, and unstructured lines receive no record level. This byte-only
path also provides no invalid-encoding counter or exact lost-byte count. Those
requirements need a separately authorized guest-output contract. PID0 isolation
remains separate under #755.

## Scope and remaining gaps

The policy applies to the Host tracing subscriber, including enabled dependency
events. It does not sanitize returned RPC errors, direct CLI error rendering,
interactive shell output, or direct PID0 guest stderr. `src/kernel.rs` still
passes process stderr directly to PID0. Full-daemon Host-log isolation remains
under #755. These outputs must not be mistaken for sanitized JSON Host event
records.

Unconverted events intentionally lose free-text explanations. Source location and
safe numeric fields remain available; future meaningful events require a reviewed
code and typed fields. An unknown event does not inherit another event's reason.
The #778 logging slice added no OTLP exporter, causal-link serialization,
production log collector, or retention system. The subsequent
[local observability workflow](observability.md) adds bounded OTLP export,
causal links, and a local collector stack. Workload volume and timing evidence
belong in the qualification report; formatter structure alone does not establish
production performance.
