# Discovery -- Greeter RPC

Two-agent Greeter demo showing named vat peer discovery over
the DHT. Agent A publishes a Greeter service. Agent B discovers
it by service name, dials it via Cap'n Proto RPC, and gets a
typed greeting back. No configuration, no service registry, no
hardcoded addresses.

## What it demonstrates

- **Cap'n Proto cell** (`WW_CELL_MODE=vat`) -- named vat RPC
- Explicit `VatListener.serveRaw` for this ungated discovery fixture
- `VatClient` for typed RPC dialing
- Service-name DHT discovery via `routing.provide()` / `findProviders()`
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

This compiles the WASM guest and generates build-time schema-byte and CID
metadata for introspection.
Vat publication uses the service name `greeter`.

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
/ > (perform :load "glia/register.glia")
/ > (perform :load "glia/serve.glia")
```

Expected output on Agent B:

```
[INFO] service: peer ..a1b2c3d4
[INFO] service: name greeter
[INFO] service: routing key bafy...
[INFO] service: looking for peers...
[INFO] service: found 1 peer(s)
[INFO] ..a1b2c3d4 -> ..e5f6g7h8: Hello, peer ..a1b2c3d4! I'm ..e5f6g7h8
```

## How it works

```
BUILD TIME:
  greeter.capnp --> capnpc --> discovery.wasm

AGENT A (service mode):                    AGENT B (service mode):
  membrane.graft()                           membrane.graft()
  routing.hash("greeter")                    routing.hash("greeter")
  routing.provide(key)  --DHT-->            routing.find_providers(key)
                                             |
                         <--libp2p stream--  vat_client.dial(A, "greeter")
  VatListener serves                         |
  persistent Greeter cap                     bootstrap --> Greeter cap
                                             greeter.greet("peer B")
                         --RPC response-->   "Hello, peer B! I'm A"
```

The DHT key is derived deterministically from the service name via
`routing.hash("greeter")`. The vat protocol is the normal service name
`greeter`; schema CIDs remain build-time metadata, not locators.

### Schema

```capnp
interface Greeter {
  greet @0 (name :Text) -> (greeting :Text);
}
```

### Cell mode vs service mode

The same binary serves both roles:

- **Cell mode** (no args): spawned by Glia before publication.
  Creates a `GreeterImpl` and exports it via `system::serve()`.
  `host :serve-raw-vat` explicitly publishes that exported capability under
  `greeter` without recipient authentication.
- **Service mode** (default): long-running discovery loop. Provides
  the service-name routing key on the DHT, discovers peers via
  `routing.find_providers()`, dials them with `VatClient`, and calls
  `greet()`. Exponential backoff (2 s to 15 min).

## Demo snippets

`glia/register.glia`:

```clojure
(def discovery-wasm (perform :load "bin/discovery.wasm"))
(def discovery-executor (perform runtime :load discovery-wasm))
(def discovery-process (perform discovery-executor :spawn))
(def discovery-cap (perform discovery-process :bootstrap))

(perform host :serve-raw-vat discovery-cap "greeter")
```

`glia/serve.glia`:

```clojure
(perform runtime :run (perform :load "bin/discovery.wasm") "serve")
```

`etc/init.d/discovery.glia` is now a deployment-only hook. Keep
init-based boot scripts for packaged images, but use snippets as the
default demo flow.

## Without Kubo

The demo works without Kubo. Schema push to IPFS is best-effort
at build time. Discovery happens via DHT `provide/findProviders`
regardless.

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
│   ├── discovery.wasm
│   └── discovery.wasm
├── glia/
│   ├── register.glia      # shell-loaded registration
│   └── serve.glia         # DHT provide + discovery loop
├── etc/
│   └── init.d/
│       └── discovery.glia # deployment-only hook
└── src/
    └── lib.rs             # guest implementation
```
