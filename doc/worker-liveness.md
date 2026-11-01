# Executor worker liveness

## Detection model

Each executor worker owns a current-thread Tokio runtime and a `LocalSet`.
A synchronous blocking operation or a future poll that does not return prevents
that worker from scheduling other work. Pending P3 I/O and cooperative fuel
suspension return control to the executor and do not constitute worker stalls.

The worker root publishes once before admitting Cells. A periodic task on the
same `LocalSet` then publishes confirmed event-loop progress every 250 ms.
Missed ticks are skipped. The existing admin service reads per-worker atomic
state from its own thread; observation requires no response from the worker.
The publisher does not use guest hooks, Cell histories, logging, or the worker-0
epoch publisher. The failure model was recorded before implementation.

`ww_executor_worker_stalled` becomes 1 at five seconds since the last confirmed
publication. A fresh publication clears the stall at the next observation.
Timestamps use `std::time::Instant` and microsecond resolution, independent of
Tokio's test clock and wall-clock adjustments. Timestamp truncation can advance
the threshold boundary by less than one microsecond.

A stall means no observed event-loop progress for that interval. It does not
prove permanent deadlock or identify a Cell. OS descheduling, debugger pauses,
long polls and timer starvation can also delay publication. Machine suspend
behavior depends on the platform's monotonic clock. A responsive worker does
not prove that every Cell or the epoch publisher progresses. If the admin thread
or entire process stalls, an external scrape watchdog must detect the missing
response. Worker-0 stalls can stop epoch publication while other workers remain
responsive and their Cells correctly await fuel; those workers are not stalled.

## Operator contract

The existing Host collector supplies these families on `GET /metrics`:

| Metric | Meaning |
| --- | --- |
| `ww_executor_worker_heartbeat_interval_seconds` | Target publication interval, 0.25 seconds. |
| `ww_executor_worker_stall_threshold_seconds` | Stall threshold, 5 seconds. |
| `ww_executor_worker_state{worker,state}` | One-hot lifecycle: `starting`, `running`, `shutting_down`, `stopped`. |
| `ww_executor_worker_progress_age_seconds{worker}` | Seconds since confirmed progress; absent until the first publication. |
| `ww_executor_worker_stalled{worker}` | 1 when progress age reaches the threshold, except after the worker stops. |

The `worker` label is the decimal pool index, from zero through the configured
worker count minus one. Four lifecycle samples, one stall sample, and at most
one age sample exist per worker. Two policy samples exist before pool startup.
There are no Cell or generation labels. Scrapes compute current age; no separate
observer timer or cached stall bit can become stale. With scrape interval S and
responsive monitoring, detection occurs by approximately five seconds plus S
since the last publication. Collection and transport delays add to that bound.

`starting` means the worker has not entered its event loop. No confirmed age is
invented during startup, and startup delay is not classified as an event-loop
stall. Alert separately on prolonged startup if required.

`shutting_down` becomes visible when shutdown is requested, even if the worker
cannot acknowledge the request. It also covers worker-acknowledged drain.
Shutdown does not prove progress: age and stall status remain visible until a
fresh publication or stop. `stopped` is published only after the runtime and
LocalSet have been destroyed, including unwinding. Stopped workers retain their
last progress timestamp; reported age continues increasing, but stalled=0. A stopped worker is not a healthy worker.
Unexpected worker termination has the same stopped state; existing error logs
provide the exit reason.

A snapshot atomically reads each worker's timestamp and phase. Shutdown request
state and different workers are sampled separately. Concurrent shutdown or
publication can appear on the next scrape; the snapshot is not a pool-wide
transaction. A short registry mutex protects startup/replacement and cloning
the pool pointer. Workers never acquire that mutex. Sampling does not wait for
worker acknowledgement, retry a seqlock, or send work to the executor.

Pool replacement reuses worker indices with fresh state. An overlapping live
pool on the same collector is rejected. Replacing a stopped pool discards its
records, including indices removed by a smaller worker count. A retained
collector does not retain threads. Persistent state is O(configured workers),
and each scrape allocates O(configured workers). Caller-retained snapshots are
ordinary caller-owned values.

`/healthz` and `/readyz` keep their existing contracts. The observer does not
change worker assignment, fuel policy, epoch ownership, Cell cancellation, or
worker restart behavior. No automatic termination or restart occurs.

## Validation and measurements

See [qualification and performance](measurements/worker-liveness.md) for commands,
independent review, deterministic gates, measurements and qualification limits.
