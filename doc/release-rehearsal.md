# Disposable release rehearsal

`scripts/qualify_release_rehearsal.py` exercises the public package boundary with
an actual `release-input.car` from the existing release validation job. It uses
the public `ww-release-package` executable and disposable Ed25519 signing keys.
It does not publish, provision trusted channels, register services, or admit a
protected production workflow.

Build the tool from a recorded source revision:

```sh
cargo build --locked -p ww-release --features package --bin ww-release-package
```

Read the source run, attempt, artifact ID and SHA-256 digest independently from
GitHub. Preserve the original CAR and producer logs. Downloading an artifact
does not satisfy private publication admission. A run waiting for protected
deployment approval remains inadmissible under the current private policy.

Run the rehearsal with the disposable publisher's canonical IPNS name and a
fresh output directory:

```sh
python3 scripts/qualify_release_rehearsal.py \
  --package-tool target/debug/ww-release-package \
  --input-car /path/to/release-input.car \
  --sha256 SHA256_FROM_ARTIFACT_API \
  --revision FULL_SOURCE_COMMIT \
  --run-id WORKFLOW_RUN_ID --attempt 1 \
  --channel DISPOSABLE_CANONICAL_IPNS_NAME \
  --output .context/release-rehearsal
```

The script checks the CAR digest before import. It verifies all five original
Host receipts, executable formats, architectures and binary checksums. The
public tool validates the FHS, prepares the descriptor, and verifies signed
payloads. Two constructions must produce the same descriptor, root and CAR
bytes. A separate verifier trust file authenticates the imported signed graph.

Negative controls require exit code 1 and the expected diagnostic. Truncated,
missing, duplicate, tampered and oversized CAR records must fail. Failed imports
must remove their newly created spool. Record reordering is supported and must
preserve authenticated identity. Wrong-key verification and changed signed
Host bytes must fail.

`report.json` records identities, producer receipts, tool hashes and results.
`commands.json` and per-command output files retain executable evidence. A
second signed sequence reuses the same genuine Host and FHS bytes. The private
disposable harness can consume `candidate.json`, `candidate-next.json`, both
CARs and `verifier-trust`. These local test receipts do not replace protected
artifact admission or authorize official publication.

The output includes test-only private keys under mode-0700 signer directories.
Keep the output private. Commit source and reviewed reports, not generated keys
or artifact trees. Use `tree-b` for the intact first signed tree; `tree-a` is
deliberately corrupted by the changed-Host negative control.

This rehearsal does not prove embedded revision provenance, actual consumer
installation, service lifecycle, running native identity, or native upgrade
recovery. Preserve separate producer/disassembly, consumer and platform evidence
for those acceptance gates. See [native upgrade gates](native-upgrades.md) and
the [private publisher boundary](release-publisher-infra-contract.md).

## Consumer rehearsal

`scripts/qualify_release_consumer.py` consumes the intact `tree-b`, signed CAR,
metadata, independently provisioned verifier trust, and disposable signer from
the public rehearsal. Supply the matching native Host and a pinned Kubo CLI on
`PATH`. Use a fresh output directory:

```sh
python3 scripts/qualify_release_consumer.py \
  --rehearsal .context/release-rehearsal \
  --binary /path/to/original-platform-artifact/ww \
  --package-tool target/debug/ww-release-package \
  --output .context/consumer-rehearsal
```

The consumer script starts an offline Kubo with no bootstrap peers or mDNS. It
imports signed CARs and runs the actual `install`, `image update`, and `upgrade`
commands under a fresh `HOME`. No daemon is registered. The script checks fresh
installation, repeat installation, original Host bytes, private-state sentinel
preservation, and an exact-artifact native no-op. A second signed sequence keeps
the original producer bytes. A local proxy substitutes another validly signed
release and must receive a requested-root CID rejection without installed changes.

A separate transition fixture appends a valid custom section to the genuine
status component. This derived FHS is not original CI producer-byte evidence.
The script signs and statically validates the fixture, applies an actual FHS
change, checks that Host bytes remain unchanged, and checks idempotent retry.
It then replays the retained FHS journal with missing target bytes. Recovery must
restore the previous baseline before contacting an unavailable endpoint. Journal
replay does not qualify a real process kill during the FHS update.

Optionally pass `--alternate-binary /path/to/separately-built/ww` to test
unmanaged native-upgrade refusal. Retain that binary's exact source, toolchain,
build command, input hashes, and output hash separately. The candidate receives
distinct local-builder provenance. The helper installs the original release in
a separate `HOME` without registering a service. It invokes the changed upgrade
with `--timeout-secs 120` and requires unmanaged replacement refusal before
executable replacement or journal creation. Installed and private file bytes
must remain unchanged. Service observation errors fail the rehearsal; the helper
does not accept arbitrary errors as compatibility refusal.

The helper then invokes shell `--recover --timeout-secs 120`. Because refusal
created no transaction, the shell must reject the request for a missing pending
journal before transport or authentication. The helper checks that no journal
appears and that installed and private file bytes remain unchanged. This negative
control does not demonstrate successful authenticated recovery. The default
exact-artifact upgrade no-op remains unchanged and does not require a timeout.

Successful interrupted native recovery belongs to the dedicated
[`qualify_native_upgrade.py` qualification](../scripts/qualify_native_upgrade.py)
and its [native integration runner](../tests/native_release_integration.py).
See [authenticated native recovery](native-upgrades.md) for the recovery contract.
The consumer rehearsal does not construct a pending native transaction or repeat
that qualification campaign.

Consumer `report.json` and per-command outputs distinguish successful no-ops,
derived FHS transitions, and native refusal checks. None establishes successful
native replacement, interrupted native recovery, native restart admission,
launchd lifecycle, or automatic channel freshness. Private-state sentinels
establish byte preservation, not signed freshness verification.
