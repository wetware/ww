# Echo Cell

A minimal WASI stdin/stdout echo guest for integration testing.

## What it demonstrates

- raw-cell behavior with `WW_CELL_MODE=raw`
- WASI P3 `wasi:cli/run@0.3.0`
- byte-for-byte stdin/stdout forwarding
- spawn, pipe, and process collection
- the runtime-free `sync-command` world

## Build

```sh
rustup toolchain install nightly-2026-08-30 --component rust-src
make echo
```

The build produces `examples/echo/bin/echo.wasm`.

Echo uses `std/system`'s `sync-command` WIT world. This world exports the P3
command entry point and imports stdout explicitly. The compiled Rust standard
library adds the remaining CLI and clock interfaces listed in the echo target's
build allowlist. Echo does not import `wetware:transport`, construct a
`capnp_rpc::RpcSystem`, or link the async guest runtime.

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
