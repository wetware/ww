# Discovery -- Greeter RPC

A Greeter guest for named vat RPC and service-name DHT discovery.

## What it demonstrates

- Cap'n Proto vat RPC with `WW_CELL_MODE=vat`
- explicit ungated `VatListener.serveRaw` publication for this fixture
- typed `VatClient` dialing
- `routing.provide()` and `findProviders()`
- service and consumer execution modes

## Build

```sh
rustup target add wasm32-wasip2
make discovery
```

The build produces `examples/discovery/bin/discovery.wasm`.

## Runtime composition status

The repository keeps the Rust guest and its in-memory RPC coverage. The Rust
PID0 installs only `/status`; no discovery deployment composition is
currently shipped.

## Tests

```sh
cargo test -p discovery
```

## Files

- `greeter.capnp`: Greeter schema
- `src/lib.rs`: guest implementation
- `Makefile`: WASM build
