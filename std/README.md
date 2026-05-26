# std

Everything in `std/` ships in the `ww` namespace. If it's a WASM cell,
a Glia module, or the guest SDK, it goes here.

## Layout

| Path | Role |
|------|------|
| `system/` | Guest SDK (rlib) -- connects a WASM agent to the host over WASI streams and drives Cap'n Proto RPC. All guests link against this. |
| `kernel/` | Init agent (pid0) -- grafts onto the host Membrane, runs init.d, re-exports attenuated capabilities to peers. |
| `shell/`  | Interactive Glia shell -- REPL cell for live capability exploration. |
| `caps/`   | Capability handlers (rlib) -- shared Cap'n Proto dispatch logic for guest cells. |
| `lib/ww/` | Glia standard library -- `.glia` source files that ship at `/lib/ww/` in the namespace tree. |

## Convention

Each cell builds to `bin/main.wasm` (or `bin/<name>.wasm`) inside its directory.
Build artifacts are gitignored, not committed.

```bash
make kernel    # builds std/kernel/bin/main.wasm
make shell     # builds std/shell/bin/shell.wasm
make status    # builds std/status/bin/status.wasm
make std       # builds kernel + shell + status
```

## vs crates/

`std/` = content that ships in the namespace (targets `wasm32-wasip2` or is Glia source).
`crates/` = Rust libraries consumed by the host binary or shared between host and guests.
