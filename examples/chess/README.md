# Chess Engine

Two-node chess over libp2p RPC capabilities.

## What it demonstrates

- Cap'n Proto vat RPC with `WW_CELL_MODE=vat`
- authenticated `VatListener` publication
- typed `VatClient` calls
- service-name DHT discovery
- distinct Reader and Player method authority over one game

## Build

```sh
rustup target add wasm32-wasip2
make chess
```

The build produces `examples/chess/bin/chess-demo.wasm`.

## Authority proof

```sh
cargo run -p chess --example authority_proof
```

The proof starts two real Wetware libp2p hosts. It verifies unknown-identity
rejection, Reader and Player method profiles, revocation, epoch invalidation,
and connection cleanup.

## Runtime composition status

The repository keeps the Rust guest and its direct authority proof. The Rust
PID0 installs only `/status`; no generic chess deployment composition is
currently shipped.

## Tests

```sh
cargo test -p chess --lib
cargo test -p chess --test authority_proof
cargo test -p chess direct_libp2p_terminal_enforces_chess_authority
cargo run -p chess --example authority_proof
```

## Files

- `chess.capnp`: `ChessEngine` schema
- `src/lib.rs`: guest implementation
- `src/chess_authority.rs`: typed authority profiles
- `proof/authority_proof.rs`: real-network proof runner
- `doc/replay.md`: replay log format
