# Original GNU ARM64 qualification inputs

These are the exact original archives from producer A run `42623846342` and producer B run `42648050888`, attempt 1. They are diagnostic fixtures, not published releases. Both producer `Test` failures remain failures. These bytes do not authorize native acceptance, signing, publication or production activation.

GitHub originally retained these artifacts for one day. Both artifacts later became unavailable. Keeping their exact ZIP bytes and historical receipts in Git separates the lifetime of the diagnostic fixture from GitHub's temporary download storage. Git attributes disable line-ending conversion for every retained input. Each ZIP is approximately 16 MB; the complete retained bundle is approximately 33 MB before Git packing.

`inputs/retention.json` hashes every original ZIP, artifact/run/job receipt and upload log. `.github/native-release-integration.json` pins that manifest's SHA-256 as well as the original ZIP and executable identities. `inputs/capture-manifest.json` is the unchanged evidence manifest recorded before the original retention periods ended. Its twelve relevant file entries bind the retained inputs. Its timestamp is a historical record, not a cryptographic timestamp or new release authority.

Use the verifier's explicit `--retained` mode for these fixtures. It checks the manifest and file hashes, confirms capture occurred within the original artifact retention interval, and preserves all repository, workflow, source, attempt, build, upload, executable, FHS and failed-check bindings. Fresh API inputs still reject current transport expiry. There is no arbitrary clock override in the command-line interface.

The verifier also retains the original runtime/build source-equality requirement. These old executables must not be relabelled as a changed checkout. A new runtime implementation needs separate producer qualification; retaining these fixtures does not satisfy that requirement.

CI verifies the historical pair offline against a detached checkout of B, `d3d827c24d2f2d0525fd7056adaf6d18a7f33074`. The receipt records that source separately from the current harness. CI does not execute either historical Host.

`prepare_current_native_release.py` retains the separate current-source build and receipt verifier. Its receipt binds the exact checkout, component source, Cargo profile, toolchain, executable, archive and build log. A same-job candidate is not an uploaded producer artifact or a completed check. No current-source native build or integration job is enabled by this stage.

The historical branch name in `.github/native-release-integration.json` identifies the original producer receipts. It is not a permanent CI branch exception. Current-source receipt validation has no Rome branch-name exception.

The live development-boot and native upgrade entrypoints return `UNAVAILABLE` with exit code 3 until PR 3 supplies the required runtime interfaces and reviewed live policy. Offline report validation preserves `PARTIAL`; neither `PARTIAL` nor `UNAVAILABLE` means native acceptance.

The offline commands require Python 3.11 or newer and Git on a Unix system.

To copy retained inputs without contacting expired GitHub transport:

```sh
python3 scripts/fetch_native_release_inputs.py \
  --pins .github/native-release-integration.json \
  --retained tests/fixtures/native-release-originals/inputs \
  --output /absolute/new/input-directory
```

To verify both artifacts and historical source, from the repository root:

```sh
git worktree add --detach /absolute/historical-source d3d827c24d2f2d0525fd7056adaf6d18a7f33074
python3 scripts/verify_native_release_inputs.py --retained \
  --checkout /absolute/historical-source \
  --inputs tests/fixtures/native-release-originals/inputs \
  --pins .github/native-release-integration.json \
  --output /absolute/new/verified-directory
git worktree remove /absolute/historical-source
```

Output directories must not exist. Their parent directories must exist. Historical source B must be present in the local Git object database. Verification against a changed current runtime must refuse source drift.

The original archive bytes are neither unpacked nor executed by repository checkout. The verifier validates ZIP paths, member types, sizes, ELF target, hashes, sidecars and producer identity before writing separate executable copies into disposable qualification storage.
