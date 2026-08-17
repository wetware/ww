# std

`std/` contains the Rust PID0, standard guest components, and the guest SDK.

## Layout

| Path | Role |
|------|------|
| `system/` | Guest SDK (rlib) -- connects a WASM agent to the host over WASI streams and drives Cap'n Proto RPC. All guests link against this. |
| `kernel/` | Rust kernel (pid0) -- directly installs the shipped `/status` composition. The host embeds and publishes this component. |
| `status/` | Standard `/status` guest component loaded by Rust PID0 with an explicit `host` grant. |

## Convention

Each cell builds to `bin/main.wasm` (or `bin/<name>.wasm`) inside its directory.
Build artifacts are gitignored, not committed.

```bash
make kernel       # builds the default std/kernel/bin/main.wasm
make status       # builds std/status/bin/status.wasm
make std          # builds kernel + status
```

## vs crates/

`std/` = the Rust PID0, standard guest components, and the guest SDK.
`crates/` = Rust libraries consumed by the host binary or shared between host and guests.
