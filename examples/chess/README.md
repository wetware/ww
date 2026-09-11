# Chess Demo

## Build and run

```sh
rustup toolchain install nightly-2026-08-30 --component rust-src
make chess

# Play the match: spawns two nodes, they discover each other, then play a game over Cap'n Proto RPC.
cargo run -p chess --bin play_match -- game.pgn

# Visualize the game.
cargo run -p chess --bin view_match -- game.pgn
```
