# Legacy IPFS publication hold

`.github/workflows/rust.yml` keeps `jobs.publish.if: ${{ false }}`. The held job
prepares signed release artifacts and then refuses infrastructure publication.
The obsolete official publisher scripts are removed. `make publish` refuses
before builds or external calls. `publish-std` remains a separate developer
command; it does not authorize official release publication.

This repository condition does not fence historical workflows, copied scripts,
authenticated sessions, or operations already accepted by Kubo. Infra owns the
[operational fence and activation runbook](https://github.com/wetware/infra/blob/master/ops/vps/publication-transition.md)
and the [current production handoff](https://github.com/wetware/infra/blob/master/ops/vps/steady-state.md).
The migrated Kubo node and release identities must be preserved. Publication,
DNSLink changes, key provisioning, and `/install` activation require separate
approval. GHCR image publication is not VPS deployment.

Run `bash tests/test_release_workflow.sh` and
`bash tests/test_ipfs_release_publish.sh`. The runtime test checks that the
historical entry point refuses before any build or external command. The workflow
test rejects restoration of the former publication condition. These tests use
no production service.
