# Release publisher executable contract

`wetware/ww` owns release formats, authentication, packaging, retention, the
single durable publication owner, and `ww-release-publisher`. `wetware/infra`
owns executable installation, restricted transport, configuration, persistent
storage, lifecycle, and operational qualification. Production publication remains
HOLD. This contract grants no deployment or key-provisioning authority.

The infrastructure proposal and VPS runbook belong in
[infra's publisher contract](https://github.com/wetware/infra/blob/master/ops/vps/publisher-contract.md),
tracked by [infra#37](https://github.com/wetware/infra/issues/37).
The normal Host does not execute the official release publisher.

## Portable build and package commands

Build with `cargo build --release --locked --bin ww-release-publisher`.
GitHub Actions owns Host/FHS construction, provenance, validation, and signing.

```sh
ww-release-publisher sequence RUN_NUMBER RUN_ATTEMPT
ww-release-publisher metadata --artifacts artifacts --tree release-tree --channel CHANNEL --sequence SEQUENCE --builder BUILDER > metadata.json
ww-release-publisher prepare --tree release-tree --spool release-spool --metadata metadata.json
scripts/sign_release.sh release-tree/CHECKSUMS.txt release-tree/CHECKSUMS.txt.sig private.pem trusted-keys
ww-release-publisher finalize --tree release-tree --spool release-spool --trusted-keys trusted-keys
ww-release-publisher verify --spool release-spool --trusted-keys trusted-keys
```

`metadata` verifies the five actual producer receipts against staged Host bytes.
Reused artifacts retain their original producer revision and builder identity.
`sequence` computes `(run_number << 16) | run_attempt`: run numbers use 47 positive
bits; attempts are 1..65,535. A rerun creates a new signed sequence; delivery
retries reuse the sealed candidate. The trusted CI admission checks the current
master revision. Signed sequence ordering does not prove Git ancestry.

Preparation adds the canonical descriptor binding before signing. Finalization
verifies signature and payload parity before creating the immutable UnixFS root.
Finalization and verification return `root`, `descriptor`, `sequence`, `request_id`,
and `stages`. `root` is the transport envelope; `descriptor` is its typed IPLD link.

`seed --spool PATH --trusted-keys PATH --kubo-api ENDPOINT` retains authenticated
immutable content without IPNS mutation or a publication journal. Infrastructure
must qualify candidate reachability; a DHT acknowledgement alone is insufficient.
The held workflow prepares, signs, and finalizes. Its final step refuses until
infra transport integration is reviewed. `jobs.publish.if` remains literal false.

## Request and result ABI

`request --root CID` emits version-1 request JSON. The ID is the first 16 bytes of
SHA-256 of canonical CID text, encoded as 32 lowercase hexadecimal characters.
A request is at most 512 bytes and contains exactly these fields:

```json
{"version":1,"request_id":"<content-bound-id>","descriptor":"<candidate-root-cid>"}
```

The historical `descriptor` field means the UnixFS envelope, because that root
reaches the detached signature without a hash cycle. The publisher requires a
canonical CIDv1 DAG-PB SHA-256 root. SSH grammar and envelope framing belong to
infra; they do not replace Rust CID or release authentication.

Installed no-argument mode requires `WETWARE_PUBLISH_ABI=1`, `KUBO_API=/unix/...`,
and fixed `/etc/wetware/release-publisher.json`. Configuration contains exactly
`version: 1`, `domain`, `channel`, `key`, `journal`, `trusted_keys`, and
`operation_timeout_seconds` (1..120). Infra chooses the values and provisions
configuration/public trust independently of candidate content. `domain` identifies
one Kubo repository and channel. `channel` is canonical base36 IPNS text. Kubo
retains the discovery key; the publisher checks its public name before commitment.
The signing private key never reaches the publisher.

`journal` requires existing private persistent local storage supporting exclusive
locks, atomic rename, file sync, and directory sync. All invocations for a channel
use that same directory. Its `acquisition/` child stores bounded disposable blocks.
The [publication protocol](release-publication.md) defines limits and recovery.
Infra must not create another release journal, restore an older owner snapshot,
or implement release ordering in a transport adapter.

Producer stdout is one JSON line below 16 KiB, correlated by `version`,
`request_id`, and `descriptor`. A committed result adds `local_committed: true`,
`delivery_acknowledged`, `root`, `generation`, and `release_sequence`.
Delivery acknowledgement is distinct from eventual discoverability.
An uncertain result uses `local_committed: null`, `delivery_acknowledged: false`,
and `error: "publication-result-unknown"`. Such a result can exit zero.
Callers must inspect the flags, not infer success or rollback from exit status.

Missing output or process termination also leaves commitment uncertain. Retry
the same immutable request through the same owner. The owner refuses superseded
roots; a previously accepted remote write can still finish later. Renewal repeats
the current request. Scheduling and transport deadlines belong to infra.

`publish --config PATH --kubo-api ENDPOINT [--package PATH]` exposes the same
protocol for isolated qualification. The portable direct-Kubo suite remains in
`ww`; worker, SSH, cgroup, storage, renewal, and recovery acceptance belong to infra.
