# Wasmtime dependency patch

Wetware pins `wasmtime`, `wasmtime-wasi`, and `wasmtime-wasi-io` 48.0.1 from
[wetware/wasmtime](https://github.com/wetware/wasmtime) at
`a5fb020948a771aad3aa78dfb983caf3dfbf8f98`. This is the merge commit for
[wetware/wasmtime#1](https://github.com/wetware/wasmtime/pull/1), based on the
upstream `ww/wasmtime-48.0.1` maintenance branch.

The patch adds an optional synchronous filesystem-operation hook. Successful
admission returns an opaque owner that Wasmtime retains through native work,
native resource disposal, and active P3 stream sessions. Clones share one
admission. Rejection maps to P1 `again`, P2 `would-block`, and P3 `busy`;
`is-same-object` preserves the typed rejection as a trap. Completed but
unpolled P2 readiness results release ownership after their native work and
file-clone cleanup finish.

All three crates are patched to the same revision so Wasmtime types and WASI
I/O resources resolve from one coherent source graph. The standalone `fuzz`
workspace repeats the same three patches because it does not inherit the root
workspace patch table.

## Validation and removal

In the dependency checkout, run the filesystem lifecycle tests:

```sh
cargo test -p wasmtime-wasi filesystem::
cargo test -p wasmtime-wasi p3::filesystem::lifecycle_tests
```

In Wetware, verify source provenance and the host build:

```sh
cargo tree --workspace --target all | rg 'wasmtime(-wasi|-wasi-io)? v48\.0\.1'
cargo check -p cell --locked
cargo check -p ww --locked
```

Replace this pin only after an upstream release exposes equivalent lifecycle
semantics and passes the native P3 lifetime, rejection, resource-table failure,
and completed-unpolled P2 regressions. Update the three patch entries in both
`Cargo.toml` and `fuzz/Cargo.toml`, both `Cargo.lock` and `fuzz/Cargo.lock`, and
this document together.
