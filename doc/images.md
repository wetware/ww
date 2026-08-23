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
| IPNS path | `/ipns/k51qzi5uqu5...` |
| Layered | `ww run /ipfs/QmBase my-overlay` |

Targeted mounts (`source:/guest/path`) are not accepted by backend virtual mode.
An `/ipns/...` mount is an image layer that Kubo resolves during composition.
It is distinct from `--ipns-stem`, which follows signed changes and replaces
the active deployment generation.

## Installed host layout

`ww perform install` and `ww perform update` separate private host state from
the local image root:

```
~/.ww/
  identity              # private node identity
  etc/ns/               # host-only namespace configuration
  logs/                  # host logs
  run/                   # host runtime state
  ipns/
    follow/*.record      # durable signed follower ordering floors
    publish/*.record     # durable signed records for the default publisher
  fhs/                   # publishable local image root
    bin/status.wasm      # status component used by the shipped Rust PID0
```

The generated service recursively imports only `~/.ww/fhs` and explicit image
arguments. The service reads `~/.ww/etc/ns` through a separate host-only
configuration argument. The service does not mount `~/.ww` as an image.

Put intentional local image content under `~/.ww/fhs`, or pass an explicit
image path to `ww daemon install`. Do not put private host state in an image
root because Kubo can store, pin, and provide every imported file.

## Stem coordination

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

`--ipns-stem <name>` selects an IPNS record instead of an Atom contract. The
two flags are mutually exclusive. Wetware fetches the raw signed record through
Kubo HTTP Routing V1 and validates it locally. A valid new `/ipfs/<cid>` value
uses the same revoke, prepare, swap, and replacement path described above.

The signed record's EOL is authoritative. Transport failures before EOL keep
the current image active. EOL without a valid replacement emits
`InvalidHead`, removes the rooted epoch, and terminates PID0. An expired
persisted value does not seed boot, although its signed ordering floor remains
durable. See [deployment.md](deployment.md#ipns-stem) for ordering and recovery
semantics.
