# Deployment

`wetware/ww` publishes immutable artifacts. The separate `wetware/infra`
repository decides which image digest runs in the cluster. A merge to `ww`
master is therefore a release build, not a production deployment.

## Artifact publication

```text
ww master commit
       |
       +--> host build matrix ----------> downloadable host binaries
       |
       +--> WASM build -----------------> kernel/shell/status artifacts
       |                                      |
       +--> deploy image assembly <------------+
       |        |
       |        +--> ghcr.io/wetware/ww:master-<git-sha>
       |        +--> ghcr.io/wetware/ww@sha256:<digest>
       |
       +--> IPFS release tree ----------> immutable CID
                                            |
                                            +--> ww-release IPNS
```

The SHA-tagged GHCR image and its immutable digest identify the same image.
IPNS is mutable discovery for an otherwise content-addressed release tree. Its
publisher uses Git ancestry plus compare-and-set checks so an older workflow
cannot supersede a newer published revision.

The release name is
`k51qzi5uqu5dg9eci41ad4b1wyf9kocngntfviq12qjuvusra3nt94xlx98me1`.

IPFS remains a required publication path while `scripts/install.sh` installs
from IPNS. It must not be made non-blocking until a GitHub Release has been
published and the installer has migrated to that distribution path.

## Manual image promotion

Production promotion is intentionally declarative:

```text
GHCR immutable digest
       |
       v
wetware/infra k8s/ww-master/kustomization.yaml
       |
       v
review + merge release-labelled infra PR
       |
       v
make -C k8s/ww-master up
       |
       +--> capacity/storage preflight
       +--> kubectl apply -k
       +--> rollout status
       +--> public /status check
```

The `images[].digest` field in the infra Kustomization is the sole promotion
field. Never promote the mutable `:master` tag. Merging an infra PR records the
desired deployment in Git; an operator must still run the guarded `make up`
target to apply it.

After rollout, verify the runtime identity from a `wetware/ww` checkout:

```sh
WW_EXPECTED_IMAGE_DIGEST=sha256:<digest> \
WW_EXPECTED_GIT_SHA=<40-character-sha> \
scripts/deploy_verify.sh
```

The verifier checks the selected Ready pod's Kubernetes
`status.containerStatuses[].imageID` against the immutable digest. It then
executes `ww healthcheck --ready --expect-git-sha ...` inside the distroless
container. The image check deliberately uses Kubernetes as the authority:
containers cannot intrinsically discover the registry digest selected by the
runtime.

## Runtime layers

The production image contains two core layers:

```text
/usr/share/wetware/kernel/
  bin/main.wasm
  bin/status.wasm
  etc/init.d/05-status.glia

/usr/share/wetware/shell/
  bin/shell.wasm
```

`ww run` merges root layers left-to-right. Optional IPNS application content
comes first; kernel and shell layers follow so application content cannot
replace pid0 or the image-owned status route.

## HTTP request path

```text
client
  |
  v
Traefik / Ingress
  |
  v
ww-master :2080 (WAGI adapter)
  |
  v
longest registered route prefix
  |
  v
cell registered by etc/init.d/*.glia
```

The public `/status` cell route is an end-to-end serving check. The
localhost-only admin plane on `127.0.0.1:2026` is the process control surface:

- `/healthz` confirms the admin server is accepting requests.
- `/readyz` reports whether the runtime has reached its serving phase.
- `/version` reports source and embedded-artifact provenance plus degraded
  cache state.
- `/metrics` exposes host/runtime counters.

Keep the admin listener on loopback unless an authenticated network boundary
is added; these endpoints are intentionally unauthenticated.

Kubo TCP connection attempts are bounded to five seconds. The small local `/api/v0/id`
readiness probe is additionally bounded to 30 seconds, so a listener that
accepts connections but fails to answer cannot wedge the startup wait loop.
Bulk content transfer and ordinary runtime DHT operations deliberately do not
inherit that 30-second deadline: they can make legitimate progress for longer
than a readiness interval.

`ww run` uses a 120-second Kubo wait by default so a local development
invocation fails clearly when Kubo is absent. A production deployment that
must survive a sustained Kubo outage must set `WW_KUBO_WAIT_MAX_SECS=0`; the
reviewed `ww-master` manifest does so. This keeps `/healthz` available while
`/readyz` remains closed until a live current-epoch route is registered and
trusted PID0 has committed that generation after init/init.d. The commit uses a
private Wasm host import installed only for PID0; it is not a Cap'n Proto
capability and cannot be delegated over the network. Readiness is not a
continuous Kubo availability probe after startup; liveness remains the
process-level signal during a later dependency outage.

After Kubo identity succeeds, every boot-only Kubo API call used for namespace
and mount resolution has a separate 90-second no-progress watchdog. Override
it with `WW_KUBO_BOOT_OPERATION_TIMEOUT_SECS`; it must be a positive number
because disabling it would reintroduce an unbounded boot hang.
`WW_KUBO_BOOT_RETRY_MAX_SECS` independently bounds how long a
retryable boot call (Kubo transport failure, HTTP 429, or HTTP 502/503/504)
may retry;
its development default is 120 seconds and `0` retries indefinitely. A bad
local mount directory and Kubo 4xx response other than 429 fail immediately rather than
becoming a retry loop. The watchdog is deliberately scoped to individual API
calls, not an entire image merge, so a slow valid multi-layer merge can make
progress. `/healthz` remains available and `/readyz` remains closed while a
mandatory mount call retries. Namespace fallback to its bootstrap CID marks
the runtime degraded. This is not a global HTTP timeout: content reads and
DHT activity after startup remain unbounded by it. Initial-head and namespace
pins are best-effort, so a failed or stalled pin is logged and does not delay
serving.

Related references: [architecture](architecture.md),
[capability model](capabilities.md), and [CLI](cli.md).
