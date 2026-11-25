# Deployment

`wetware/ww` CI builds immutable Host/FHS artifacts and container images.
Private `wetware/infra` owns official authenticated software releases. A merge
to `ww` master does not deploy production or authorize release publication.

The accepted Epiphyte infrastructure runs Debian 13, native Caddy, and Kubo
under Podman/Quadlet and systemd. Kubernetes/k3s and the `ww-master` cluster
deployment are retired. See [infra steady-state operations](https://github.com/wetware/infra/blob/master/ops/vps/steady-state.md).

The tag-driven release pipeline is implemented, but production signing, IPFS
publication, DNSLink changes and `/install` activation remain held. The
[operator runbook](https://github.com/wetware/infra/blob/9f0815f8555e1b00d00f063837bf0f806bed4963/ops/vps/official-release.md)
is the reviewed draft in [infra#50](https://github.com/wetware/infra/pull/50).
The [publication transition](https://github.com/wetware/infra/blob/master/ops/vps/publication-transition.md)
defines the remaining activation gates.

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
the [Gateway configuration guidance](cli.md#ww-run). Transport and server
failures retry.
Unexpected media types, oversized bodies, cryptographic failures, and rejected
`PUT` responses remain distinct errors. Wetware does not select them as
authority.

Wetware assumes one running process per `~/.ww` state directory. The local
state is trusted against rollback. The implementation does not provide
multiprocess coordination or secure monotonic storage.

## Artifact publication

Public CI builds and validates Host binaries, the canonical FHS, and container
images. It exports five Host/FHS archives and one `release-source` archive,
preserving original producer receipts. These build inputs are not an authenticated
release. See the [producer artifact contract](release-publisher-infra-contract.md).

After separately approved production activation, the implemented official path is:

1. An operator selects a `v`-prefixed SemVer tag on `wetware/ww` master history
   and manually dispatches the private Official release workflow once.
2. `wetware/infra` independently admits the exact successful CI build and
   artifacts, rechecks the tag, and automatically signs with separate release
   authority. Verification uses independently provisioned public trust.
3. Restricted `acquire-v1` transfers the signed CAR. `publish-v1` invokes the
   publisher, whose durable Owner retains the graph in Epiphyte's Kubo before
   committing the release and requesting announcement through the existing
   `ww-release` IPNS identity.
4. Operators explicitly install or upgrade authenticated releases. Publication
   does not install software or restart consumer Hosts.

The Owner's durable commitment, Kubo delivery acknowledgement, independent IPNS
discovery and successful installation are distinct results. Delayed older writes
can temporarily supersede newer discovery values. See
[release publication](release-publication.md) for consumer freshness and the
reproduced IPNS limitation.

GHCR container images remain supported build artifacts, including
`ghcr.io/wetware/ww:master-<git-sha>` and immutable digest references.
They are not the official authenticated IPFS software-release distribution path.
The public `release_image` job still publishes tested images; its legacy display
name, “Build and publish ww-master image,” does not imply cluster deployment.
The public IPFS publication job and all private official release jobs remain held.

Provision the current release IPNS identity independently from infra's accepted
identity record; do not reuse the retired hardcoded name or initialize a new key.
The HTTPS installer embeds the accepted identity and discovers the release
from its signed IPNS record. Standalone `ww install --release CID` uses an
explicit immutable release.
For an existing installation, `ww upgrade` supports authenticated IPNS selection
with `WW_RELEASE_IPNS` or `--release-ipns`, while `--release CID` selects an
immutable release explicitly. Channel checks persist freshness floors without
replacing the Host. See [installation](installation.md) and
[native upgrades](native-upgrades.md) for trust, platform and recovery requirements.

## Default host IPNS publication

`ww install` creates the Host, baseline FHS, and identity. It does not create
`~/.ww/etc/ns/ww` or configure publication. `ww image update` changes only the
owned baseline; it never restarts the Host or rewrites namespaces. Existing
namespace configuration remains intact.

An operator can explicitly configure the `ww` namespace with the Host's own
IPNS name and an immutable bootstrap CID. On startup, the Host signs that
configured target before namespace resolution. If initial publication has a
temporary failure, the Host uses the immutable bootstrap for that boot and
retries publication in the background. Software installation does not grant
this publication authority.

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

The [default IPNS identity contract](keys.md#default-ipns-identity) defines
key ownership and storage. Kubo never needs `~/.ww/identity`; an old Kubo `"ww"`
key can remain but is ignored. Provider `Finder` and `Announcer` capabilities
do not resolve or publish IPNS records and receive no Host signing authority.
Configure the Gateway listener as described in the [CLI reference](cli.md#ww-run).
Wetware does not fall back to Kubo key management.

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

This is the Host's configured WAGI listener, not the production website path.
Epiphyte's Caddy serves the static website; no replacement cluster route is implied.

```text
client
  |
  v
Host HTTP listener :2080 (WAGI adapter)
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
- `/metrics` exposes live Wasmtime cache and lifecycle, ExecutorPool, applied fuel decisions,
  settled fuel consumption, worker liveness, and telemetry export/loss. See the
  [metrics contract](observability.md#metrics-collection-contract) for
  unavailable measurements.

Keep the admin listener on loopback unless an authenticated network boundary
is added; these endpoints are intentionally unauthenticated.

### Metrics collection policy

`/metrics` exposes Prometheus samples for Wasmtime cache and lifecycle,
ExecutorPool, applied fuel decisions, settled fuel, worker liveness and telemetry.
Missing families represent unavailable measurements, not zero activity.
See the [metrics collection contract](observability.md#metrics-collection-contract)
for counter meanings, lifetimes, label bounds and producer measurement references.
Use the [local observability workflow](observability.md) to collect and correlate
metrics, Host logs and sampled traces. Collection does not establish runtime
readiness or production qualification.

### Kubo startup availability

Kubo TCP connection attempts are bounded to five seconds. The small local `/api/v0/id`
readiness probe is additionally bounded to 30 seconds, so a listener that
accepts connections but fails to answer cannot wedge the startup wait loop.
Bulk content transfer and ordinary runtime DHT operations deliberately do not
inherit that 30-second deadline: they can make legitimate progress for longer
than a readiness interval.

`ww run` uses a 120-second Kubo wait by default so a local development
invocation fails clearly when Kubo is absent. A production deployment that
must keep waiting through a sustained Kubo outage can set
`WW_KUBO_WAIT_MAX_SECS=0`. This keeps `/healthz` available while
`/readyz` remains closed after the enabled admin listener has bound. Ordinary
startup requires no native-upgrade admission and does not consult native journals.
`/readyz` stays closed until trusted PID0 commits the current generation after
composition. The commit uses a private Wasm host import installed only
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
progress. After the enabled admin listener has bound, `/healthz` remains available
and `/readyz` remains closed while a mandatory mount call retries. Namespace fallback to its bootstrap
CID marks the runtime degraded. This is not a global HTTP timeout: content reads and
DHT activity after startup remain unbounded by it. Configured-layer,
Stem-head, and effective-root pins gate deployment activation. A transient pin
failure keeps readiness closed and enters the deployment retry policy.

Related references: [architecture](architecture.md),
[capability model](capabilities.md), and [CLI](cli.md).
