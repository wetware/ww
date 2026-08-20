# Image Layout

Each wetware image follows a minimal FHS convention:

```
<image>/
  bin/
    main.wasm          # conventional application entrypoint
    status.wasm        # status component used by the shipped Rust PID0
  svc/                 # optional application content; not composed automatically
```

The Host selects PID0 independently from the effective application root. The
shipped Rust PID0 consumes only `bin/status.wasm` from that root. An
application-specific composition can select `bin/main.wasm`; the shipped Rust
PID0 does not select it automatically.

## Rust PID0 boot flow

The shipped Rust PID0 reads `bin/status.wasm`, loads the component, registers
`/status`, grants `host`, and commits kernel readiness. The Rust PID0 does not
evaluate an init directory.

## Mount sources

Mounts can be local paths or IPFS paths. Multiple mounts are merged
as layers (later mounts override earlier ones):

| Form | Example |
|------|---------|
| Local path | `std/status` |
| IPFS path | `/ipfs/QmAbc123...` |
| Layered | `ww run /ipfs/QmBase my-overlay` |

Targeted mounts (`source:/guest/path`) are not accepted by backend virtual mode.

## On-chain coordination

The `--stem` flag connects to an Atom contract on an EVM chain.
The contract holds a monotonic head pointer (an IPFS CID). When the
head is updated:

1. The Atom Source reads the chain tip.
2. It reads `Atom.head()` at `tip - confirmation_depth`.
3. Deployment advances its host-local epoch and publishes `root: None`.
4. Deployment terminates PID0 while it prepares the head plus frozen layers.
5. Deployment waits for teardown, swaps `CidTree`, publishes the rooted epoch,
   and starts the replacement.

Contract events are not required for correctness. The boot head and later
updates use the same finalized-depth rule. Atom's contract sequence remains
private to the Source and does not become `Epoch.seq`.

This provides a coordination primitive across trust boundaries:
multiple independent nodes watching the same contract will
synchronize their agent lifecycle to the same on-chain state.
