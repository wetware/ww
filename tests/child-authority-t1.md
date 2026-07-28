# T1 child-authority harness

The real-WASM probe lives at `tests/fixtures/authority-probe`. It emits one
small JSON line per focused probe. Ordinary tests run current characterization
and the Cap'n Proto fork gate. The intentionally failing confinement tests are
isolated from CI:

```sh
cargo test --test child_authority_confinement t1_expected_red -- --ignored --nocapture --test-threads=1
```

Each ignored test names the current leak it proves. Do not mark these cases
green until the production construction path removes the corresponding
authority.

## Layering and blocked cases

| Case | T1 state |
|---|---|
| Empty-grant guest enumeration and concrete core-cap calls | Expected red now |
| Repeated current `graft()` name set | Passing characterization |
| Same server under two names, two deliveries | Passing hard gate; exact fork revision asserted |
| Empty/malformed/duplicate `caps` wire names | Expected red now |
| Arbitrary unexported strings | Passing characterization; strings are not authority |
| Restricted Executor descendant amplification | Expected red now |
| No-epoch/no-stream usable raw `Host` | Expected red now |
| Args/env/stdio and clock/randomness | Passing characterization |
| Mounted image-rooted filesystem, known-CID read, scratch isolation | Blocked: current `ExecutorImpl` supplies neither `CidTree` nor cache mode |
| CID enumeration and `/ipfs` mutation absence | Current-negative characterization only |
| CAS size/concurrency/fetch/cache pressure | Blocked on the T6 substrate/CAS fixture and measurable cache wiring |
| `InitialAuthorityRecord`, exact record delivery, shared encoder | Blocked on T2 |
| Grants-only bootstrap surface/no `graft()` | Blocked on T5 |
| Glia `:grants`, source duplicate diagnostics, lexical-capture removal | Blocked on T4 |

The wire duplicate test deliberately says nothing about Glia map literals:
Glia's ordinary map evaluation normalizes through `im::HashMap`, so source
duplicate detection belongs at parse/analysis time in T4.

## Ambient-graft migration inventory

Host/pid0 surfaces that retain grafting and must **not** be mechanically
migrated as ordinary children:

- `std/kernel/src/lib.rs`: `KernelBootstrap` and pid0's initial graft. The
  corrected Autoplan addendum explicitly keeps this pid0/shell export surface.
- `src/executor.rs`, `src/cli/main.rs`, and `src/cli/shell.rs`: pid0/daemon and
  remote shell bootstrap consumers.
- `std/shell/src/lib.rs`: shell-side consumption of the pid0-exported membrane.

Ordinary guest graft consumers that need named grants in T5/T8:

- `std/status/src/lib.rs`: status cell obtains `host`.
- `examples/oracle/src/lib.rs`: HTTP cell obtains `http-client`; serve and
  consume modes obtain `host`/routing/network capabilities.
- `examples/discovery/src/lib.rs`: service mode obtains `host` and `routing`.
- `examples/chess/src/lib.rs`: service mode obtains host/routing/network
  authority.
- Their README snippets and architecture/capability docs currently teach
  universal `membrane.graft()` and must be updated with the later migration.

Spawner/listener sites currently relying on ambient graft or lexical capture:

- `src/launcher.rs`: `ExecutorImpl::spawn` decodes the wire `caps` list and
  calls `build_membrane_rpc` for every spawned child.
- `crates/rpc/src/graft.rs`: `HostGraftBuilder` constructs the universal graft
  by inserting host/runtime/routing/authority/identity/IPFS/HTTP plus extras.
- `std/status/etc/init.d/05-status.glia` and
  `tests/status_cell_e2e.rs` / `tests/status_cell_http_listener_e2e.rs`; these
  positively depend on ambient `host` today and must receive it explicitly.
- `examples/oracle/glia/register.glia`, `serve.glia`, and `consume.glia`.
- `examples/discovery/glia/serve.glia`.
- `examples/chess/glia/serve.glia`.
- `examples/counter/glia/register.glia`,
  `examples/snap-hello-rs/glia/register.glia`, and
  `examples/echo/glia/register.glia` currently omit grants; these should be
  reviewed and made explicitly zero-grant if they need only substrate.
- Direct `Executor.spawn` flows in the chess, discovery, and oracle registration
  scripts should explicitly pass zero grants unless their default cell mode
  grows an authority dependency.
- `crates/glia/src/eval.rs` has two `cell` paths that call
  `env.collect_caps()`.
- `std/kernel/src/lib.rs` has both `cell` spawn encoders and the direct
  `runtime :run` spawn path, plus HTTP- and stream-listener grant encoders.
- `crates/rpc/src/http_listener.rs` and
  `crates/rpc/src/stream_listener.rs` replay captured templates per child.
- `src/dispatcher/mod.rs` is the WAGI dispatcher spawn site and currently
  receives the same universal child graft.
- Direct zero-cap spawn sites in `tests/discovery_integration.rs`,
  `tests/stdin_shutdown_integration.rs`, `tests/shell_e2e.rs`,
  `tests/runtime_spike_test.rs`, and `examples/echo_handler_e2e.rs` do not need
  node authority for their stated behavior. They still exercise the ambient
  construction path today and should become explicit zero-grant
  characterization after T3/T5.
- `crates/rpc/src/http_listener.rs` and `stream_listener.rs` unit tests, plus
  `std/kernel/src/lib.rs` listener/cell tests, must assert exact template
  forwarding once the shared encoder lands rather than relying on the
  universal graft to mask omissions.

This inventory is documentation only. T1 performs no migration.
