# Image Layout

Each wetware image follows a minimal FHS convention:

```
<image>/
  bin/
    main.wasm          # agent entrypoint (required)
  svc/                 # nested service images (spawned by pid0)
  etc/                 # configuration (consumed by pid0)
    init.glia          # top-level boot orchestration + export policy
    init.d/            # boot scripts discovered/evaluated by init.glia
```

Only `bin/main.wasm` is required. Everything else is convention
between the image author and the kernel (pid0).

Boot is fail-closed: `etc/init.glia` is required, and any parse/eval error in
`init.glia` or scripts it loads (including `init.d`) aborts boot.

`init.d` is optional. If present, ordering is lexical; use numeric prefixes for
deterministic intent, for example `00-setup.glia`, `10-http.glia`, `20-worker.glia`.

## Minimal export policy

A strict posture can export nothing:

```clojure
(load-file "/lib/init/default.glia")
{}
```

Export selected capabilities with a bare map from cap name to cap value:

```clojure
(load-file "/lib/init/default.glia")

{:host host
 :runtime runtime}
```

Recursive attenuation uses normal `attenuate` syntax on map values:

```clojure
(load-file "/lib/init/default.glia")

{:host
 (attenuate host
   :allow [:id :network]
   :returns {:network
             {:stream-dialer (attenuate :self :allow [:dial])
              :stream-listener (attenuate :self :allow [:listen])}})}
```

## Demo vs deployment boot flow

- **Demo default:** run a node process, attach with `ww shell`, then
  load explicit Glia snippets (for example `glia/register.glia`,
  `glia/serve.glia`) from the example directory.
- **Deployment default:** bake service wiring into `etc/init.d/*.glia`
  so services auto-register at boot without an interactive shell.

Use snippets for interactive walkthroughs and reproducible tutorials.
Use init scripts for packaged images and unattended startup.

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
