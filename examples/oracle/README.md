# Oracle -- Gas Price Feed

A gas-price guest with typed vat RPC and HTTP/WAGI execution modes.

## What it demonstrates

- Cap'n Proto vat RPC with `WW_CELL_MODE=vat`
- WAGI with `WW_CELL_MODE=http`
- outbound requests through an explicit `HttpClient` grant
- DHT provider and consumer modes
- cache and JSON response behavior

## Build

```sh
rustup target add wasm32-wasip2
make oracle
```

The build produces `examples/oracle/bin/oracle.wasm`.

## Runtime composition status

The repository keeps the Rust guest and its direct unit and RPC tests. The Rust
PID0 installs only `/status`; no oracle route or DHT service composition is
currently shipped.

## Tests

```sh
cargo test --manifest-path examples/oracle/Cargo.toml
```

## Files

- `oracle.capnp`: PriceOracle schema
- `src/lib.rs`: guest implementation
- `Makefile`: WASM build and test targets
