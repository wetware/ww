# Fuel Auction

Peer-to-peer compute marketplace via RFQ (request-for-quote) protocol.

## What it demonstrates

- **ComputeProvider as a vat cell** -- the auction provider is a regular WASM guest, not host-side code
- `FuelPolicy::Oneshot` for budget-tracked cell execution (cells trap at fuel exhaustion)
- `Identity.signer()` for quote signing + `Identity.verify()` for signature verification
- `VatHandler::Serve` for persistent capability export (one auction object serves all bidders)
- Schema-keyed DHT discovery via `routing.provide()` / `findProviders()`
- `Runtime.load()` + `Executor.spawn()` with fuel policy for spawning metered child cells
- Nonce-based replay prevention with TTL pruning

## How it works

```
Consumer                     DHT                     Provider
    |                         |                         |
    |-- find_providers(CP) -->|                         |
    |<-- [provider_A, ...] ---|                         |
    |                         |                         |
    |-- dial + login ---------------------------------->|
    |<-- ComputeProvider cap ----------------------------
    |                         |                         |
    |-- quote(wasm_cid, 5M) --------------------------->|
    |<-- Quote{sig, price=120, nonce=42} ---------------
    |                         |                         |
    |-- accept(Quote) --------------------------------->|
    |    [verify sig, check nonce+expiry, spawn cell]   |
    |<-- Process ----------------------------------------
    |                         |                         |
    |  ... cell runs, fuel ticks down ...               |
    |  ... remaining_budget -> 0 -> Trap::OutOfFuel ... |
```

Quotes are **defunctionalized** -- plain structs, not live capabilities. A coordinator
agent can solicit quotes from multiple providers, compare them, and hand the best
quote to a worker agent. No live RPC references flow through the graph.

## Prerequisites

- Rust toolchain with `wasm32-wasip2` target:
  ```sh
  rustup target add wasm32-wasip2
  ```

## Building

```sh
make auction
```

## Schema

`auction.capnp` defines:

- **`Quote`** -- signed price commitment (price, fuel, expiry, CID binding, nonce, Ed25519 signature)
- **`ComputeProvider`** -- four methods:
  - `quote(wasmCid, fuelRequested)` -- get a signed quote
  - `accept(quote)` -- redeem a quote, spawn a metered cell
  - `price()` -- spot rate for quick comparison
  - `status()` -- live auction state for metrics

## Pricing

Posted price with utilization multiplier:

```
price = base_price * (1 + committed / total_capacity)
```

At 0% utilization, price equals the base rate. At 50%, it's 1.5x. At 90%, 1.9x.
The operator sets `base_price` and `total_capacity`.

## Security

- **Replay prevention:** Each quote carries a random nonce. Redeemed nonces are tracked.
  Expired nonces are pruned on each `accept()`. Hard cap at 10K stored nonces.
- **CID binding:** `wasmCid` in the Quote binds the price commitment to a specific binary.
  Providers reject `accept()` if the CID doesn't match.
- **Quote expiry:** Quotes are valid for 5 minutes (`QUOTE_TTL_SECS`). Expired quotes
  are rejected on `accept()`.
- **Budget enforcement:** Spawned cells use `FuelPolicy::Oneshot`. The epoch callback
  in the host runtime deducts consumed fuel from `remaining_budget` and stops refueling
  at 0. The cell traps naturally via `Trap::OutOfFuel`.

## Curl it

```sh
curl http://127.0.0.1:2080/auction
```

Returns JSON with current auction status:

```json
{
  "status": "ok",
  "base_price_per_mfuel": 1,
  "total_capacity": 10000000000,
  "utilization": 0.0
}
```

The HTTP endpoint returns static defaults (each WAGI invocation is a
fresh cell with no shared state). Real-time data comes from the vat
RPC `status()` method.

## Running

### Step 1: Run the provider node (daemon terminal)

Start a host with HTTP enabled:

```sh
ww run --http-listen 127.0.0.1:2080 --port=2025 std/kernel
```

Leave this process running.

### Step 2: Connect with `ww shell` (provider shell)

In a second terminal:

```sh
cd examples/auction
ww shell
```

If multiple local nodes are running, use `ww shell --select <index|peer-id>`.

### Step 3: Load snippets to register and serve

```clojure
/ > (load "glia/register.glia")
/ > (load "glia/serve.glia")
```

### Step 4: Query from a consumer (optional)

Open a second terminal and boot a consumer node:

```sh
ww run --port=2026 std/kernel
```

From another terminal, connect to that node:

```sh
cd examples/auction
ww shell
```

If prompted, select the host for the `--port=2026` node.

Then compare quotes from Glia:

```clojure
/ > (load "glia/register.glia")
/ > (perform auction :compare "QmWasmCid...")
```

## Demo snippets

`glia/register.glia`:

```clojure
;; One binary, two transports.
(def auction
  (cell (load "bin/auction.wasm")
        (load "bin/auction.capnpc")))

(perform host :listen auction)              ;; vat RPC (schema-keyed, libp2p)
(perform host :listen auction "/auction")   ;; HTTP/WAGI at /auction
```

`glia/serve.glia`:

```clojure
(perform runtime :run (load "bin/auction.wasm") "serve")
```

`etc/init.d/auction.glia` is now a deployment-only hook. Keep
init-based boot scripts for packaged images, but use snippets as the
default demo flow.

## Comparing from the shell

From a running Glia REPL:

```clojure
(perform auction :compare "QmWasmCid...")
;; => ({:provider "12D3Koo..." :price 120 :fuel 1000000 :expires 1743724800}
;;     {:provider "12D3Koo..." :price 150 :fuel 1000000 :expires 1743724800})
```

The `:compare` handler discovers providers via DHT, solicits quotes in parallel
(up to 10 providers, 5s timeout each), filters expired quotes, and sorts by price.

## Files

```
examples/auction/
├── Cargo.toml
├── Makefile              # make auction
├── README.md             # this file
├── auction.capnp         # ComputeProvider schema source
├── bin/                  # build output (gitignored)
│   ├── auction.wasm
│   └── auction.capnpc    # compiled schema bytes
├── glia/
│   ├── register.glia     # shell-loaded registration
│   └── serve.glia        # DHT provide loop
├── etc/
│   └── init.d/
│       └── auction.glia  # deployment-only hook
└── src/
    └── lib.rs            # guest implementation
```

## Phase 2 roadmap

- **Metered FuelPolicy** -- JIT refueling via `FuelSource` capability (pay-as-you-go)
- **Stake-weighted ordering** -- providers weighted by on-chain collateral
- **Membrane fuel attenuation** -- `GraftPolicy` maps identity to fuel budget
- **Operator market** -- aggregator vat routes bids to cheapest/closest provider
