# Legacy IPFS publication hold

Status: proposed repository hold. Production cutover remains **HOLD**.
This runbook grants no authority to change production, credentials, or CI runs.

## Repository boundary

`.github/workflows/rust.yml` sets `jobs.publish.if` to `${{ false }}`.
Workflow executions using this definition cannot schedule the legacy IPFS job,
including master pushes and manual dispatch. Rust tests, formatting, Clippy,
metrics tests, release builds, release validation, and GHCR publication retain
their existing conditions. The legacy publisher scripts remain available.

The condition does **not** fence queued or running executions using older
workflow definitions, historical reruns, dispatch to refs without the hold,
existing SSH sessions, or operations already accepted by Kubo. GitHub reruns
use the original event's commit and ref; merging the hold does not rewrite them.
See GitHub's [job condition documentation](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idif)
and [rerun documentation](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/re-run-workflows-and-jobs).

The operator has approved replacing the old IPNS identity. This supersedes
the identity-preservation requirement recorded in
[wetware/infra#38](https://github.com/wetware/infra/issues/38).
Identity replacement does not revoke old SSH access or stop old writers.
This change creates no replacement identity and changes no discovery pointers.

## Separate operational fence

Execute these steps only in a separately authorized operational session.
Keep evidence private and omit secrets and private keys.

1. **Inventory workflow executions.** Record the hold commit and inspect all
   queued, pending, waiting, and running `rust.yml` runs, including concurrency
   queues and jobs awaiting approval. Record run IDs, attempts, event, ref,
   workflow commit, and publisher job state. Include completed runs that can
   be rerun, and branches or tags that still contain the old definition.
   After separate approval, cancel affected executions and confirm their jobs
   terminated. Cancellation alone does not establish the fence.
2. **Inventory publisher access and processes.** Trace `DEPLOY_SSH_KEY` and
   `VPS_SSH` to the accepted server-side SSH principal without printing secrets.
   Inspect active SSH sessions and their descendants, including detached shells,
   `/tmp/ww-ipfs-publish-release.sh`, `kubectl exec`, and pod-side `ipfs` clients.
   Inventory alternate keys, accounts, agents, kubeconfigs, Kubo API access,
   and copies of the old `ww-release` signing key. Include operator machines.
3. **Remove old access after separate approval.** Revoke or disable the
   server-side publication credential and remove its CI secret reference as
   appropriate. Do not rely on deleting the GitHub secret alone: a running job
   might already hold the key. Confirm rejected new authentication through an
   authorized check. Terminate existing publisher sessions and descendants
   separately; key revocation does not terminate authenticated sessions.
   Maintain a verified independent recovery-access path.
4. **Prevent publisher restart after separate approval.** Inspect Kubernetes
   Jobs, CronJobs, Deployments, StatefulSets, restart policies, and reconciliation
   sources. Also inspect host services, timers, cron, detached processes, and
   container restart policies. Fence their publication authority and disable
   the identified restart paths. Preserve required release content before any
   service retirement. A stopped pod or removed k3s service alone is insufficient.
5. **Account for accepted Kubo operations.** An SSH disconnect, client timeout,
   failed CI job, or cancelled `kubectl exec` does not prove a server operation
   stopped. Inspect old Kubo processes, API requests, logs, and publication
   state. After separate approval, quiesce the old daemon and every process
   with old signing authority when completion cannot be established. Prevent
   restart with that authority. Record the old IPNS value, sequence, and
   observation time after quiescence. Already signed records can propagate
   later; an unchanged cached IPNS resolution is not proof of no pending write.
6. **Check alternate entry points.** `scripts/ipfs_publish_release.sh` can run
   directly. `make publish` calls `scripts/publish_local_release.sh` and can
   advance `ww-release` when its Kubo endpoint has that key. `make publish-std`
   can publish through an explicitly supplied `IPNS_KEY`. Other clients with
   Kubo API access or copies of the signing key can also publish. Fence those
   authorities, including historical CI, before declaring retirement.

## Retirement criteria

Declare the old publication path retired only when an operator has recorded:

- The hold is present on the intended active workflow refs, and affected queued
  and running publisher jobs have terminated.
- Historical reruns and old-ref dispatch cannot authenticate to the old VPS
  publisher, even if their workflow condition permits publication.
- No legacy publisher session, descendant, controller, or restart path retains
  usable publication authority. Alternate credentials and key copies are
  accounted for; an unknown writer keeps the decision at HOLD.
- No old Kubo operation can subsequently create a newer release-pointer record.
  Record how accepted operations were drained or the daemon and signing
  authority were fenced. Account for later propagation of already signed records
  when checking consumers; a quiet observation interval alone is insufficient.
- Required content remains retrievable. Any new identity, DNSLink, installer,
  or consumer transition has separate approval and validation. Replacing the
  identity does not establish that the old path is retired.

GHCR publication remains active and is not a deployment operation in this
workflow. Inventory external image consumers separately before retiring k3s.
Do not re-enable the legacy job as a rollback on a VPS without Kubernetes.
A replacement publisher requires a separate review and approval.

## Local verification

Run `bash tests/test_release_workflow.sh` and the existing IPFS static/runtime,
release-ancestry, and deployment regression tests. The workflow test also accepts
a fixture path: restoring the former publishing condition in a temporary copy
must fail with `IPFS publish must remain unconditionally disabled for every event`.
Validate YAML and Actions syntax separately with `actionlint`.
No validation here requires a production connection or a live publication.
