# Fuel Scheduling: EWMA Ratio Estimator

This document covers the design and rationale of the cooperative fuel
scheduler that multiplexes WASM cells onto executor worker threads.

Primary code references:
- `src/sched.rs` — constants
- `src/cell/proc.rs` — `FuelEstimator`, call_hook, epoch_deadline_callback
- `src/runtime.rs` — `ExecutorPool`, epoch tick task

## Problem

Each executor worker thread runs many cells on a single `LocalSet`.
Wasmtime's `fuel_async_yield_interval` ensures cells yield every N
instructions, but we still need to decide *how much fuel to grant* each
cell per scheduling epoch.  Too much and a compute-heavy cell starves
its siblings.  Too little and an I/O-heavy cell wastes yield overhead on
work that would have voluntarily suspended anyway.

Time-based preemption (Cloudflare Workers, V8 interrupt API) ties the
budget to wall-clock time, which varies with CPU speed.  Instruction-
based metering via Wasmtime fuel is deterministic: the same binary
consumes the same fuel regardless of host clock.  This makes scheduling
behavior independently verifiable — a property that matters for
on-chain attestation.

## EWMA ratio estimator

The scheduler tracks each cell's *consumed/budget ratio* — the fraction
of its fuel budget actually burned between observations.  The ratio is
smoothed with an exponentially weighted moving average (EWMA, α = 0.3)
and the next budget is sized *inversely* to the smoothed ratio.

### Observation

At each `ReturningFromHost` boundary (a WASI import completing):

```
consumed = budget - remaining
ratio    = consumed * RATIO_SCALE / budget     // 0..1000
```

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

### Budget sizing

```
new_budget = (MAX_FUEL * (RATIO_SCALE - avg_ratio) / RATIO_SCALE)
             .clamp(MIN_FUEL, MAX_FUEL)
```

| Workload | avg_ratio | Budget |
|---|---|---|
| Pure I/O (consumed ~ 0) | ~0 | MAX_FUEL (10M) |
| Balanced | ~500 | 5M |
| Pure compute (consumed ~ budget) | ~1000 | MIN_FUEL (10K) |

### Why inverse?

The ratio depends on the budget.  If budget were sized *proportionally*
to the ratio, a cell that exhausts its budget would receive an even
larger one next epoch, increasing consumption, spiraling toward MAX_FUEL
under bursty workloads.  Inverse sizing breaks the positive feedback
loop: high consumption shrinks the budget, which reduces the next
observation's consumed count, stabilizing the ratio.

This is the same insight behind multiplicative-decrease in AIMD
congestion control — the corrective direction must oppose the signal
direction to converge.  We use a continuous inverse mapping rather than
AIMD's step function because the fuel ratio is a smooth signal (not a
binary loss/no-loss event).

## Two refueling paths

### Path 1 — call_hook (I/O-bound cells)

Fires at every `ReturningFromHost` transition (WASI import completing).
The estimator observes the remaining fuel, updates the EWMA, and reloads
the store with the new budget.  This is the fast path for cells that
make frequent host calls.

Each call increments `host_calls_this_epoch` to signal the epoch
callback that this cell is already being observed.

### Path 2 — epoch_deadline_callback (compute-bound cells)

Fires every `EPOCH_TICK_MS` (10 ms) when the epoch tick task calls
`Engine::increment_epoch()`.  This is the safety net for cells that
never (or rarely) make host calls — without it, a pure-compute cell
would exhaust its fuel and trap.

The callback checks `host_calls_this_epoch`:

- **Zero** — cell is compute-bound.  Observe full consumption
  (`on_host_return(0)`) so the EWMA converges toward MIN_FUEL.
- **Non-zero** — cell made host calls; the call_hook already updated the
  EWMA.  Just refuel without re-observing (prevents double-counting).

Then reset the counter and refuel.

### Why two paths?

A single path can't cover both workloads efficiently:

- call_hook alone misses compute-bound cells (no host calls to trigger it).
- epoch_deadline_callback alone observes at fixed intervals regardless of
  host call frequency, losing the fine-grained signal that makes EWMA
  responsive.

The combination gives accurate observation for I/O cells and guaranteed
liveness for compute cells, with the `host_calls_this_epoch` guard
preventing double-observation.

## Yield interval vs budget

Two independent knobs control cooperative scheduling:

| Knob | Value | Controls |
|---|---|---|
| `fuel_async_yield_interval` | YIELD_INTERVAL (10K) | *Preemption granularity* — how often a cell yields back to the LocalSet |
| EWMA budget | MIN_FUEL..MAX_FUEL | *Scheduling quantum* — how much total work a cell does between observations |

A cell with MAX_FUEL (10M instructions) still yields every 10K
instructions.  Each yield returns `Poll::Pending` to the LocalSet,
giving sibling cells a turn.  The budget determines how many of those
yield cycles the cell gets before the next EWMA observation.

Without the yield interval, a compute-heavy cell with MIN_FUEL (10K)
would run 10K instructions without yielding — acceptable.  But an I/O
cell with MAX_FUEL would run 10M instructions without yielding, starving
siblings for ~10 ms.  The yield interval caps the longest uninterrupted
run to ~10K instructions (~10 us) regardless of budget.

## Oneshot budget exhaustion

Cells spawned with a `FuelPolicy::Oneshot` carry a finite
`remaining_budget` that decrements each epoch.  When it reaches zero,
the epoch callback stops refueling.  The cell traps on the next
instruction that would consume fuel (`Trap::OutOfFuel`).

The callback doesn't trap directly — it returns
`UpdateDeadline::Continue(1)` without calling `set_fuel()`.  This is
deliberate: the trap happens deterministically at the next fuel
consumption point, not at an arbitrary epoch boundary.

The `OneshotFuel` schema allows per-cell `maxPerEpoch` / `minPerEpoch`
overrides, clamped to system limits.  This lets fuel-market callers
constrain burst behavior independently of total budget.

## Epoch tick placement

All executor workers share a single `Arc<Engine>`.
`Engine::increment_epoch()` is a global atomic bump — calling it on N
workers would advance the epoch N times per tick, multiplying the
callback frequency.  The tick task runs on worker 0 only.

## Constants

| Name | Value | Rationale |
|---|---|---|
| INITIAL_FUEL | 1,000,000 | ~1 ms at 1 GHz; neutral starting point |
| MAX_FUEL | 10,000,000 | I/O-bound convergence ceiling |
| MIN_FUEL | 10,000 | Compute-bound floor; prevents starvation |
| YIELD_INTERVAL | 10,000 | ~10 us preemption; matches MIN_FUEL for consistency |
| RATIO_SCALE | 1,000 | Fixed-point precision; 3 decimal digits |
| EPOCH_TICK_MS | 10 | 100 Hz; fast enough to catch compute-bound cells before fuel exhaustion at MIN_FUEL |

## References

- S.W. Roberts, "Control Chart Tests Based on Geometric Moving
  Averages," *Technometrics* 1(3), 1959.  Origin of the EWMA as a
  statistical process control tool.

- V. Jacobson, "Congestion Avoidance and Control," *SIGCOMM '88*.
  Introduces EWMA-smoothed RTT estimation for TCP with α = 0.125.
  The same smoothing principle applies here: noisy per-observation
  signals, smoothed to drive a control decision (RTT → RTO timeout;
  fuel ratio → budget sizing).

- D.M. Chiu and R. Jain, "Analysis of the Increase and Decrease
  Algorithms for Congestion Avoidance in Computer Networks,"
  *Computer Networks and ISDN Systems* 17(1), 1989.  Formalizes AIMD
  convergence.  Our inverse budget sizing achieves the same corrective
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
