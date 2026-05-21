# Image Layout

Each wetware image follows a minimal FHS convention:

```
<image>/
  bin/
    main.wasm          # agent entrypoint (required)
  svc/                 # nested service images (spawned by pid0)
  etc/                 # configuration (consumed by pid0)
    init.d/            # boot scripts evaluated by the kernel
```

Only `bin/main.wasm` is required. Everything else is convention
between the image author and the kernel (pid0).

## Mount sources

Mounts can be local paths or IPFS paths. Multiple mounts are merged
as layers (later mounts override earlier ones):

| Form | Example |
|------|---------|
| Local path | `std/kernel` |
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
4. Agents re-graft, receiving capabilities scoped to the new epoch

This provides a coordination primitive across trust boundaries:
multiple independent nodes watching the same contract will
synchronize their agent lifecycle to the same on-chain state.
