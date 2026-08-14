# Image Layout

Each wetware image follows a minimal FHS convention:

```
<image>/
  bin/
    main.wasm          # agent entrypoint (required)
  svc/                 # nested service images (spawned by pid0)
  etc/                 # configuration (consumed by pid0)
    init.d/            # boot scripts evaluated by the legacy Glia kernel
```

Only `bin/main.wasm` is required. Everything else is convention
between the image author and the kernel (pid0).

## Default and legacy Glia boot flow

- **Default Rust kernel:** directly installs the shipped `/status`
  composition. The Rust kernel does not evaluate `etc/init.d`.
- **Legacy Glia workflow:** run the node with the explicit Glia kernel, attach
  with `ww shell`, and load Glia snippets from the example directory.
- **Legacy Glia deployment:** bake service wiring into `etc/init.d/*.glia`
  so the Glia kernel registers services at boot.

Build and select the legacy kernel explicitly:

```sh
make kernel-glia
ww run --kernel file:std/kernel-glia/bin/main.wasm std/kernel-glia
```

## Mount sources

Mounts can be local paths or IPFS paths. Multiple mounts are merged
as layers (later mounts override earlier ones):

| Form | Example |
|------|---------|
| Local path | `std/kernel-glia` |
| IPFS path | `/ipfs/QmAbc123...` |
| Layered | `ww run /ipfs/QmBase my-overlay` |

Targeted mounts (`source:/guest/path`) are not accepted by backend virtual mode.

## On-chain coordination

The `--stem` flag connects to an Atom contract on an EVM chain.
The contract holds a monotonic head pointer (an IPFS CID). When the
head is updated:

1. The off-chain indexer detects the `HeadUpdated` event
2. Waits for confirmation depth (reorg safety)
3. Advances the epoch, revoking all agent capabilities
4. The host terminates pid0 and starts a replacement for the new epoch

This provides a coordination primitive across trust boundaries:
multiple independent nodes watching the same contract will
synchronize their agent lifecycle to the same on-chain state.
