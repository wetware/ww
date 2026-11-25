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
artifact. Its image job still requires the exact tested digest.

### CI tiers

| Entry point | Checks and outputs |
| --- | --- |
| PRs and pushes to `master` or `staging` (`fast.yml`) | Repository artifacts and lockfiles, formatting, Clippy, Foundry, fuzz corpus, architecture map, offline release and installer controls, standalone standard library, and selected native Rust tests. The `Test` job requires every fast check to succeed. |
| Full dispatch (`rust.yml`) | Fast checks plus workspace build and Host tests with Kubo and Foundry, WASI P3 fixtures and guest cancellation, canonical FHS, packaged installation and boot, all five native producers, and source and artifact container boots. |
| Master PRs (`changelog.yml`) | The existing `Require changelog entry` check runs independently. |

`scripts/test_fast_rust.sh` selects native library and integration targets.
Full Host tests require generated first-party components, the native P3,
authority, Gate 2B, routing-key and stream-lifecycle probes, and a live Kubo API.
Full CI checks the downloaded fixture paths before running the workspace suite.

### Selecting a producer run

Dispatch `rust.yml` on a topic branch with the reviewed 40-character commit
SHA in `expected_sha` for a rehearsal. The default `validation_only=true`
disables Pages and GHCR. A topic branch can point to the intended master SHA.
For official producer inputs, dispatch on `master` with that exact SHA and
explicitly set `validation_only=false`. The admission job compares the event
SHA with the requested SHA before checkout. It rejects tags, master rehearsals
and image-producing requests on other branches. Master pushes can update
Pages through `fast.yml`; master image updates require the official dispatch.

The private tag selector requires exactly one successful `rust.yml` push or
dispatch for the tagged SHA on master before it inspects jobs. Routine checks
use a separate top-level workflow path. Before an official dispatch, check for
an existing successful producer run for the SHA, including historical master
pushes. A second success makes selection ambiguous. Retry a **failed** run
with a fresh dispatch. Admission, artifact validation and image loading require
attempt 1. This excludes partial and full reruns that could reuse earlier job
results or artifacts. Avoid rerunning a successful producer.

After admission and lockfile checks, WASM and native test compilation run in
parallel with fast checks. Source-container qualification starts after WASM
production, independently of the five native producers and Host tests.
`validate-release` requires successful tests, WASM, producers and source boot
for the current run before comparing bytes, booting artifact containers and
emitting source receipts. The GHCR job checks the source attempt before
downloading the digest-verified tested image. Private admission requires
successful jobs named `Test`, `Validate release packages and container boot`
and `Build and publish ww-master image`, then verifies the six producer
artifacts against the revision, run and attempt. Authority to sign and publish
remains in private infrastructure under the existing holds.

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
