# Deployment

`wetware/ww` publishes immutable artifacts. The separate `wetware/infra`
repository decides which image digest runs in the cluster. A merge to `ww`
master is therefore a release build, not a production deployment.

## Runtime deployment lifecycle

The host-side `deployment` module owns runtime deployment transitions. A
deployment combines a frozen installed FHS baseline, an optional authoritative
Stem head, frozen namespace images, and ordered CLI `FrozenMount` operations.
This order applies from lowest to highest priority to every guest path.
Directory targets merge recursively; other collisions use the later source.

Before deployment starts, the CLI captures all local sources in one admitted
batch, then publishes and verifies the snapshots. Source CIDs, targets, and
retained source-root identities transfer to deployment before preparation.
The installed baseline uses the same capture and retention machinery and counts
toward the existing operation limit. Only its FHS directory enters the snapshot;
private identity, namespace credentials, records, watermarks, and logs stay outside.
Every epoch replays those same operations. Local paths and successfully frozen
names are never read again during retry, speculation, or replacement.

The initial deployment baseline starts at local epoch `0`. Accepted Source
updates during boot preparation can advance the local epoch before the first
rooted activation. Atom contract revisions and IPNS record sequences are Source
state; neither value becomes `Epoch.seq`. A host restart starts at epoch `0`
again. Static startup prepares the frozen operations through the same path,
publishes a rooted epoch `0`, and starts no Source task.

For a replacement, deployment publishes the new local epoch with `root: None`
before root preparation. This publication revokes old authority. Deployment
then prepares a `PreparedRoot` while the old `kernel::Generation` terminates.
Preparation pins the head, applies the frozen operations, pins the effective
root, and pre-warms the root without changing `CidTree`. Preparation selects
`/boot/kernel.wasm` against that explicit effective CID, verifies its bytes, and
statically validates the component with the canonical Wasmtime engine.
`PreparedKernel` carries the effective root, source-file CID, runtime CID, and
validated component alongside the prepared root's head and retention ownership.
Preparation does not execute PID0 or publish active identity.

Typed `CompositionError` failures, including target obstruction, leave the
accepted epoch unrooted. A later valid head can recover using the same frozen
operations. Missing or invalid selected paths, symlinks, malformed or incompatible
components, and oversize components are deterministic preparation failures.
Unavailable content and transport failures retain bounded retry. Unexpected
internal errors remain fatal. No failure selects embedded or previous bytes.

A valid head without `/boot/kernel.wasm` can inherit the installed kernel through
ordinary composition. Invalid or unavailable heads cannot become baseline-only
deployments. Baseline inheritance has no automatic expiration.

Deployment drains queued Source updates after preparation and teardown. A
newer update supersedes the prepared root. If the prepared root remains current,
deployment waits for old PID0 to stop, calls `CidTree::swap_root`, and publishes
`root: Some(...)` without an intervening await. The CLI then requests the next
ordinary generation through deployment. The CLI retains interactive behavior,
exit codes, signals, and teardown-timeout process policy.

Generation construction uses the prepared component for the same effective root.
The final admission fence rejects superseded candidates before guest launch or
current identity publication. `/version` does not report a prior kernel as current
while the epoch is unrooted. Runtime CID, source-file CID, effective root CID, and
authority epoch identify different objects. The persistent Host PeerId remains
unchanged. Repeated kernel bytes do not preserve prior authority or readiness.

Native compilation can continue after logical cancellation. The cancelled worker
retains its physical compilation permit until completion; obsolete results cannot
activate a deployment. No subprocess compiler or thread termination is used.

Deployment tracks frozen, active, preparation-attempt, and failed-unpin pin
ownership. Its protected set includes every frozen source and retained source
root, including sources hidden by later operations. Repeated Kubo pins do not
create independent ownership. Frozen source pins remain retained after shutdown;
failed or uncertain publication never triggers compensating unpins.

The deployment cache uses `RetainedPinner`. Cache eviction releases local
staging and residency while retaining Kubo pins that could overlap deployment
content. Retained Kubo content requires explicit operator cleanup after all users
release it. Known candidate roots still use deployment's protected-set cleanup.
Each cleanup pass shares at most 128 unpin attempts and one 30-second deadline
across queued sets. Remaining work is reported and deferred. These protections apply within the
current deployment, not across independent processes sharing a Kubo node.
`CidTree::swap_root` only changes the root and clears its in-memory directory
cache. Deployment removes stale readdir stubs after rooted publication.

## IPNS Stem

`ww run --ipns-stem <name>` selects an IPNS name as the authoritative mutable
base. `--ipns-stem` and the Atom `--stem` option are mutually exclusive. The
name can be a canonical Base36 CIDv1 IPNS name, a legacy base58 Peer ID, or the
same name prefixed with `/ipns/`.

The IPNS Source uses Kubo's Gateway listener for raw HTTP Routing V1 records:

```text
GET /routing/v1/ipns/{canonical-base36-name}
Accept: application/vnd.ipfs.ipns-record
```

Wetware limits records to 10,240 bytes. `rust-ipns` decodes the protobuf,
verifies the signature and signer, and reads authenticated value, sequence,
EOL, and TTL semantics. The value must be exactly one `/ipfs/<cid>` binding.
Kubo supplies transport and routing only.

The Source stores the accepted raw record at
`~/.ww/ipns/follow/<canonical-base36-name>.record`. Persistence completes
before the Source returns an update. On Unix, directories use mode `0700` and
record files use mode `0600`. The write path syncs a temporary file, renames
it, and syncs the parent directory.

The persisted record is both the restart watermark and the ordering floor. A
valid unexpired record seeds boot. An expired record retains its ordering floor
but does not seed a deployment. Lower-order and exact-duplicate observations
are ignored. A higher-order record with the same value advances the watermark
without advancing the deployment epoch. A valid same-value refresh also
updates the stored record and EOL without a deployment transition. A
higher-order record with a different deployment CID emits `Head` after
persistence. A same-sequence record with a different value is publisher
equivocation; Wetware retains the accepted binding while valid.

A validly signed higher-order record whose value is not exactly one
`/ipfs/<cid>` binding is also persisted before it emits `InvalidHead`. This is
an authoritative publisher selection, not network garbage.

Malformed protobufs, invalid signatures, wrong signers, expired network
records, and transport failures do not revoke authority. Once an accepted
record reaches its signed EOL, the Source emits one `InvalidHead` even when
retrieval is retrying. Deployment advances its local epoch, publishes
`root: None`, and terminates the old PID0. The host remains alive. A later
valid record can recover through the ordinary deployment path.

A Routing V1 `404` means that no record exists. Unsupported routes fail with
the Gateway configuration guidance above. Transport and server failures retry.
Unexpected media types, oversized bodies, cryptographic failures, and rejected
`PUT` responses remain distinct errors. Wetware does not select them as
authority.

Wetware assumes one running process per `~/.ww` state directory. The local
state is trusted against rollback. The implementation does not provide
multiprocess coordination or secure monotonic storage.

## Artifact publication

```text
ww master commit
       |
       +--> host build matrix ----------> downloadable host binaries
       |
       +--> WASM build -----------------> kernel/status artifacts
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

## Default host IPNS publication

`ww perform install` creates `~/.ww/identity` and `~/.ww/etc/ns/ww` when absent.
The default namespace contains the IPNS name derived from that key and an
immutable bootstrap CID. `ww perform update` preserves identity, reuses that
namespace unless its bootstrap is absent, and renders the daemon with explicit
`--ipfs-url` and `--ipns-routing-url` values. The
running daemon signs the configured bootstrap CID before namespace resolution.
If initial publication has a temporary failure, the daemon uses the immutable
bootstrap for that boot and retries publication in the background.

The publisher stores its canonical raw record at
`~/.ww/ipns/publish/<canonical-base36-name>.record`. It reconciles valid newer
network state before creating a changed binding. It persists locally before
Routing V1 `PUT`, so a failed `PUT` can retry the same signed bytes. A changed
CID increments sequence. A same-CID refresh preserves sequence and extends
the signed EOL. If a newer network record selects a different value, Wetware
adopts its ordering floor but does not publish over it. Publication stops with
a single-writer conflict diagnostic for operator review.

The republisher waits one minute after an initial success, then refreshes every
four hours. Failed publication retries after five minutes. Records use a
48-hour lifetime and five-minute TTL. Only the default name derived from the
host identity receives this lifecycle. Following a third-party IPNS Stem does
not publish that name.

The default publisher no longer creates or uses Kubo's `"ww"` signing key.
An old `"ww"` key can remain in the Kubo keystore, but Wetware ignores it.
Kubo never needs `~/.ww/identity`. The legacy guest `Routing.publish` method is
removed. Provider `Finder` and `Announcer` capabilities do not resolve or
publish IPNS records and do not receive host signing authority.

Kubo 0.33 requires this one-time operator configuration:

```sh
ipfs config --json Gateway.ExposeRoutingAPI true
# Restart Kubo after changing its configuration.
```

Wetware uses `http://localhost:8080` by default. Set `IPFS_ROUTING_API` or
`--ipns-routing-url` when the Gateway listener uses another address. Do not
point this option at the administrative RPC listener on port 5001. Wetware
fails with an actionable error when the listener does not expose Routing V1;
it does not fall back to Kubo `name/resolve`, `name/publish`, or key management.

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

The packaged container FHS supplies the installed baseline:

```text
/usr/share/wetware/kernel/
  boot/kernel.wasm
  bin/status.wasm
```

All three container recipes set `WW_FHS_ROOT=/usr/share/wetware/kernel` and
default to `ww run`. The baseline stays outside the writable HOME volume.
Stem, namespace, and explicit CLI content can override baseline paths through
the same composition rules. Explicit CLI operations remain highest across epochs.

The Rust pid0 directly installs the image-owned `/status` composition.

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
status cell registered by the default Rust pid0
```

The public `/status` cell route is an end-to-end serving check. The
localhost-only admin plane on `127.0.0.1:2026` is the process control surface:

- `/healthz` confirms the admin server is accepting requests.
- `/readyz` reports whether the runtime has reached its serving phase.
- `/version` reports source and current generation kernel provenance plus degraded
  cache state.
- `/metrics` exposes live Wasmtime cache state and counters. See the collection
  policy below for measurements that are not yet available.

Keep the admin listener on loopback unless an authenticated network boundary
is added; these endpoints are intentionally unauthenticated.

### Metrics collection policy

`GET /metrics` serves Prometheus text format 0.0.4. The four live metric
families use the process-wide Wasmtime engine factory:

| Family | Meaning |
| --- | --- |
| `ww_wasmtime_cache_state` | Three Boolean samples with `state` labels `enabled`, `disabled`, and `fallback`; exactly one is 1. State records cache initialization, not ongoing filesystem health. |
| `ww_wasmtime_cache_hits_total` | Successful loads from the persistent compilation cache. |
| `ww_wasmtime_cache_stores_total` | Successful stores to that cache, not lookup misses or compilation attempts. |
| `ww_wasmtime_component_compilations_total` | Calls to the canonical component compilation API, including cache hits and failed attempts. |

The counters cover canonical engines in this process and reset when the process
restarts. Scraping reads a snapshot; it does not compile components or scan the
cache directory. The snapshot reads counters separately and is not a transaction
across concurrent compilations. Disabled and fallback cache states legitimately
report zero hits and stores. A later write failure can leave the initialization
state `enabled`; #609 owns reporting those failures.

The following families retain HELP/TYPE metadata but have no production
collectors connected. Missing samples mean unavailable measurements, not zero
activity:

- ARC cache: `ww_cache_hits_total`, `ww_cache_misses_total`,
  `ww_cache_evictions_total`, `ww_cache_weight_bytes`, and `ww_cache_entries`.
  #260 owns the connection to `PinsetCache` statistics.
- Stream pumps: `ww_stream_bytes_pumped_total` and `ww_stream_pump_ops_total`.
  #754 owns producer instrumentation. The renderer omits these samples and ARC
  samples even if callers modify the retained placeholder registries.
- Fuel: `ww_cell_fuel_remaining` and `ww_cell_fuel_consumed_total`.
  The epoch callback does not publish to `FuelRegistry`. #752 owns fuel and
  scheduler instrumentation.
- RPC: `ww_rpc_calls_total` and `ww_rpc_duration_seconds`.
  Production RPC paths do not update `RpcMetricsRegistry`; #754 owns that work.

Fuel and RPC registries remain available to callers. Empty or poisoned
registries omit their samples. Their dynamic labels, histogram input validation,
snapshot locking, and lifetime bounds need review before production wiring.
#750 tracks that remaining collection work and its operation-to-HTTP validation.

### Kubo startup availability

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
`/readyz` remains closed until trusted PID0 commits the current generation
after composition. The commit uses a private Wasm host import installed only
for PID0; it is not a Cap'n Proto capability and cannot be delegated over the
network. HTTP route liveness is checked by dispatch and the public `/status`
endpoint, not folded into kernel readiness. Readiness is not a continuous Kubo
availability probe after startup; liveness remains the process-level signal
during a later dependency outage.

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
DHT activity after startup remain unbounded by it. Configured-layer,
Stem-head, and effective-root pins gate deployment activation. A transient pin
failure keeps readiness closed and enters the deployment retry policy.

Related references: [architecture](architecture.md),
[capability model](capabilities.md), and [CLI](cli.md).
