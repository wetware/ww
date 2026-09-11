# Chess Demo

## Build and run

```sh
# Build the chess example.
make kernel chess
# Play the match: spawns two nodes, they discover each other, then play a game over Cap'n Proto RPC.
cargo run -p chess --bin play_match -- game.pgn
# Visualize the game.
cargo run -p chess --bin view_match -- game.pgn
```
