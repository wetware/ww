# Wasmtime dependency patch

Wetware pins `wasmtime`, `wasmtime-wasi`, and `wasmtime-wasi-io` 48.0.1 from
[wetware/wasmtime](https://github.com/wetware/wasmtime) at
`96e45d8b025aac7a5aea5aa505bd73826996919d`. This descriptor-factory
backport is a direct child of the previous exact pin,
`fe367749da9584f974b516f28c5c8055861d1512`, on the fork's
`backport/v48-descriptor-factories` branch. Wasmtime remains 48.0.1.

The backport separates backing-content retention from operation admission:

- Each descriptor retains its attached operation factory. Wetware captures the
  actual `StagedContentLease` and the descriptor's `CellResources`/`FsScope` in
  that factory. Idle descriptors retain their generation without active admission.
- Each successful factory call returns a `FilesystemOperation` containing both
  a generation lease clone and the existing admission receipt. Workers retain
  these owners through native work and native-result cleanup, including after
  cancellation or Store destruction. Secondary operands retain their complete
  descriptors without a second admission.
- Custom opens return a native-first `PreparedDescriptor` containing the native
  descriptor and a temporary `WasiFilesystemCtx`. Installation calls
  `push_descriptor` inside `FsResultEnvelope::install`. The temporary context
  drops after installation; abandoned or rejected results dispose native state
  before backing ownership. No long-lived context accumulates generation leases.
- Children and streams inherit the descriptor's factory. Replacing the context
  hook does not change factories on existing descriptors. Shared admission state
  still permits later rejection. Custom descriptors must use `push_descriptor`;
  bare `ResourceTable::push` does not attach a factory.

`FilesystemHandle` is removed. Native fields are `Arc<std::fs::File>` again.
Native extraction no longer retains external backing ownership automatically.
A supported escaping duplicate must pair its native file with an unchanged
attached `Descriptor` or an explicit generation lease, declared after the native
field so native state drops first. Wetware's preserving-duplicate regression uses
this pair. Locally borrowed native reads keep their source descriptor alive.
Arbitrary raw OS-handle duplication remains outside the ownership contract.

`FilesystemOperation` still carries `LogicalIoPermit`,
`ForegroundProducerPermit`, and `FsWorkToken`. Rejection remains P1 `again`, P2
`would-block`, and P3 `busy`; `is-same-object` preserves typed rejection as a trap.
Completed inert results release admission after native work ends; resource results
retain ownership through installation or disposal. Terminal streams can retain
backing content while their native descriptor remains live, without retaining
active admission.

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

## Consumer qualification boundary

The consumer regressions cover actual shared-generation child inheritance,
explicit preserving native duplication, abandoned prepared results, table-full
installation, independent-generation cleanup, admission and cancellation.
The native-P3 fixture exercises the production interceptor with isolated staging.
A single guest witness combining a shared generation, gated running Wasmtime
worker, eviction, Store destruction, and final cleanup remains unavailable:
Wasmtime's worker gate is private to its unit tests. The fork qualification and
consumer tests cover those boundaries separately; they do not constitute that
combined witness. No new worker instrumentation or fork changes are included.
