# Public package and private publisher boundary

Public `wetware/ww` owns deterministic release formats, authentication, offline
package construction, independent verification and consumer freshness. Private
`wetware/infra` owns official publication policy, the single durable Owner/journal,
recovery, Kubo announcement and the protected signing/publication workflow.
Production publication remains held; neither source merge grants activation.

## Public offline tool

```sh
cargo build --release --locked -p ww-release --features package --bin ww-release-package
ww-release-package prepare --tree release-tree --spool release-spool --metadata metadata.json
scripts/sign_release.sh release-tree/CHECKSUMS.txt release-tree/CHECKSUMS.txt.sig private.pem trusted-keys
ww-release-package finalize --tree release-tree --spool release-spool --trusted-keys trusted-keys
ww-release-package verify --spool release-spool --trusted-keys trusted-keys
```

`Metadata` is explicit caller input. Official sequence allocation and original
producer receipt admission belong to infra. The reusable signing script verifies
its output against independent public trust. The caller owns workflow admission
and signing-key access. Public builds/tests use disposable fixture keys only.

Preparation constructs the canonical descriptor and binding before signing.
Finalization verifies signatures and payloads before returning an immutable root.
Finalization and verification emit only `root`, `descriptor` and `sequence`.
Private request IDs, stage counts, journal generations and retention policy are
not public package interfaces.

## Artifact handoff

Public CI exports `validated-release`: the validated release tree, five original
Host receipt sets, public package tool and tested image. Infra selects an exact
successful public workflow run, attempt, source revision, artifact ID and digest.
Infra preserves each Host's actual producer provenance. Its protected signer
uses a separately pinned public tool revision and independent trust.

The final private manifest pins both public crates to one exact Git SHA and owns
its Cargo lockfile and root patches. Public code never fetches private source or
uses private credentials. Read-only artifact access, artifact availability and
protected signing custody require separate operational qualification.

The installed version-1 request/result ABI moved with the private executable.
Its definitions, single-journal rules, restricted transport and activation gates
are maintained in [infra's publisher contract](https://github.com/wetware/infra/blob/master/ops/vps/publisher-contract.md)
and [private executable reference](https://github.com/wetware/infra/blob/master/tools/release-publisher/README.md).
No production request/result schema or journal format change accompanies the move.
