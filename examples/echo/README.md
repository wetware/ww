# Echo Cell

Minimal stdin/stdout echo cell for integration testing.

## What it demonstrates

- **Raw cell** (`WW_CELL_MODE=raw`) -- no schema, no Cap'n Proto
- `StreamListener` for byte-stream protocol handling
- WASI P2 `cli::run` guest interface
- Full spawn-pipe-collect pipeline validation

## Prerequisites

- Rust toolchain with `wasm32-wasip2` target:
  ```sh
  rustup target add wasm32-wasip2
  ```

## Building

```sh
make echo
```

Or manually:

```sh
cargo build -p echo --target wasm32-wasip2 --release
mkdir -p examples/echo/bin
cp target/wasm32-wasip2/release/echo.wasm examples/echo/bin/echo.wasm
```

## Running

### Step 1: Run a node (daemon terminal)

Start a host:

```sh
ww run --port=2025 std/kernel
```

Leave this process running.

### Step 2: Connect with `ww shell` (shell terminal)

In a second terminal:

```sh
cd examples/echo
ww shell
```

If multiple local nodes are running, use `ww shell --select <index|peer-id>`.

### Step 3: Load the demo snippet to register the handler

From the Glia prompt:

```clojure
/ > (load "glia/register.glia")
```

This registers protocol `"echo"` with `StreamListener`. Each incoming
stream spawns a fresh echo cell.

## How it works

```
  Caller
    │
    ▼
┌──────────┐     stdin: raw bytes
│ Runtime  │ ──────────────────────► ┌───────────┐
│ +Executor│ ◄────────────────────── │ Echo Cell │
└──────────┘     stdout: same bytes  │ (WASI P2) │
                                     └───────────┘
```

The echo cell implements the WASI `cli::run` guest interface.
On start, it polls stdin in a loop, copying each chunk to stdout
verbatim. On EOF it flushes and exits. No dependencies beyond
`wasip2` and `wit-bindgen`.

This is the simplest possible cell: no schema, no capability
negotiation, no RPC. It exists to validate that the host can
spawn a WASM process, pipe bytes through it, and collect the
output.

## Demo snippet

`glia/register.glia`:

```clojure
; Register the echo cell as a raw stream handler.
; StreamListener spawns a cell per connection.
(perform host :listen runtime "echo" (load "bin/echo.wasm"))
```

`etc/init.d/echo.glia` is now a deployment-only hook. Keep
init-based boot scripts for packaged images, but use snippets as the
default demo flow.

## Tests

The echo cell is used by the end-to-end integration test:

```sh
cargo run --example echo_handler_e2e
```

This exercises `Runtime.load()` -> `Executor.spawn()` ->
stdin/stdout round-trip.

## Files

```
examples/echo/
├── Cargo.toml          # standalone WASI P2 crate
├── Makefile            # make echo
├── README.md           # this file
├── bin/                # build output (gitignored)
│   └── echo.wasm
├── glia/
│   └── register.glia   # shell-loaded demo registration
├── etc/
│   └── init.d/
│       └── echo.glia   # deployment-only hook
└── src/
    └── lib.rs          # guest implementation
```
