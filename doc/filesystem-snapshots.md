# Owner-local filesystem snapshots

`FsCompletion::try_snapshot()` reads one filesystem root's existing ownership
state. Host Rust code that already holds `FsCompletion` can request a snapshot.
The API adds no guest capability, RPC method, HTTP endpoint, or global registry.
This is the filesystem slice of [#779](https://github.com/wetware/ww/issues/779)
under [#756](https://github.com/wetware/ww/issues/756).

## Source and ownership

`HostFsOwner` owns the filesystem driver, bounded root table, and cleanup thread
in `crates/cache/src/fs_driver.rs`. Each registered root has a `State` in
`crates/cache/src/fs_work.rs`. Its `Lifecycle` mutex already protects admission,
work receipts, cleanup claim, recorded failures, and cleanup completion.

`FsOwner` and `FsFinalizer` close root admission. `FsScope` admits work and
registers cleanup. `FsWorkToken` retains a work receipt until its owner drops it.
`NativeFsLease` in `crates/cell/src/fs_intercept.rs` carries a receipt through
native Wasmtime filesystem work. `ForegroundTransaction` continuations share
receipts; result envelopes can retain those receipts after a backend returns.
`FsCompletion::wait()` retains the existing frozen completion result.

The snapshot copies existing fields. It adds no operation counters or lifecycle
recording. The owner-local publication principle does not require Osaka's TLS
enrollment, grant routing, or worker counter sharding. Existing fuel collectors
remain independent of this API.

## Contract

`FsSnapshot` is a fixed-size value with read-only accessors:

| Accessor | Meaning |
| --- | --- |
| `root()` | Opaque slot/incarnation identity, comparable only within one `HostFsOwner` lifetime. |
| `root().generation()` | Existing slot generation. Changes on reuse; not a snapshot sequence. |
| `captured_at()` | Process-local monotonic `Instant` captured under the lifecycle mutex. |
| `root_admission_open()` | Root admission flag. Host closure or resource limits can independently reject work. |
| `outstanding_work()` | Outstanding ownership receipts, not individual operations or bytes. |
| `cleanup()` | `Pending`, `Ready`, `Running`, or `Completed`, as defined below. |
| `cleanup_poisoned()` | A recorded cleanup failure poisoned new cleanup-bearing admission. |
| `failure_recorded()` | Whether the root recorded any failure. No failure text is copied. |

Cleanup phase derives from the existing fields, in this order:

1. `Completed`: the reaper finished callbacks and set `finished`.
2. `Running`: the reaper claimed cleanup and has not set `finished`.
3. `Ready`: root admission is closed, work is zero, and cleanup is unclaimed.
4. `Pending`: root admission remains open or work receipts remain.

`Running` covers the entire claimed interval, including gaps between callbacks.
`Completed` does not mean success: inspect `failure_recorded()` and use `wait()`
for the completion result. It also does not certify root-table removal or permit
release, which follow completion recording. Completion recording precedes watch
notification. Existing code can record a late failure, so a later live snapshot
can differ from the frozen `wait()` result.

Guest cancellation does not prove native completion. Zero work does not prove
cleanup completion. A nonzero receipt count can describe retained results after
native completion. Ordinary I/O errors are not necessarily recorded failures.
The API cannot identify individual operations, guest cancellation requests,
backend start/end times, or the cause of a failure.

## Consistency, freshness, and resource bounds

Each successful read acquires the root mutex once with `try_lock` and copies
fixed-size fields. All fields describe the same root state while that mutex is
held. The snapshot does not combine host admission, permits, other roots, or
`Store` state. Root identities can repeat across separate host owners or restarts.

`FsCompletion` holds a weak reference for diagnostics. A call temporarily upgrades
the reference; holding the completion handle or snapshot does not retain work,
cleanup authority, or a live root. The weak reference retains the allocation's
control block after the root's contents are released, until the handle is dropped.

The call returns one of three errors instead of cached or partial data:

- `Busy`: the lifecycle mutex is held; the call did not wait or queue.
- `Unavailable`: no live root state remains. The completion result can remain
  available through `wait()` independently.
- `Poisoned`: the mutex cannot provide certified state.

There is no stale fallback, timeout queue, root scan, background task, history,
or diagnostic allocation per read. Only one reader holds the lifecycle mutex at
a time. Other concurrent reads return `Busy`. The critical section copies a
constant number of fields and captures one clock value. It does not await,
invoke callbacks, format messages, or acquire another lock. Scheduling can delay
a thread; this is not a hard real-time wall-clock guarantee.

The snapshot becomes historical as soon as capture ends. Consumers can compare
`captured_at()` with their own monotonic deadline. Consumers that retry `Busy`
must bound attempts or elapsed time. The API does not infer owner shutdown from
admission closure. After shutdown, a retained `FsScope` can still expose completed
state; after all strong references disappear, reads return `Unavailable`.

Existing lock order remains unchanged: table operations release the root-table
mutex before wake notification; candidate scans release it before lifecycle reads.
Root close and work release unlock lifecycle before wake notification. Cleanup
callbacks run outside those locks. Completion publication unlocks lifecycle before
watch access. Table removal drops its root reference outside the table lock, then
the reaper releases the cleanup obligation through its separate mutex.

## Internal consumer

`OwnedChildLifecycle::run_until_shutdown` in `src/launcher.rs` reads the existing
completion handle after guest exit and before waiting for filesystem cleanup.
With launcher debug tracing enabled, one event contains the snapshot or explicit
read error. For example, set `RUST_LOG=ww::launcher=debug` for the host process.
The tracing macro does not evaluate the read when the callsite is disabled.
This is a single lifecycle observation, not a polling service.

The event contains only the owner-local identity, timestamp, receipt count,
phase, and booleans. It does not contain paths, CIDs, capabilities, or error text.
Root identities are not Prometheus labels. The event does not change the existing
completion wait or delegated-authority release sequence.

## Extension boundary

[#780](https://github.com/wetware/ww/issues/780) can reuse this read contract and
existing root ownership handles when adding authoritative native-P3 operation
admission, cancellation, and backend-completion evidence. Those distinctions need
evidence at the native operation boundary; timestamps and receipt counts cannot
reconstruct them. The broader agent interface remains under #756.
