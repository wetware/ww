# Chess Engine

Two-node cross-network chess over libp2p RPC capabilities.

## What it demonstrates

- **Cap'n Proto cell** (`WW_CELL_MODE=vat`) -- named vat RPC
- `VatListener.serve` for persistent capability export
- `VatClient` for typed RPC dialing
- Service-name DHT discovery via `routing.provide()` / `findProviders()`
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

This compiles the WASM guest and generates build-time schema-byte and CID
metadata for introspection.
Vat publication uses the service name `chess`.

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
               (perform host :serve-vat ...)
                           |
                  persistent ChessEngine cap
```

Two execution modes, selected by runtime inputs:

- **Cell mode** (no args): spawned by Glia before publication.
  Creates a `ChessEngineImpl` and exports it via `system::serve()`.
  `host :serve-vat` publishes that exported capability under `chess`.
- **Service mode** (default): long-running discovery loop. Provides
  the service-name routing key on the DHT, discovers peers via
  `routing.find_providers()`, dials them with `VatClient` to get
  typed `ChessEngine` capabilities, and plays random games.
  Exponential backoff (2 s to 15 min).

### Service Name

The vat protocol is the normal service name `chess`. The DHT key is
`routing.hash("chess")`, so the Routing API still receives a CID-shaped
key without making schema identity the locator. Schema bytes are
compiled at build time for tooling/introspection.

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
(def chess-wasm (load "bin/chess-demo.wasm"))
(def chess-executor (perform runtime :load chess-wasm))
(def chess-process (perform chess-executor :spawn))
(def chess-cap (perform chess-process :bootstrap))

(perform host :serve-vat chess-cap "chess")
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
│   ├── chess-demo.wasm
│   └── chess-demo.wasm
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
