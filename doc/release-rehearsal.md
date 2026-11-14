# Disposable release rehearsal

`scripts/qualify_release_rehearsal.py` exercises the public package boundary with
an actual `release-input.car` from the existing release validation job. It uses
the public `ww-release-package` executable and disposable Ed25519 signing keys.
It does not publish, provision trusted channels, register services, or admit a
protected production workflow.

## Artifact-only CI qualification

The existing `.github/workflows/rust.yml` accepts a boolean `artifact_only`
input, defaulting to `false`. An explicit `true` on a reviewed non-`master`
branch reuses all five native builds, Wasm builds, package validation and
container boot checks. It exports the same raw CAR format as
`qualification-release-input.car`, retaining the original producer receipts.
It does not run the Pages build/deployment or GHCR publication jobs. IPFS
publication remains unconditionally disabled. The Host test Kubo runs offline
and receives no configured production peer address in this mode.

The dependency graph is:

```text
checks + Wasm → five native builds → validate-release → qualification marker
                      Test ───────────────┘                  │
                                       successful overall run required
Pages build/deploy: skipped    GHCR: skipped    IPFS publication: held
```

Ordinary master pushes and dispatches retain their existing behavior. Do not
use ordinary dispatch for a disposable qualification. Qualification on `master`
or a tag cannot pass the qualification marker or produce the qualification CAR.

A future run requires separate approval to push the reviewed patch to a named
non-`master` branch, followed by approval for exactly one dispatch of
`wetware/ww/.github/workflows/rust.yml` on that branch at its recorded full commit
SHA, with `artifact_only=true`. Verify the remote branch still names that SHA
immediately before dispatch. The approval must exclude retries, other workflows,
publication, deployment and production signing. Example, **only after approval**:

```sh
gh workflow run rust.yml --repo wetware/ww --ref QUALIFICATION_BRANCH -f artifact_only=true
```

Private `tools/release-publisher/scripts/artifact-handoff.py fetch-qualification`
provides separate test admission. Use the reviewed private tooling revision and
supply the exact branch, run, attempt, source/tool revisions, artifact ID and
API digest. Admission requires a successful overall run, the `Test-only release
qualification` marker, skipped publication/deployment jobs, and original
five-platform receipts from that same run and attempt. It retains the downloaded
CAR for the signing rehearsal below. Downloading alone does not establish
admission. Keep the output and original API metadata as evidence.

```sh
python3 /path/to/reviewed/infra/tools/release-publisher/scripts/artifact-handoff.py fetch-qualification \
  --branch QUALIFICATION_BRANCH --run-id RUN_ID --attempt ATTEMPT \
  --revision FULL_PRODUCER_SHA --tool-revision FULL_PACKAGE_TOOL_SHA \
  --artifact-id ARTIFACT_ID --digest sha256:API_DIGEST \
  --package-tool /path/to/reviewed/ww-release-package --out /fresh/private/admission
```

The existing private tool uses `WW_ARTIFACT_READ_TOKEN` for read-only GitHub API
access. Keep the credential out of logs and signing output. The command does not
request a production signing key or publish the candidate.

Qualification admission carries `purpose: disposable-qualification`. Official
admission still requires `master`, the official artifact name, and successful
GHCR publication. Official sealing and receipt verification reject marked test
data. Test signing uses only the disposable keys and independent verifier trust
created by the existing rehearsal. A locally signed test CAR does not authorize
official signing or publication. Never provision its keys as production trust.

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

Optionally pass `--alternate-binary /path/to/separately-built/ww` to test native
compatibility refusal and authenticated shell recovery. Retain that binary's
exact source, toolchain, build command, input hashes, and output hash separately.
The candidate receives distinct local-builder provenance. The native adapter
must reject compatibility after verified staging. The shell must authenticate
the original release before aborting the pre-replacement transaction. The test
retains native service observation and admission checks; an incompatible platform
environment fails the test instead of changing the operator's service.

Consumer `report.json` and per-command outputs distinguish successful no-ops,
derived FHS transitions, and pre-replacement recovery. None establishes successful
native replacement, native restart admission, launchd lifecycle, or automatic
channel freshness. Private-state sentinels establish byte preservation, not signed
freshness verification.
