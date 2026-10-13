# Wasmtime dependency patch

Wetware pins `wasmtime`, `wasmtime-wasi`, and `wasmtime-wasi-io` 48.0.1 from
[wetware/wasmtime](https://github.com/wetware/wasmtime) at
`fe367749da9584f974b516f28c5c8055861d1512` (tree
`b754538681a92382a051185fcf1d548449b07273`). This v48 filesystem-lifecycle
backport is a direct child of the previous exact pin,
`a5fb020948a771aad3aa78dfb983caf3dfbf8f98`, which merged
[wetware/wasmtime#1](https://github.com/wetware/wasmtime/pull/1) on the upstream
`ww/wasmtime-48.0.1` maintenance branch.

The backport exposes two separate lifecycle primitives:

- `FilesystemOperation` is the optional synchronous admission hook for active
  native filesystem work. Successful admission returns an opaque operation
  owner that Wasmtime retains through the admitted work and native-result
  cleanup. Clones share one admission. Rejection maps to P1 `again`, P2
  `would-block`, and P3 `busy`; `is-same-object` preserves the typed rejection
  as a trap. Completed but unpolled P2 readiness results release ownership only
  after native work and file-clone cleanup finish.
- `FilesystemHandle` carries native `File` or `Dir` authority together with an
  opaque owner for the backing content. Wasmtime preserves that owner through
  descriptor clones, native workers, P2 streams, P3 producers, child
  descriptors, directory iterators, and other native-handle derivatives. An
  idle descriptor therefore retains its backing content without consuming an
  active `FilesystemOperation` admission.

Wetware uses `FilesystemOperation` for bounded active work and
`FilesystemHandle` for staged CID-generation lifetime. These owners must remain
separate: completion of an operation does not imply that a returned native
handle, or its backing content, is no longer live.

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
