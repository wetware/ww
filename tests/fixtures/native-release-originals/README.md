# Original GNU ARM64 qualification inputs

These are the exact original archives from producer A run `42623846342` and producer B run `42648050888`, attempt 1. They are diagnostic fixtures, not published releases. Both producer `Test` failures remain failures. These bytes do not authorize native acceptance, signing, publication or production activation.

GitHub originally retained these artifacts for one day. Both artifacts later became unavailable. Keeping their exact ZIP bytes and historical receipts in Git separates the lifetime of the diagnostic fixture from GitHub's temporary download storage. Git attributes disable line-ending conversion for every retained input. Each ZIP is approximately 16 MB; the complete retained bundle is approximately 33 MB before Git packing.

`inputs/retention.json` hashes every original ZIP, artifact/run/job receipt and upload log. `.github/native-release-integration.json` pins that manifest's SHA-256 as well as the original ZIP and executable identities. `inputs/capture-manifest.json` is the unchanged evidence manifest recorded before the original retention periods ended. Its twelve relevant file entries bind the retained inputs. Its timestamp is a historical record, not a cryptographic timestamp or new release authority.

Use the verifier's explicit `--retained` mode for these fixtures. It checks the manifest and file hashes, confirms capture occurred within the original artifact retention interval, and preserves all repository, workflow, source, attempt, build, upload, executable, FHS and failed-check bindings. Fresh API inputs still reject current transport expiry. There is no arbitrary clock override in the command-line interface.

The verifier also retains the original runtime/build source-equality requirement. These old executables must not be relabelled as a changed checkout. A new runtime implementation needs separate producer qualification; retaining these fixtures does not satisfy that requirement.

The original archive bytes are neither unpacked nor executed by repository checkout. The verifier validates ZIP paths, member types, sizes, ELF target, hashes, sidecars and producer identity before writing separate executable copies into disposable qualification storage.
