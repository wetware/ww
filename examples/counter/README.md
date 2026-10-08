# Counter -- WAGI Cell

A WAGI guest that receives CGI metadata and a request body through WASI, then
writes a CGI response to stdout.

## What it demonstrates

- CGI request metadata and response formatting
- awaited P3 stdout and host-writer flush completion
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

The handler implements `wagi_guest::AsyncGuest` and exports with
`wagi_guest::export_async!`. It awaits `respond_bytes_async` so the root cannot
report success before stdout accepts and flushes the complete response.

## Files

- `src/lib.rs`: guest implementation
- `Makefile`: WASM build
- `bin/counter.wasm`: generated artifact
