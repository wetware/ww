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

Related references: [architecture](architecture.md),
[capability model](capabilities.md), and [CLI](cli.md).
