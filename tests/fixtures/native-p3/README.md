# Native WASI P3 fixture

This isolated fixture exercises the production P3 host adapters. Its
async exports cover ordered transport, bounded backpressure, delayed flush,
half-close, abnormal failure, `Store::run_concurrent` owner abort, and CidTree
filesystem policy. The guest contains no Tokio, custom scheduler, `PollSet`,
parker, Cap'n Proto RPC, or first-party application composition.

The build lane pins this tuple:

- Rust `nightly-2026-08-30` with LLVM 23.1.0;
- `rust-src` and `-Z build-std=std,panic_abort`;
- Rust target `wasm32-wasip3`;
- WASI SDK 34.0;
- `wasm-component-ld` 0.5.30 with LLD 23.1.0;
- `wit-bindgen` 0.61.1;
- `wasm-tools` 1.258.0;
- Wasmtime 48.0.1 in the host workspace.

WASI SDK 34 bundles the pinned `wasm-component-ld` and `wasm-ld`. Download
[WASI SDK 34](https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34)
and [wasm-tools 1.258.0](https://github.com/bytecodealliance/wasm-tools/releases/tag/v1.258.0).
Set `WASI_SDK_PATH` to the extracted SDK directory. Set `WASM_TOOLS` to the
`wasm-tools` executable when the executable is not on `PATH`.

Install the pinned Rust toolchain:

```bash
rustup toolchain install nightly-2026-08-30 \
  --profile minimal \
  --component rust-src
```

Build and validate the fixture from the repository root:

```bash
WASI_SDK_PATH=/path/to/wasi-sdk-34.0-<arch>-<os> \
WASM_TOOLS=/path/to/wasm-tools \
make test-p3-fixture
```

The command builds
`target/native-p3-fixture/wasm32-wasip3/release/native_p3_fixture.wasm`.
The command then runs `wasm-tools validate --features all`, prints the
component WIT, and rejects every WASI import that is not version `0.3.x`.
The command also rejects WASI socket imports and interfaces outside the
fixture-specific allowlist. It then runs the ignored host artifact tests with
`WW_NATIVE_P3_FIXTURE` set to the validated component.

The fixture uses real P3 streams, futures, clocks, and filesystem calls. It
isolates the host adapters from the production Cap'n Proto guest session.
Production RPC concurrency, transport-failure propagation, and cancellation
coverage live in the ordinary Cell integration tests.

Do not run `rustup target add wasm32-wasip3`. Rustup does not distribute the
Tier 3 target's standard library. This lane builds the standard library from
the pinned nightly's `rust-src` component.
