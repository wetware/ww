# Guest Crates

Zero-dependency (or near-zero) crates intended for use from WASM guests
(`wasm32-wasip3`). Keep these lightweight — no capnp-rpc, no Tokio, no
host-only deps.

Host-side RPC servers that consume these primitives live elsewhere
(e.g. `crates/authority`).
