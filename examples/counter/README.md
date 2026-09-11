# Counter -- WAGI Cell

A WAGI guest that handles FastCGI records over WASI stdin and stdout.

## What it demonstrates

- `WW_CELL_MODE=http`
- FastCGI request and response framing
- one guest process per HTTP request
- GET and POST counter behavior

## Build

```sh
rustup toolchain install nightly-2026-08-30 --component rust-src
make counter
```

The build produces `examples/counter/bin/counter.wasm`.

## Runtime composition status

The repository keeps the Rust guest as a buildable WAGI reference. The Rust
PID0 installs only `/status`; no counter route composition is currently
shipped.

## Files

- `src/lib.rs`: guest implementation
- `Makefile`: WASM build
- `bin/counter.wasm`: generated artifact
