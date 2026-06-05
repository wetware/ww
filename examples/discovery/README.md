# Discovery -- Greeter RPC

Two-agent Greeter demo showing peer discovery over the DHT plus typed vat RPC.
Agent A publishes a `greeter` service. Agent B discovers a provider, dials the
`greeter` vat protocol, binds the returned `VatConnection`, and gets a typed
greeting back. No hardcoded addresses.

## What it demonstrates

- **Cap'n Proto cell** (`WW_CELL_MODE=vat`) -- service-name vat RPC
- `VatListener` for per-connection capability cells
- `VatClient` for `VatConnection` dialing and lazy binding
- DHT discovery via `routing.provide()` / `findProviders()`
- Dual-mode binary: cell mode (RPC server) + service mode (discovery loop)
- Exponential backoff with jitter for peer discovery

## Prerequisites

- Rust toolchain with `wasm32-wasip2` target:
  ```sh
  rustup target add wasm32-wasip2
  ```
- A running Kubo node for DHT bootstrap:
  ```sh
  ipfs daemon
  ```

## Building

```sh
make discovery
```

This compiles the WASM guest and embeds canonical `SchemaBundle` bytes in the
`ww.schema.v1` WASM custom section. Vat routes use service names;
`VatConnection` exposes the embedded schema to dialers.

## Running

### Step 1: Run the two nodes (daemon terminals)

Start two hosts:

```sh
# Terminal A
ww run --port=2025 std/kernel

# Terminal B
ww run --port=2026 std/kernel
```

Leave both processes running.

### Step 2: Connect with `ww shell` (two shell terminals)

Open two more terminals:

```sh
# Terminal C (node on :2025)
cd examples/discovery
ww shell

# Terminal D (node on :2026)
cd examples/discovery
ww shell
```

If prompted, select the matching host for each port.

### Step 3: Load snippets on both nodes

From each Glia prompt:

```clojure
/ > (load "glia/register.glia")
/ > (load "glia/serve.glia")
```

Expected output on Agent B:

```
[INFO] service: peer ..a1b2c3d4
[INFO] service: locator greeter, schema CID bafy...
[INFO] service: looking for peers...
[INFO] service: found 1 peer(s)
[INFO] ..a1b2c3d4 -> ..e5f6g7h8: Hello, peer ..a1b2c3d4! I'm ..e5f6g7h8
```

## How it works

```
BUILD TIME:
  greeter.capnp --> capnpc --> SchemaBundle --> ww.schema.v1 --> discovery.wasm

AGENT A (service mode):                    AGENT B (service mode):
  membrane.graft()                           membrane.graft()
  routing.provide(name) --DHT-->            routing.find_providers(name)
                                             |
                         <--libp2p stream--  vat_client.dial(A, "greeter")
  VatListener accepts                        |
  spawns cell (cell mode)                    bootstrap --> Greeter cap
  cell serves Greeter                        greeter.greet("peer B")
                         --RPC response-->   "Hello, peer B! I'm A"
```

The vat route is `/ww/0.1.0/vat/greeter`. The embedded schema bundle is still
the typed declaration for the `Greeter` capability returned by
`VatConnection.bind()`.

### Schema

```capnp
interface Greeter {
  greet @0 (name :Text) -> (greeting :Text);
}
```

### Cell mode vs service mode

The same binary serves both roles:

- **Cell mode** (`WW_CELL_MODE=vat`): spawned by `VatListener`
  per incoming RPC connection. Creates a `GreeterImpl` and exports
  it via `system::serve()`. The host bridges the capability to the
  connecting peer.
- **Service mode** (default): long-running discovery loop. Provides the service
  locator on the DHT, discovers peers via `routing.find_providers()`, dials them
  with `VatClient`, binds the `VatConnection`, and calls `greet()`. Exponential
  backoff (2 s to 15 min).

## Demo snippets

`glia/register.glia`:

```clojure
; Register vat cell for the Greeter capability.
; VatListener spawns a cell per connection; the cell exports
; a Greeter capability via system::serve().
(def discovery-wasm (load "bin/discovery.wasm"))

(perform host :listen :vat "greeter" (cell discovery-wasm))
```

`glia/serve.glia`:

```clojure
(perform runtime :run (load "bin/discovery.wasm") "serve")
```

`etc/init.d/discovery.glia` is now a deployment-only hook. Keep
init-based boot scripts for packaged images, but use snippets as the
default demo flow.

## Without Kubo

The demo works without Kubo for local testing. DHT provider discovery needs a
connected libp2p routing backend in real multi-host runs.

## Tests

```sh
cargo test -p discovery
```

Runs unit tests for the Greeter implementation and RPC round-trip
tests over in-memory Cap'n Proto duplex.

## Files

```
examples/discovery/
├── Cargo.toml
├── Makefile               # make discovery
├── README.md              # this file
├── greeter.capnp          # Greeter schema source
├── bin/                   # build output (gitignored)
│   └── discovery.wasm     # final WASM with ww.schema.v1
├── glia/
│   ├── register.glia      # shell-loaded registration
│   └── serve.glia         # DHT provide + discovery loop
├── etc/
│   └── init.d/
│       └── discovery.glia # deployment-only hook
└── src/
    └── lib.rs             # guest implementation
```
