# `ww-release-publisher` integration contract for infra#37

Status: implementation handoff for review. Production publication remains HOLD.
This document does not authorize infrastructure changes, deployment, key
provisioning, identity replacement, DNSLink updates, or release promotion.

## Ownership

`wetware/ww` owns the Rust executable, descriptor schema, authentication, staged
retention, local publication journal, consumer selection, and qualification tests.
GitHub Actions builds and validates the five Host artifacts and FHS, signs the
manifest, seeds immutable candidate blocks, and initiates the restricted command.
`wetware/infra` owns the installed executable digest, worker sandbox, persistent
journal storage, public trust configuration, Kubo repository and discovery key,
and renewal scheduling. The normal `ww` Host never executes this publisher.

The integration preserves the existing Kubo-owned `ww-release` identity. The
publisher receives no private discovery key file and does not export that key.
The independent #412 Ed25519 release key stays in the protected signing step.
The worker receives only its independently provisioned public keyring.

## Existing worker boundary

The inspected infra contract is commit
`527cd7e0e26b500cc2d1fd140896b2605d37f9e9`:
[`ops/vps/publisher-contract.md`](https://github.com/wetware/infra/blob/527cd7e0e26b500cc2d1fd140896b2605d37f9e9/ops/vps/publisher-contract.md).
The deployed Kubo version in that contract is 0.43.1. The worker has AF_UNIX
access to `/run/kubo/api.sock`, a private network namespace, 256 MiB memory,
32 tasks, 64 descriptors, 25% CPU, and a 120-second maximum producer timeout.
The current fixture accepts only `test_only=true`; it cannot activate this binary
without a reviewed infra change. No infra file changes are part of #738.

## Build and offline preparation

Build `cargo build --release --locked --bin ww-release-publisher` in `wetware/ww`.
Install the reviewed executable at the worker's fixed producer slot,
`/usr/local/libexec/wetware-release-producer`, only in a later approved operation.

```sh
ww-release-publisher prepare --tree release-tree --spool release-spool --metadata metadata.json
scripts/sign_release.sh release-tree/CHECKSUMS.txt release-tree/CHECKSUMS.txt.sig private.pem trusted-keys
ww-release-publisher finalize --tree release-tree --spool release-spool --trusted-keys trusted-keys
ww-release-publisher verify --spool release-spool --trusted-keys trusted-keys
```

`metadata.json` contains `channel`, positive `sequence`, `version`, `source`, and
`hosts`. `source` is `{repository, revision, builder}`; `hosts` maps each supported
Rust target triple to its actual `{repository, revision, builder}`. Reused binaries
retain their actual revision. `scripts/release_metadata.py` checks producer
receipts against the staged bytes. Each matrix producer writes `PROVENANCE.json`
with its actual repository, revision, and builder identity; reused artifacts keep
that receipt. The packaging run does not replace the producer identity.
Compatibility currently uses
`wetware-host-pid0-p3-v1`.

Preparation validates FHS bytes, constructs artifact graphs, and adds the exact
`RELEASE-DESCRIPTOR` binding to the manifest. Finalization requires an authorized
signature and verifies legacy paths against signed descriptor artifacts.
The output contains `root`, `descriptor`, `sequence`, `request_id`, and `stages`.
Here `root` is the final UnixFS envelope and `descriptor` is its typed descriptor.

`seed --spool release-spool --trusted-keys trusted-keys --kubo-api ENDPOINT`
retains immutable stages in a disposable CI Kubo. It performs no IPNS mutation
and creates no publication journal. The CI Kubo must make the complete candidate
retrievable until the infrastructure worker has acknowledged retention. A DHT
provide acknowledgement alone does not prove worker reachability. Before
activation, infra must qualify the actual runner-to-Kubo transfer, resource
limits, and timeout using production-sized candidate graphs.

The held Actions job signs only the current `origin/master` tip after Git
ancestry admission. Its signed release sequence is `(github.run_number << 16) | github.run_attempt`.
Attempts must be between 1 and 65,535; run numbers must fit 47 positive bits.
A full workflow rerun builds a new candidate with a new signed sequence. Delivery
retries for a sealed candidate reuse its original root and request ID.
Infra must agree on the initial sequence namespace before activation. A numeric
sequence does not prove Git ancestry. The trusted build/signing process attests
source provenance; the restricted worker has no Git checkout. A reset workflow
counter, rewritten master history, or journal migration requires separate review.

## Fixed command and producer input

Preserve the existing SSH grammar:

```text
publish-v1 <request-id> <candidate-root-cid>
```

`request-id` is the first 16 bytes of SHA-256 of the candidate CID's canonical
ASCII text, encoded as 32 lowercase hexadecimal characters. The executable
rejects other IDs. This binds retries to immutable content without another
request registry.

The worker invokes the installed binary with no arguments and writes one bounded
JSON object to stdin:

```json
{"version":1,"request_id":"0123456789abcdef0123456789abcdef","descriptor":"<candidate-root-cid>"}
```

The example request ID is illustrative; compute the actual ID with
`ww-release-publisher request --root CID`.
The historical field name `descriptor` identifies the final UnixFS envelope,
not the bare typed descriptor, because the envelope reaches the detached
signature without a hash cycle. Only canonical CIDv1 DAG-PB SHA-256 roots pass.
The producer accepts at most 512 request bytes and rejects unknown fields.
SSH supplies no path, key alias, API endpoint, executable, or configuration.

The worker sets `WETWARE_PUBLISH_ABI=1` and
`KUBO_API=/unix/run/kubo/api.sock`. No-argument mode reads only
`/etc/wetware/release-publisher.json`:

```json
{
  "version": 1,
  "domain": "wetware-official-release:<stable-kubo-repository-id>:<channel>",
  "channel": "<canonical-base36-IPNS-name>",
  "key": "ww-release",
  "journal": "/run/wetware-release-state/channel",
  "trusted_keys": "/etc/wetware/release-trusted-keys",
  "operation_timeout_seconds": 120
}
```

Provision the configuration and public keyring read-only and root-owned.
The key alias is fixed configuration. The publisher verifies the Kubo key's
public name before commitment and delivery. Changing the alias, channel, domain,
or Kubo repository is not an ordinary retry.

The journal path above must be a writable bind mount of **persistent local
storage**, not transient `/run` storage. Provision the private source directory
with durable parent creation and worker ownership before mounting it. The
filesystem must support exclusive locks, atomic rename, file sync, and directory
sync. Keep one directory per release channel. Never delete its lock file, edit
its journal, restore an older snapshot, or run a second owner elsewhere.
The directory's `acquisition/` child is bounded disposable block storage, not
publication authority. Provide sufficient disk space for the candidate graph.

## Result and retry semantics

Successful local commitment returns a bounded JSON line:

```json
{"version":1,"request_id":"...","descriptor":"<candidate-root>","local_committed":true,"delivery_acknowledged":false,"root":"<candidate-root>","generation":1,"release_sequence":1}
```

`delivery_acknowledged=false` means the local release committed but the delivery
acknowledgement is uncertain. Retry the same request. Acknowledgement `true`
proves Kubo accepted one request; it does not prove latest discoverability.
The real infra result validator must distinguish these states from fixture
`test_only=true`. It must preserve request correlation and bounded output.

A valid invocation that fails before a proven result emits a correlated JSON
object with `local_committed:null`, `delivery_acknowledged:false`, and
`error:"publication-result-unknown"`. The process can exit zero after emitting
that result. The worker and Actions must inspect the JSON flags; process exit
status alone is not a publication acknowledgement. Malformed input can fail
without a result.

A process termination, missing result, or journal-sync error leaves the caller
uncertain about commitment. Reopen the same journal and retry the same candidate.
Do not infer rollback from a timeout or delete state to make a retry succeed.
A superseded root cannot be republished by an old request. An already accepted
older Kubo request can still finish later; consumer freshness handles discovery
regression. There is no remote CAS or delayed-effect fence.

Renew the current root before its IPNS record expires by repeating its content-
bound request through the same worker and owner lock. Renewal must not depend on
a running `ww` Host or a developer machine. The worker must not schedule old-root
retries after a newer local commitment. No new release signing is needed for
same-root discovery renewal. Kubo can change transport sequence independently.

## Activation prerequisites and remaining qualification

Infra#37 must review the real producer/result binding, mount persistent journal
storage and public trust configuration, qualify its actual Kubo 0.43.1 Unix
socket, and exercise interrupted multi-stage acquisition/retention under the
sandbox's memory, CPU, task, descriptor, and timeout limits. Candidate seeding
and current-root renewal require end-to-end qualification. These are handoff
requirements; #738 does not modify or activate infrastructure.

Keep `jobs.publish.if: ${{ false }}`. Historical workflow executions and copied
legacy scripts still require the separate fence in
[legacy-publication-hold.md](legacy-publication-hold.md). Current legacy official
publisher entry points fail before external calls. The shell installer remains
compatible with the signed legacy tree and retains its existing replay limit.
No production release, key, discovery pointer, installer endpoint, or Host
replacement changes during this assignment.
