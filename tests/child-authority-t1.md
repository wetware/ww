# T1 child-authority harness

The real-WASM probe lives at `tests/fixtures/authority-probe`. It emits one
small JSON line per focused probe. Ordinary tests run current characterization,
the closed T3 confinement regressions, and the Cap'n Proto fork gate. The two
intentionally failing cross-tranche tests are isolated from CI:

```sh
cargo test --test child_authority_confinement t1_expected_red -- --ignored --nocapture --test-threads=1
```

The former T4 expected-red test is now a normal green regression covering both
Glia evaluator paths. The remaining T5 expected-red test covers the temporary
child-side `Membrane.graft()` compatibility interface used to deliver an exact
`InitialAuthorityRecord`.

## Layering and blocked cases

| Case | T1 state |
|---|---|
| Empty-grant guest enumeration and concrete core-cap calls | Passing T3 regression |
| Repeated current `graft()` name set | Passing characterization |
| Same server under two names, two deliveries | Passing hard gate; exact fork revision asserted |
| Empty/duplicate `caps` wire names | Passing T3 regression |
| Path-like opaque wire label (`bad/name`) | Passing valid-name regression |
| Arbitrary unexported strings | Passing characterization; strings are not authority |
| Restricted Executor descendant amplification | Passing T3 regression |
| No-epoch/no-stream usable raw `Host` | Passing T3 regression |
| Args/env/stdio and clock/randomness | Passing characterization |
| Mounted image-rooted filesystem, known-CID read, scratch isolation | Blocked: current `ExecutorImpl` supplies neither `CidTree` nor cache mode |
| CID enumeration and `/ipfs` mutation absence | Current-negative characterization only |
| CAS size/concurrency/fetch/cache pressure | Blocked on the T6 substrate/CAS fixture and measurable cache wiring |
| `InitialAuthorityRecord`, exact record delivery, shared encoder | Passing T3 regression |
| Grants-only bootstrap surface/no `graft()` | Expected red; owned by T5 |
| Glia `:grants`, source duplicate diagnostics, lexical-capture removal | Passing T4 regression |

The wire duplicate test deliberately says nothing about Glia map literals:
Glia's ordinary map evaluation normalizes through `im::HashMap`, so source
duplicate detection belongs at parse/analysis time in T4.

## Post-T3 migration inventory

Host/pid0 surfaces that retain grafting and must **not** be mechanically
migrated as ordinary children:

- `std/kernel/src/lib.rs`: `KernelBootstrap` and pid0's initial graft. The
  corrected Autoplan addendum explicitly keeps this pid0/shell export surface.
- `src/executor.rs`, `src/cli/main.rs`, and `src/cli/shell.rs`: pid0/daemon and
  remote shell bootstrap consumers.
- `std/shell/src/lib.rs`: shell-side consumption of the pid0-exported membrane.

Ordinary children now receive exactly their requested named grants through
`InitialAuthorityRecord`; they do not receive a universal host graft. The
temporary compatibility bootstrap still implements `Membrane.graft()`, so the
following guest consumers remain migration targets for T5:

- `std/status/src/lib.rs`: status cell obtains `host`.
- `examples/oracle/src/lib.rs`: HTTP cell obtains `http-client`; serve and
  consume modes obtain `host`/routing/network capabilities.
- `examples/discovery/src/lib.rs`: service mode obtains `host` and `routing`.
- `examples/chess/src/lib.rs`: service mode obtains host/routing/network
  authority.
- Their README snippets and architecture/capability docs must move from
  `membrane.graft()` to the T5 grants-only interface with the guest migration.

Spawner/listener state after T3:

- `src/launcher.rs`: `ExecutorImpl::spawn` validates the wire `caps` list into
  `InitialAuthorityRecord` before process construction and builds only the
  bounded child bootstrap.
- `crates/rpc/src/graft.rs`: `HostGraftBuilder` constructs the universal graft
  only for trusted pid0 by inserting
  host/runtime/routing/authority/identity/IPFS/HTTP plus extras.
- `std/status/etc/init.d/05-status.glia` and
  `tests/status_cell_e2e.rs` / `tests/status_cell_http_listener_e2e.rs`
  explicitly pass the `host` grant needed by status children.
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
- `crates/glia/src/eval.rs` now routes both `cell` paths through one explicit
  grant validator; neither path captures lexical capabilities.
- `std/kernel/src/lib.rs` has both `cell` spawn encoders and the direct
  `runtime :run` spawn path, plus HTTP- and stream-listener grant encoders.
- `crates/rpc/src/http_listener.rs` and
  `crates/rpc/src/stream_listener.rs` decode registration grants once and replay
  immutable templates per child.
- `src/dispatcher/mod.rs` is the WAGI dispatcher spawn site and receives the
  bounded executor rather than a universal child graft.
- Direct zero-cap spawn sites in `tests/discovery_integration.rs`,
  `tests/stdin_shutdown_integration.rs`, `tests/shell_e2e.rs`,
  `tests/runtime_spike_test.rs`, and `examples/echo_handler_e2e.rs` do not need
  node authority for their stated behavior and now exercise explicit zero-grant
  construction.
- HTTP and stream listener unit tests assert that repeated children receive the
  same fixed registration template.

The remaining production migrations in this inventory belong to T4/T5 and are
deliberately outside T3.
