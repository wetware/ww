# Echo Cell

A minimal WASI stdin/stdout echo guest for integration testing.

## What it demonstrates

- raw-cell behavior with `WW_CELL_MODE=raw`
- WASI Preview 2 `cli::run`
- byte-for-byte stdin/stdout forwarding
- spawn, pipe, and process collection

## Build

```sh
rustup target add wasm32-wasip2
make echo
```

The build produces `examples/echo/bin/echo.wasm`.

## Runtime composition status

The repository keeps the Rust guest and direct handler E2E. The Rust PID0
installs only `/status`; no echo listener composition is currently shipped.

## Tests

```sh
cargo run --example echo_handler_e2e
```

## Files

- `src/lib.rs`: guest implementation
- `Makefile`: WASM build
- `bin/echo.wasm`: generated artifact
