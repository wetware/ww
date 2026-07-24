# Guest Crates

Zero-dependency (or near-zero) crates intended for use from WASM guests
(`wasm32-wasip2`). Keep these lightweight — no capnp-rpc, no tokio, no
host-only deps.

Host-side RPC servers that consume these primitives live elsewhere
(e.g. `crates/authority`).
