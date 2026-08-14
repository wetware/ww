# std

Everything in `std/` ships in the `ww` namespace. If it's a WASM cell,
a Glia module, or the guest SDK, it goes here.

## Layout

| Path | Role |
|------|------|
| `system/` | Guest SDK (rlib) -- connects a WASM agent to the host over WASI streams and drives Cap'n Proto RPC. All guests link against this. |
| `kernel/` | Default Rust kernel (pid0) -- directly installs the shipped `/status` composition without Glia. The host embeds and publishes this component. |
| `kernel-glia/` | Legacy Glia kernel (pid0) -- runs init.d and re-exports attenuated capabilities. Select this component explicitly for rollback and Glia-based examples. |
| `caps/`   | Capability handlers (rlib) -- shared Cap'n Proto dispatch logic for guest cells. |
| `lib/ww/` | Glia standard library -- `.glia` source files that ship at `/lib/ww/` in the namespace tree. |

## Convention

Each cell builds to `bin/main.wasm` (or `bin/<name>.wasm`) inside its directory.
Build artifacts are gitignored, not committed.

```bash
make kernel       # builds the default std/kernel/bin/main.wasm
make kernel-glia  # builds the legacy std/kernel-glia/bin/main.wasm
make status       # builds std/status/bin/status.wasm
make std          # builds both kernels + status
```

Select the legacy Glia kernel through the existing explicit source control:

```bash
ww run --kernel file:std/kernel-glia/bin/main.wasm std/kernel-glia
```

## vs crates/

`std/` = content that ships in the namespace (targets `wasm32-wasip2` or is Glia source).
`crates/` = Rust libraries consumed by the host binary or shared between host and guests.
