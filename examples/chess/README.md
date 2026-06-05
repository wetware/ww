# Chess Engine

Two-node cross-network chess over libp2p RPC capabilities.

## What it demonstrates

- **Cap'n Proto cell** (`WW_CELL_MODE=vat`) -- service-name vat RPC
- `VatListener` for per-connection capability cells
- `VatClient` for `VatConnection` dialing and lazy binding
- DHT discovery via `routing.provide()` / `findProviders()`
- IPFS replay log publishing
- Dual-mode binary: cell mode (RPC server) + service mode (discovery loop)

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
make chess
```

This compiles the WASM guest and embeds canonical `SchemaBundle` bytes in the
`ww.schema.v1` WASM custom section. The vat route uses the service name
`chess`; `VatConnection` exposes the embedded schema to dialers.

## Running

### Step 1: Run the two nodes (daemon terminals)

Start two hosts:

```sh
# Terminal 1
ww run --port=2025 std/kernel

# Terminal 2
ww run --port=2026 std/kernel
```

Leave both processes running.

### Step 2: Connect with `ww shell` (two shell terminals)

Open two more terminals:

```sh
# Terminal 3 (node on :2025)
cd examples/chess
ww shell

# Terminal 4 (node on :2026)
cd examples/chess
ww shell
```

If prompted, select the matching host for each port.

### Step 3: Load snippets on both nodes

From each Glia prompt:

```clojure
/ > (load "glia/register.glia")
/ > (load "glia/serve.glia")
```

Both nodes bootstrap into the DHT, exchange provider records,
discover each other, and play a game of random chess via typed RPC.

## How it works

### Image layers

The `ww run` command takes one or more **image layers** as positional
args. Each layer is a directory that gets merged into a single FHS
root, left to right. The kernel (PID 0) sees this merged root as
its virtual filesystem. If you run a layered image such as
`ww run --port=2025 std/kernel examples/chess`, the merged tree looks
like:

```
$WW_ROOT/
├── bin/
│   └── chess-demo.wasm    <- from examples/chess (built by make chess)
├── glia/
│   └── register.glia      <- shell-loaded snippet
├── boot/
│   └── main.wasm          <- from std/kernel
└── ...
```

The host publishes this merged directory to IPFS and sets `$WW_ROOT`
to `/ipfs/<cid>`. In the shell-forward flow, registration is loaded
explicitly from `glia/register.glia`.

### Architecture

```
             ww shell loads glia/register.glia
                           |
               (perform host :listen ...)
                           |
                       cell mode
                    (per-connection)
```

Two execution modes, selected by runtime inputs:

- **Cell mode** (`WW_CELL_MODE=vat`): per-connection vat cell
  spawned by `VatListener`. Creates a `ChessEngineImpl` and exports
  it via `system::serve()`. The host bridges the capability to the
  connecting peer via Cap'n Proto RPC bootstrapping.
- **Service mode** (default): long-running discovery loop. Provides the service
  locator on the DHT, discovers peers via `routing.find_providers()`, dials them
  with `VatClient`, binds the `VatConnection` to get typed `ChessEngine`
  capabilities, and plays random games. Exponential backoff (2 s to 15 min).

### Service Name And Schema Metadata

The protocol address is `/ww/0.1.0/vat/chess`. The name is a locator, not type
authority. The embedded schema bundle declares the `ChessEngine` capability
interface returned by `VatConnection.bind()`.

### Schema

```capnp
interface ChessEngine {
  getState      @0 () -> (fen :Text);
  applyMove     @1 (uci :Text) -> (ok :Bool, reason :Text);
  getLegalMoves @2 () -> (moves :List(Text));
  getStatus     @3 () -> (status :GameStatus);

  enum GameStatus {
    ongoing   @0;
    checkmate @1;
    stalemate @2;
    draw      @3;
  }
}
```

## Demo snippets

`glia/register.glia`:

```clojure
; Register vat cell for the ChessEngine capability.
; VatListener spawns a cell process per connection; the cell exports
; a ChessEngine capability via system::serve().
(def chess-wasm (load "bin/chess-demo.wasm"))

(perform host :listen :vat "chess" (cell chess-wasm))
```

`glia/serve.glia`:

```clojure
(perform runtime :run (load "bin/chess-demo.wasm") "serve")
```

`etc/init.d/chess.glia` is now a deployment-only hook. Keep
init-based boot scripts for packaged images, but use snippets as the
default demo flow.

## Tests

```sh
cargo test -p chess --lib
```

## See also

- [doc/replay.md](doc/replay.md) -- replay log structure

## Files

```
examples/chess/
├── Cargo.toml
├── Makefile              # make chess
├── README.md             # this file
├── chess.capnp           # ChessEngine schema source
├── bin/                  # build output (gitignored)
│   └── chess-demo.wasm   # final WASM with ww.schema.v1
├── glia/
│   ├── register.glia     # shell-loaded registration
│   └── serve.glia        # discovery + game loop
├── doc/
│   └── replay.md         # replay log format
├── etc/
│   └── init.d/
│       └── chess.glia    # deployment-only hook
└── src/
    └── lib.rs            # guest implementation
```
