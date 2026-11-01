# On-demand fuel inspector (#781)

## Source audit and implementation decision

The starting revision is verified `origin/master` at
`24a2ff064e88d7c9f953ff10818b23a78ef7c923` (2026-11-01).
Issues #752, #756, #779 and #781 define the ownership and observation contract.
PRs #788 and #789 remain open at this audit; their changes are not assumed here.
During qualification, #789 merged. The branch was fast-forwarded without file
conflicts to `3937229ef5ba973281e13df8a16a50cf42662aff` and qualified again.
#788's pending logging changes remain outside this diff.

`Proc::run` owns its Wasmtime Store through instantiation and the P3 event loop.
`ExecutorPool` owns opaque factory futures (`src/services.rs`), not borrowable
Stores. The filesystem `FsCompletion::try_snapshot` copies mutex-protected
filesystem state; it does not provide Store access.

`FuelEstimator` owns EWMA and authority ledgers in `crates/cell/src/proc.rs`.
`settle_and_decide` commits consumption, clears current authority, and replaces
the physical installation field with an accounting baseline. The grant
installers mutate estimator fields before fallible Wasmtime calls. Consequently,
neither those fields nor the provisional `FuelObserver` events establish the
last successfully applied grant. The inspector must retain that receipt only
after both Wasmtime configuration calls succeed.

The epoch callback is notification-only. Cranelift does not flush its local
fuel counter around that callback. Physical fuel can be captured at marked
host returns, confirmed async fuel returns, and the existing outer Store
boundaries. Opening an epoch must remain after settlement of the old epoch.

Wasmtime's P3 `Accessor::with` offers scoped Store access, but the P3 event loop
can stop polling its closure while an async fuel hook holds the Store during
suspension. An accessor-only request loop cannot reliably explain suspension.

The selected design uses an opt-in specialization of the existing fuel hook.
The normal specialization excludes diagnostic work at compile time. A private
trait selects distinct application futures statically; retaining both awaits
inside one coroutine enlarged the default hook allocation even with const
specialization. The normal application function retains the original behavior
and allocation size. Both paths share settlement, estimator and grant installers.
The enabled specialization retains one settlement and one successful grant in owner-local
memory. It serves a bounded mailbox at fuel-flushed boundaries and while the
same hook owns an actual epoch wait. It does not replace the executor, borrow
across threads, or use the unbounded test observer.

## Access contract

A trusted embedder explicitly attaches the inspector to one prepared `Proc`
before execution. The returned read handle selects that Store incarnation.
The diagnostic identity is process-local, monotonically allocated, and never
rebound to another Store. It is not a guest PID or an authorization credential
for a remote service. No guest import, global Cell registry, or remote endpoint
is added. `crates/cell/examples/inspect_fuel.rs` consumes this handle.

One outstanding request is admitted with `try_acquire`; other readers receive
`Busy`. The mailbox capacity is one. A request carries its admission permit
through response delivery so slow readers cannot create response buffers.
The requester uses a deadline capped at one second independently of Store
progress. Timeout delivery requires the requester's executor to make progress;
this is not a hard wall-clock response bound on a blocked executor.
Expired or cancelled requests cannot become fresh responses. Store destruction
closes admission; it does not fabricate a final fuel settlement or exit cause.

Each snapshot copies fixed-size owner state and captures a monotonic timestamp
only when serving a request. Store ledgers, EWMA, last settlement, and applied
grant are coherent at that boundary. The latest published Host epoch is a
separate observation. Ledger balances exclude fuel consumed since settlement;
physical remaining fuel and the accounting baseline expose that difference.
`YIELD_RESERVE` remains physical machinery and never becomes authority.

An idle Cell without a fuel boundary can time out. A returned snapshot is
historical immediately after capture; consumers must check its age. Missing
settlement/grant history uses `None`, including before the first applicable
event. Owner termination returns an unavailable result, not zero balances.

`FuelState::Running` means the hook captured a guest-resume boundary after a
successful grant or intermediate yield rearm. It is not a worker-utilization
measurement. `Suspended` means an actual one-shot epoch wait returned `Pending`
with Tokio cooperative budget available. `Exhausted` accompanies the terminal
authority error, including a zero initial budget. A pending request can receive
that terminal boundary; later requests return `OwnerStopped`. Successful command
completion, traps and cancellation do not manufacture terminal fuel history.

The handle must be attached before `Proc::run`. This first slice does not attach
to an arbitrary already-running Cell. The host retains the selected handle
through execution and requests snapshots when needed. The diagnostic identity
identifies a Store incarnation within this host process; it is not stable across
host restarts or a replacement for launcher identity.

`last_applied_grant` records the quantum and ledgers at installation. The current
quantum can differ after settlement. In particular, a new one-shot estimator
starts with `INITIAL_FUEL`; its configured EWMA floor and ceiling apply after
observation, while the epoch ledger clamps even the initial grant.

## Local reporter

After building a native P3 command, run:

```sh
cargo run -p cell --example inspect_fuel -- command.wasm
cargo run -p cell --example inspect_fuel -- command.wasm 1337 10000
```

The first command uses scheduled policy. The second sets total and per-epoch
one-shot authority. The reporter queues one inspection before execution,
prints the initial successfully applied grant and snapshot age, then runs the
command. The reporter supplies empty stdin and sinks stdout/stderr. It uses the
existing filesystem ownership and cleanup path. Its five-second execution
deadline excludes compilation and cleanup and requires executor progress.

The compiled native P3 fixture demonstrated:

```text
desired quantum:       1,000,000
total authority:           1,337
epoch authority:          10,000
applied authority:         1,337 = min(1,000,000, 1,337, 10,000)
physical installed:  10,001,337 = 1,337 + YIELD_RESERVE
```

The fixture then returned the expected total-authority exhaustion error. The
nonzero process result does not invalidate the captured initial grant. The
compiled routing-key P3 test separately observes a live epoch suspension and
resumption after publishing the next epoch.

## Resource and default-path costs

Normal Stores add one `Option<Box<Owner>>` field and one dispatch when `run`
begins. The `FuelCallHook<false>` specialization excludes all inspection calls.
It adds no per-boundary allocation, atomic, lock, clock, or counter operation.
Fuel enforcement, immediate decision metrics, and settlement publication keep
their existing order and meaning.

Enrollment allocates the optional owner, capacity-one channel and one semaphore,
and replaces the hook with the enabled specialization.
The generation counter advances once per enrollment. Inspected Stores retain
one settlement and one applied grant, not an event history. An enabled hook
checks its mailbox at a valid boundary. It captures time and copies the snapshot
only for a live request. The snapshot has no vectors, strings or guest data.

The single admission permit travels through the queued request and unread
response. Rejected readers do not allocate reply channels. Cancelled or expired
queued requests retain the permit until the owner discards the request or drops.
Inspection never waits for reader consumption. During suspension, each serviced
request yields before another request; epoch readiness takes priority.

This bounds internal outstanding storage, not copies a trusted caller chooses
to retain after reading a snapshot. No timer polls Stores and no background
diagnostic task exists. The enabled path has additional channel operations and
owner-local record writes. No throughput improvement is claimed.

## Qualification

Use independent Store fuel and ledger arithmetic for partial grants, reserve
overshoot, epoch coalescing and EWMA clamps. Exercise a real compiled P3 Cell,
actual suspension/rearming, nested frames, zero authority, installation failure,
Store destruction and generation isolation. Test request admission, cancellation,
timeouts and slow readers against fixed resource bounds.

Run fuel/Cell/P3/workspace tests, strict Clippy and formatting under the shared
`/tmp/ww-fuel-coordination/build-benchmark.lock` for heavy builds. Review the
default specialization and perform a representative lifecycle smoke. Timed
comparisons require continuous AC monitoring. Preserve all prior rejected
performance evidence. Independently review correctness, authority and ownership.

See [the qualification report](../measurements/fuel-inspector.md) for executed
checks, retained failures, smoke results and remaining limits.

The flight recorder, sampling metrics, logging formats, filesystem ownership,
cancellation policy and remote diagnostics remain outside this change.
