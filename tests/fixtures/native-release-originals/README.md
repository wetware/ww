# Original GNU ARM64 qualification inputs

These are the exact original archives from producer A run `42623846342` and producer B run `42648050888`, attempt 1. They are diagnostic fixtures, not published releases. Both producer `Test` failures remain failures. These bytes do not authorize native acceptance, signing, publication or production activation.

GitHub originally retained these artifacts for one day. Both artifacts later became unavailable. Keeping their exact ZIP bytes and historical receipts in Git separates the lifetime of the diagnostic fixture from GitHub's temporary download storage. Git attributes disable line-ending conversion for every retained input. Each ZIP is approximately 16 MB; the complete retained bundle is approximately 33 MB before Git packing.

`inputs/retention.json` hashes every original ZIP, artifact/run/job receipt and upload log. `.github/native-release-integration.json` pins that manifest's SHA-256 as well as the original ZIP and executable identities. `inputs/capture-manifest.json` is the unchanged evidence manifest recorded before the original retention periods ended. Its twelve relevant file entries bind the retained inputs. Its timestamp is a historical record, not a cryptographic timestamp or new release authority.

Use the verifier's explicit `--retained` mode for these fixtures. It checks the manifest and file hashes, confirms capture occurred within the original artifact retention interval, and preserves all repository, workflow, source, attempt, build, upload, executable, FHS and failed-check bindings. Fresh API inputs still reject current transport expiry. There is no arbitrary clock override in the command-line interface.

The verifier also retains the original runtime/build source-equality requirement. These old executables must not be relabelled as a changed checkout. A new runtime implementation needs separate producer qualification; retaining these fixtures does not satisfy that requirement.

CI verifies the historical pair against a detached checkout of B, `d3d827c24d2f2d0525fd7056adaf6d18a7f33074`. The receipt records that source separately from the current integration harness. The historical lane still executes the original installer, admitted boot and native upgrade/recovery subset.

The separate current-source ARM64 job builds a development Host and runs all five admitted boot modes. On the qualification branch, the job also builds release candidate C and executes A→C→A. `prepare_current_native_release.py` binds the exact checkout, component source, effective Cargo profile, toolchain, executable, archive and build log. C uses the repository release settings with a native GNU ARM64 compiler; the original producer used `cross`. The receipt records that difference and does not claim byte-identical builds or an upload receipt that does not exist.

Current inputs are built and consumed within one job. That job uploads the exact inputs and qualification receipts for review. A missing historical artifact never selects C implicitly. Required `Test` includes current development boot; release results supplement development correctness coverage. The 30-second startup guard, all 13 executable reads and the existing native admission restriction remain unchanged.

The original archive bytes are neither unpacked nor executed by repository checkout. The verifier validates ZIP paths, member types, sizes, ELF target, hashes, sidecars and producer identity before writing separate executable copies into disposable qualification storage.
