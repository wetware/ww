# Public release library and private release operations

Public `wetware/ww` owns the `release` Rust library: descriptors, authentication,
package construction/parsing, CAR serialization and independent verification.
Private `wetware/infra` owns the single `ww-release-package` executable, unsigned
candidate assembly, signing, finalization, publication and custody. Private code
imports the public library; public code never fetches private source or credentials.
The `release` package replaces `ww-release` without changing signed bytes or wire
formats. All private public-crate dependencies use one immutable full Git SHA.

Public consumers can authenticate releases with `release::auth::TrustedKeys`,
`release::package::Package::authenticate`, the existing `ww` Host installation and
upgrade commands, or `scripts/install.sh --verify-manifest` with independent trust.
An artifact or its download location cannot supply its own trust anchor.

## Producer artifact contract

Public CI builds five Host targets with their canonical FHS and emits five
separate standard GitHub ZIP artifacts:

| Artifact | Provenance target |
| --- | --- |
| `ww-linux-x86_64` | `x86_64-unknown-linux-gnu` |
| `ww-linux-aarch64` | `aarch64-unknown-linux-gnu` |
| `ww-macos-aarch64` | `aarch64-apple-darwin` |
| `ww-macos-x86_64` | `x86_64-apple-darwin` |
| `ww-linux-musl-x86_64` | `x86_64-unknown-linux-musl` |

Each archive has root entries `ww`, `fhs/`, `REVISION`, `PROVENANCE.json` and
`SHA256SUMS`. `ww` is a nonempty native executable; `fhs/` is the complete canonical
staged filesystem. All five FHS file inventories must match exactly. No Host or
FHS from a previous run or attempt is admitted.

After validating those artifacts and booting the container fixtures, job
`validate-release` (display name `Validate release packages and container boot`)
emits a sixth standard ZIP artifact named `release-source`, containing:

- `VERSION`
- `REVISION`
- `CHANGELOG.md`
- `scripts/install.sh`
- `PROVENANCE.json`
- `SHA256SUMS`
- Optional compiled `examples/<example>/bin/*.wasm` from the current WASM build

`REVISION` is the current 40-character lowercase source SHA followed by LF.
`PROVENANCE.json` contains exactly `repository`, `revision` and `builder`.
Repository is `https://github.com/wetware/ww`; revision matches `REVISION`;
builder is `github-actions:RUN_ID:RUN_ATTEMPT:TARGET`. Source input target is
`release-source`; Host targets appear in the table. `SHA256SUMS` inventories every
regular file except itself, including provenance and revision, sorted by ASCII
relative POSIX path. Each line is lowercase SHA-256, two spaces, path and LF.
Symlinks, special files, missing entries, duplicates and extra files are invalid.
These are build inputs and receipts, not an assembled release or authorization.

Private admission authenticates the successful producer run, attempt, source SHA,
job results and each exact artifact ID and API digest before bounded extraction.
It validates complete checksums, provenance and identical FHS, and assembles the
unsigned candidate using its own pinned tooling. It never executes a tool from a
producer archive. Missing, substituted, stale or inconsistent inputs fail closed.
Public CI has no release packager and emits no `release-input.car`.

The existing tested container image remains a separate raw `tested-image.tar`
artifact. Its image job still requires the exact tested digest. This change does
not enable release publication or modify the image job's admission conditions.

## Private signed handoff

Private metadata allocation, preparation, canonical-seed signing and finalization
produce the authenticated release graph. CAR transport preserves block bytes and
CIDs. The private signer exports the signed graph and receipt as separate raw
artifacts. The publication job checks exact artifact IDs/digests and signing-job
identity, imports and authenticates the CAR using independent trust, then checks
the receipt before deriving a publication request. The signed handoff does not
accept ZIPs or introduce fallback extraction.

Operational definitions, the single durable publisher journal and activation
gates remain in [infra's publisher contract](https://github.com/wetware/infra/blob/master/ops/vps/publisher-contract.md).
Neither a source merge nor artifact production grants production key provisioning,
secret upload, service changes or R1 publication authority.
