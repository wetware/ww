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

- `installed` is the amount passed to the most recent `set_fuel()` call.
- `total_remaining` is a one-shot Cell's cumulative compute authority.
- `epoch_remaining` is its compute authority in the current epoch.
- `quantum` is the EWMA estimator's desired grant size.

The authority ledgers can reduce any grant below the desired quantum.

## EWMA ratio estimator

The scheduler tracks each cell's *consumed/installed ratio* — the fraction
of its installed grant actually burned between observations.  The ratio is
smoothed with an exponentially weighted moving average (EWMA, α = 0.3)
and the next quantum is sized *inversely* to the smoothed ratio.

### Observation

At each `ReturningFromHost` boundary (a WASI import completing):

```
consumed = installed - get_fuel()
ratio    = consumed * RATIO_SCALE / installed  // 0..1000
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

## Two accounting paths

### Path 1 — call_hook (I/O-bound cells)

Fires at each marked `ReturningFromHost` transition. The accounting path
settles the installed grant, updates the EWMA, and installs an
authority-bounded grant. Host calls are accounting boundaries. They do not
restore one-shot authority.

Guest Wasm executed during host-call machinery remains charged. This includes
guest realloc callbacks used by Component Model lowering and lifting.

Each marked return increments `host_calls_this_epoch`. The epoch callback then
knows that the call hook already observed the Cell during this epoch.

### Path 2 — epoch_deadline_callback (compute-bound cells)

Fires every `EPOCH_TICK_MS` (10 ms) when the epoch tick task calls
`Engine::increment_epoch()` and opens a new one-shot epoch allowance.

The callback checks `host_calls_this_epoch` only to decide whether to update the
EWMA:

- **Zero** — observe the Store's actual remaining fuel at the epoch boundary.
  Component Model I/O with low consumption stays near `MAX_FUEL`. Work that
  consumes most of its installed grant converges toward `MIN_FUEL`.
- **Non-zero** — the Cell made host calls; the call hook already updated the
  EWMA. Install a new grant without another EWMA observation.

The callback always settles consumption since the last accounting boundary. It
then resets the counter, resets `epoch_remaining`, and installs the next grant.
Consumption immediately before the epoch tick belongs to the old epoch.

### Why two paths?

A single path can't cover both workloads efficiently:

- call_hook alone misses compute-bound cells (no host calls to trigger it).
- epoch_deadline_callback alone observes at fixed intervals regardless of
  host call frequency, losing the fine-grained signal that makes EWMA
  responsive.

The combination gives frequent observations for I/O Cells and periodic
observations for compute Cells. The `host_calls_this_epoch` guard prevents a
duplicate EWMA observation. The shared settlement path prevents duplicate fuel
charges.

## Yield interval vs quantum

Two independent knobs control cooperative scheduling:

| Knob | Value | Controls |
|---|---|---|
| `fuel_async_yield_interval` | YIELD_INTERVAL (10K) | Requested fuel interval between async yields; smaller grants can exhaust first |
| EWMA quantum | MIN_FUEL..MAX_FUEL | Desired grant before one-shot authority clamps |

A scheduled Cell with a MAX_FUEL (10M) grant can yield every 10K
instructions. Each yield returns `Poll::Pending` to the LocalSet. The quantum
determines the desired work between accounting boundaries.

Scheduled-cell cooperative-yield and fuel-exhaustion behavior is outside the
one-shot accounting model. Issue #679 owns that behavior. In particular, this
design does not change `MIN_FUEL`, `YIELD_INTERVAL`, or the scheduled epoch
callback's `UpdateDeadline::Continue(1)` result.

## One-shot authority exhaustion

Cells spawned with `FuelPolicy::Oneshot` use two cumulative ledgers:

- `totalBudget` is the total guest-compute authority for the Cell.
- `maxPerEpoch` is the cumulative guest-compute authority cap for one epoch.

Initialization, marked host returns, and epoch callbacks use one accounting
operation:

```
consumed        = installed.saturating_sub(get_fuel())
total_remaining = total_remaining.saturating_sub(consumed)
epoch_remaining = epoch_remaining.saturating_sub(consumed)
grant            = min(quantum, total_remaining, epoch_remaining)
installed        = grant
set_fuel(grant)
```

Initialization skips settlement and applies the same grant clamp. A zero total
installs zero fuel. A final remainder smaller than `MIN_FUEL` remains usable as
a partial grant.

At an epoch boundary, the callback first settles consumption against the old
epoch. The callback then resets `epoch_remaining` to `maxPerEpoch`. An epoch
transition never resets `total_remaining`.

`minPerEpoch` is the EWMA quantum floor. `minPerEpoch` does not force a grant
above either authority ledger. The runtime normalizes the floor at or below the
effective quantum ceiling, including when `maxPerEpoch < MIN_FUEL`.

When `total_remaining` reaches zero, the runtime installs zero fuel. Wasmtime
traps on the next instruction that consumes fuel. Total-authority exhaustion is
terminal.

When `epoch_remaining` reaches zero while `total_remaining` is still positive,
the runtime also installs zero fuel. Current Wasmtime behavior traps if the Cell
consumes fuel before the next epoch tick. The Cell can therefore terminate with
unused total authority after it exhausts `maxPerEpoch`.

That per-epoch trap is current behavior, not the desired end-state. Issue #679
owns suspending or yielding the Cell until the next epoch opens new per-epoch
authority. Issue #672 does not change that behavior. The epoch callback
continues to return `UpdateDeadline::Continue(1)`.

## Epoch tick placement

All executor workers share a single `Arc<Engine>`.
`Engine::increment_epoch()` is a global atomic bump — calling it on N
workers would advance the epoch N times per tick, multiplying the
callback frequency.  The tick task runs on worker 0 only.

## Constants

| Name | Value | Rationale |
|---|---|---|
| INITIAL_FUEL | 1,000,000 | Initial desired quantum; one-shot ledgers can reduce the first grant |
| MAX_FUEL | 10,000,000 | I/O-bound convergence ceiling |
| MIN_FUEL | 10,000 | Default EWMA quantum floor; not a minimum authority grant |
| YIELD_INTERVAL | 10,000 | Configured async-yield interval; #679 owns scheduled exhaustion before a yield |
| RATIO_SCALE | 1,000 | Fixed-point precision; 3 decimal digits |
| EPOCH_TICK_MS | 10 | 100 Hz accounting cadence; scheduled-cell liveness remains tracked by #679 |

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
