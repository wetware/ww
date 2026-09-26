# Fuel Scheduling: EWMA Ratio Estimator

This document covers the design and rationale of the cooperative fuel
scheduler that multiplexes WASM cells onto executor worker threads.

Primary code references:

- `crates/cell/src/sched.rs` — constants
- `crates/cell/src/proc.rs` — `FuelEstimator`, call hook, epoch callback
- `src/services.rs` — `ExecutorPool` and epoch tick task

## Problem

Each executor worker thread runs many cells on a single `LocalSet`.
Wasmtime's `fuel_async_yield_interval` ensures cells yield every N
instructions, but we still need to decide *how much fuel to grant* each
cell per scheduling epoch.  Too much and a compute-heavy cell starves
its siblings.  Too little and an I/O-heavy cell wastes yield overhead on
work that would have voluntarily suspended anyway.

Time-based preemption (Cloudflare Workers, V8 interrupt API) ties the
limit to wall-clock time, which varies with CPU speed.  Instruction-
based metering via Wasmtime fuel is deterministic: the same binary
consumes the same fuel regardless of host clock.  This makes scheduling
behavior independently verifiable — a property that matters for
on-chain attestation.

The scheduler and one-shot accounting use separate state:

- `authority_grant` is the guest compute that the ledger permits in the current slice.
- `physical_installed` is `authority_grant + YIELD_RESERVE`, as passed to `set_fuel()`.
- `total_remaining` is a one-shot Cell's cumulative compute authority.
- `epoch_remaining` is its compute authority in the current epoch.
- `quantum` is the EWMA estimator's desired grant size.

The authority ledgers can reduce any grant below the desired quantum. The
physical reserve is runtime machinery. Installing the reserve does not debit or
increase either authority ledger.

## EWMA ratio estimator

The scheduler tracks each cell's *consumed/installed ratio* — the fraction
of its installed grant actually burned between observations.  The ratio is
smoothed with an exponentially weighted moving average (EWMA, α = 0.3)
and the next quantum is sized *inversely* to the smoothed ratio.

### Observation

At each `ReturningFromHost` boundary (a WASI import completing):

```
consumed = physical_installed - get_fuel()
ratio    = min(consumed * RATIO_SCALE / authority_grant, RATIO_SCALE)
```

Wasmtime also emits call hooks for internal libcalls and fuel or epoch yields.
The Store therefore records one marker for each live `CallingHost` frame. A
linked import marks only its current frame. The matching `ReturningFromHost`
pops that frame and updates EWMA only when the frame is marked.

The frame stack preserves nested or overlapping P3 transitions. Two live
imports cannot collapse into one boolean marker, and an unmarked internal
return cannot consume a marked outer frame.

### EWMA update

```
if first observation:
    avg_ratio = ratio                          // seed, avoid cold-start bias

else:
    avg_ratio = (avg_ratio * 7 + ratio * 3) / 10
    //        = 0.7 * avg_ratio + 0.3 * ratio
    // Single integer division to minimize truncation.
```

α = 0.3 balances responsiveness against noise.  Lower values (0.1) are
too sluggish — a cell that shifts from I/O to compute takes many epochs
to converge.  Higher values (0.5+) over-react to transient bursts.
0.3 matches the smoothing factor used in TCP RTT estimation (Jacobson
1988) for the same reason: the signal is noisy, but workload shifts are
real and must be tracked within a handful of observations.

### Quantum sizing

```
quantum = (MAX_FUEL * (RATIO_SCALE - avg_ratio) / RATIO_SCALE)
          .clamp(MIN_FUEL, MAX_FUEL)
```

| Workload | avg_ratio | Desired quantum |
|---|---|---|
| Pure I/O (consumed ~ 0) | ~0 | MAX_FUEL (10M) |
| Balanced | ~500 | 5M |
| Pure compute (consumed ~ installed) | ~1000 | MIN_FUEL (10K) |

### Why inverse?

The ratio depends on the installed grant. If the quantum were sized
*proportionally* to the ratio, a cell that exhausts its grant would receive an even
larger one next epoch, increasing consumption, spiraling toward MAX_FUEL
under bursty workloads.  Inverse sizing breaks the positive feedback
loop: high consumption shrinks the quantum, which reduces the next
observation's consumed count, stabilizing the ratio.

This is the same insight behind multiplicative-decrease in AIMD
congestion control — the corrective direction must oppose the signal
direction to converge.  We use a continuous inverse mapping rather than
AIMD's step function because the fuel ratio is a smooth signal (not a
binary loss/no-loss event).

## Fuel accounting and epoch notification

### Fuel-flushed call hooks

The async call hook handles marked `ReturningFromHost` transitions and
confirmed out-of-gas returns. Cranelift flushes its function-local fuel counter
before both boundaries. The hook can therefore read precise Store fuel.

A marked return settles actual consumption and updates the EWMA. The hook
tracks the Store fuel expected at the next out-of-gas return. Other unmarked
Wasmtime and libcall returns are ignored.

At an intermediate out-of-gas return, the hook computes the authority left in
the current grant. The hook re-arms `fuel_async_yield_interval` to the smaller
of that remainder and `YIELD_INTERVAL`. Wasmtime preserves and repartitions the
remaining physical fuel. The next yield therefore lands at the authority
boundary, subject only to one inter-checkpoint segment.

The hook makes one explicit decision after settlement:

- `Grant(g)` installs a new authority grant plus the physical reserve.
- `Suspend` waits on the paired epoch clock before opening a new epoch.
- `Exhausted` returns the explicit `one-shot total fuel authority exhausted` error.

Scheduled Cells always choose `Grant`. Scheduled Cells never wait for an epoch
because they consumed a quantum.

Guest Wasm executed during host-call machinery remains charged. This includes
guest realloc callbacks used by Component Model lowering and lifting.

Each marked return increments `host_calls_this_epoch`. Opening an epoch resets
that count after the old epoch has been settled.

### Epoch callback

The epoch callback is notification-only. Cranelift does not flush or reload its
function-local fuel counter around the `new_epoch` libcall. Store fuel is not
authoritative inside this callback.

The callback coalesces the latest published sequence into `pending_epoch` and
returns `UpdateDeadline::Continue(1)`. The callback does not read fuel, settle
ledgers, open an allowance, install fuel, or report authority exhaustion. The
hook ignores the callback's immediate unflushed `ReturningFromHost` event.

At the next marked return or confirmed out-of-gas return, the hook performs the
authoritative order:

1. Read precise Store fuel and settle work against the old epoch.
2. Debit `total_remaining` and `epoch_remaining`.
3. Open the latest pending or published epoch once.
4. Reset `epoch_remaining` and compute `FuelDecision`.
5. Update the Store deadline and install physical fuel.

Multiple ticks before that hook coalesce to one current-epoch allowance. The
suspend path uses the same `open_epoch` operation after its watch receiver
wakes.

## Yield interval vs quantum

Two independent knobs control cooperative scheduling:

| Knob | Value | Controls |
|---|---|---|
| `fuel_async_yield_interval` | min(remaining authority, YIELD_INTERVAL) | Re-arms each slice toward the authority boundary |
| EWMA quantum | MIN_FUEL..MAX_FUEL | Desired grant before one-shot authority clamps |
| YIELD_RESERVE | 10M | Physical fuel retained for runtime boundary machinery |

A scheduled Cell with a MAX_FUEL (10M) grant can yield every 10K
instructions. Each yield returns `Poll::Pending` to the LocalSet. The quantum
determines the desired work between accounting boundaries.

After each intermediate yield, the hook re-arms the interval to
`min(YIELD_INTERVAL, remaining_authority)`. A final partial grant therefore
reaches the async hook while physical reserve remains.

Wetware accepts encoded components up to `MAX_COMPONENT_BYTES` (8 MiB).
Production charges at most one fixed fuel unit per encoded operator and zero
variable bulk-operation cost. Every operator occupies at least one encoded
byte. `YIELD_RESERVE` is 10M, so one production straight-line segment costs
less than the reserve. A guard test documents how Wasmtime's default variable
bulk-operation cost can exceed the same reserve when this production
assumption is absent. The reserve is runtime machinery, not authority.

## One-shot authority exhaustion

Cells spawned with `FuelPolicy::Oneshot` use two cumulative ledgers:

- `totalBudget` is the total guest-compute authority for the Cell.
- `maxPerEpoch` is the cumulative guest-compute authority cap for one epoch.

Marked host returns and confirmed out-of-gas returns use one accounting
operation:

```
consumed        = physical_installed.saturating_sub(get_fuel())
total_remaining = total_remaining.saturating_sub(consumed)
epoch_remaining = epoch_remaining.saturating_sub(consumed)
grant            = min(quantum, total_remaining, epoch_remaining)
physical         = grant + YIELD_RESERVE
set_fuel(physical)
```

`Builder::build` compiles, links, validates, and prepares the component. It does
not install physical fuel or call executable instantiation. `Proc::run` first
evaluates `FuelDecision`. A zero total returns explicit authority exhaustion
before `instantiate_async`, so no guest start function or guest entry point can
execute.

For a nonzero grant, `Proc::run` installs the first reserve-aware slice before
`instantiate_async`. The normal async fuel hook, `HostCallFrames`, epoch
callback, pending-epoch state, suspension path, and exhaustion path are already
installed. Start functions therefore use the same runtime accounting as
`wasi:cli/run`. After successful instantiation returns to host code, any
unsettled start-function consumption uses the same settlement and decision
operation shown above. A final remainder smaller than `MIN_FUEL` remains usable
as a partial grant.

An epoch callback records `pending_epoch`. The next fuel-flushed hook first
settles old-epoch consumption. The hook then resets `epoch_remaining` to
`maxPerEpoch`. An epoch transition never resets `total_remaining`.

`minPerEpoch` is the EWMA quantum floor. `minPerEpoch` does not force a grant
above either authority ledger. The runtime normalizes the floor at or below the
effective quantum ceiling, including when `maxPerEpoch < MIN_FUEL`.

When `total_remaining` reaches zero, `FuelDecision::Exhausted` returns a
deliberate runtime error. Wetware does not use `Trap::OutOfFuel` as the semantic
terminal signal.

When `epoch_remaining` reaches zero while `total_remaining` is still positive,
`FuelDecision::Suspend` parks the whole Store in the async call hook. The hook
waits on `watch::Receiver::changed()`. The next epoch signal opens exactly one
new allowance, installs a bounded grant, moves the Store deadline forward, and
resumes guest execution.

Actual execution can cross an authority boundary by one Wasmtime
inter-checkpoint segment. Settlement charges the complete segment to both
ledgers. The runtime does not mint later authority to compensate for overshoot.

## Epoch tick placement

`RuntimeEngine` pairs the shared `Arc<Engine>` with a versioned epoch
subscription. `ExecutorPool` owns the sole `EpochPublisher` on worker 0. Each
tick holds the shared epoch-sequence lock while it calls
`Engine::increment_epoch()`, records the paired sequence, and publishes the
sequence. A synchronous epoch callback records the paired sequence through the
same lock. A fuel-flushed hook opens the latest published sequence. This
ordering does not expose a new allowance before the engine increment completes.

Dropping worker 0 closes the only watch sender. A suspended Cell then returns a
runtime error instead of waiting forever.

## Constants

| Name | Value | Rationale |
|---|---|---|
| INITIAL_FUEL | 1,000,000 | Initial desired quantum; one-shot ledgers can reduce the first grant |
| MAX_FUEL | 10,000,000 | I/O-bound convergence ceiling |
| MIN_FUEL | 10,000 | Default EWMA quantum floor; not a minimum authority grant |
| YIELD_INTERVAL | 10,000 | Normal async-yield interval |
| MAX_COMPONENT_BYTES | 8,388,608 | Bounds fixed-cost production segments below the reserve |
| YIELD_RESERVE | 10,000,000 | Physical runtime reserve; never one-shot authority |
| RATIO_SCALE | 1,000 | Fixed-point precision; 3 decimal digits |
| EPOCH_TICK_MS | 10 | 100 Hz accounting and one-shot resume cadence |

## References

- S.W. Roberts, "Control Chart Tests Based on Geometric Moving
  Averages," *Technometrics* 1(3), 1959.  Origin of the EWMA as a
  statistical process control tool.

- V. Jacobson, "Congestion Avoidance and Control," *SIGCOMM '88*.
  Introduces EWMA-smoothed RTT estimation for TCP with α = 0.125.
  The same smoothing principle applies here: noisy per-observation
  signals, smoothed to drive a control decision (RTT → RTO timeout;
  fuel ratio → quantum sizing).

- D.M. Chiu and R. Jain, "Analysis of the Increase and Decrease
  Algorithms for Congestion Avoidance in Computer Networks,"
  *Computer Networks and ISDN Systems* 17(1), 1989.  Formalizes AIMD
  convergence. Our inverse quantum sizing achieves the same corrective
  property (decrease opposes signal direction) without the step-function
  discontinuity.

- Wasmtime fuel documentation:
  https://docs.wasmtime.dev/api/wasmtime/struct.Config.html#method.consume_fuel

- Wasmtime epoch interruption documentation:
  https://docs.wasmtime.dev/api/wasmtime/struct.Config.html#method.epoch_interruption

- Cloudflare Workers CPU time limits:
  https://developers.cloudflare.com/workers/platform/limits/
  Time-based preemption model that Wetware's instruction-based approach
  improves upon for determinism.
