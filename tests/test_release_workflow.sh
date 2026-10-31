#!/usr/bin/env bash
# Static regression checks for the publication hold and manual promotion boundary.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORKFLOW="${1:-$ROOT_DIR/.github/workflows/rust.yml}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

bash -n "$0"

release_job="$({
  awk '
    /^  release_image:/ { in_job = 1 }
    /^  publish:/ { in_job = 0 }
    in_job { print }
  ' "$WORKFLOW"
})"
non_publish_workflow="$({
  awk '
    /^  publish:/ { in_publish = 1; next }
    in_publish && /^  [[:alnum:]_-]+:/ { in_publish = 0 }
    !in_publish { print }
  ' "$WORKFLOW"
})"

[[ -n "$release_job" ]] \
  || fail "workflow must retain the ww-master image build job"

# `ww-master` must be named only by the image-release job. Any additional
# reference is a likely reintroduction of a deployment, rollout, or mutation
# path that this repository must not own.
ww_master_refs="$(grep -F 'ww-master' "$WORKFLOW" || true)"
[[ "$ww_master_refs" == '    name: Build and publish ww-master image' ]] \
  || fail "workflow must not reference ww-master outside the image-release job"

# The release job is allowed to assemble a deploy image, but it must remain an
# image publisher—not an indirect deployment wrapper.
if grep -Eq '\b(kubectl|ssh|scp|rsync|rollout|patch|scale)\b|set[[:space:]]+image' <<<"$release_job"; then
  fail "image-release job must not contain a deployment transport or mutation command"
fi

# The IPFS publisher is the only workflow job that retains VPS transport for
# this POC. Keeping SSH/kubectl out of every other job prevents a new deploy
# job, namespaced mutation, or wrapper from silently restoring CI ownership of
# ww-master.
if grep -Eq '\b(kubectl|ssh|scp|rsync)\b' <<<"$non_publish_workflow"; then
  fail "only the IPFS publisher may use remote or Kubernetes transport"
fi

grep -Fq "github.event_name == 'workflow_dispatch'" <<<"$release_job" \
  || fail "image-release job must remain reachable via workflow dispatch"
grep -Fq "github.event_name == 'push' && github.ref == 'refs/heads/master'" <<<"$release_job" \
  || fail "image-release job must remain reachable on master pushes"
grep -Fq 'docker push ghcr.io/wetware/ww:master' <<<"$release_job" \
  || fail "image-release job must push the published image"
grep -Fq 'ghcr.io/wetware/ww:master' <<<"$release_job" \
  || fail "image-release job must retain the master image tag"
grep -Fq "ghcr.io/wetware/ww:master-\${{ github.sha }}" <<<"$release_job" \
  || fail "image-release job must retain the commit-addressable image tag"

grep -Fq 'needs: [changes, test, validate-release, release_image]' "$WORKFLOW" \
  || fail "IPFS publication must wait for the image build, not a deployment"
grep -Fq 'name: Publish to IPFS' "$WORKFLOW" \
  || fail "workflow must retain IPFS publication"
grep -Fq 'get pod -l app=ipfs-daemon --field-selector=status.phase=Running -o json | jq -r' "$WORKFLOW" \
  || fail "IPFS publisher must consider only Running daemon pods"
grep -Fq 'select(.metadata.deletionTimestamp | not)' "$WORKFLOW" \
  || fail "IPFS publisher must exclude terminating daemon pods"
grep -Fq 'any(.status.conditions[]?; .type == \"Ready\" and .status == \"True\")' "$WORKFLOW" \
  || fail "IPFS publisher must select a Ready daemon pod"
retry_loop="$({
  awk '
    /for attempt in 1 2 3; do/ { in_loop = 1 }
    in_loop { print }
    in_loop && /^        done$/ { exit }
  ' "$WORKFLOW"
})"
pod_selection="POD=\"\$(select_ready_ipfs_pod)\""
grep -Fq "$pod_selection" <<<"$retry_loop" \
  || fail "IPFS publisher must reselect a Ready daemon pod for each retry"
publish_retry_selection="$({
  awk '
    /Re-select immediately before publish/ { capture = 1 }
    capture { print }
    capture && /else/ { exit }
  ' "$WORKFLOW"
})"
grep -Fq "$pod_selection" <<<"$publish_retry_selection" \
  || fail "IPFS publisher must reselect a Ready daemon pod before each publish retry"
if grep -Fq 'fetch_previous_binaries' "$WORKFLOW"; then
  fail "std-only releases must consume the current run's Host binaries"
fi
for condition in "needs.test.result == 'success'" "needs.validate-release.result == 'success'"; do
  grep -Fq "$condition" <<<"$release_job" || fail "GHCR must fail closed on $condition"
done
# The hold supersedes the former success-dependent publication condition.
# Match the job-level condition, not a comment or a step-level `if`.
publish_condition="$(awk '
  /^  publish:/ { in_job = 1; next }
  in_job && /^  [[:alnum:]_-]+:/ { exit }
  in_job && /^    if:/ { print }
' "$WORKFLOW")"
# Match the literal GitHub expression; Bash must not expand it.
# shellcheck disable=SC2016
[[ "$publish_condition" == '    if: ${{ false }}' ]] \
  || fail 'IPFS publish must remain unconditionally disabled for every event'
grep -Fq 'name: Validate actual packaged installation and startup' "$WORKFLOW" || fail 'installer boot validation missing'
grep -Fq 'bash tests/test_container_boot.sh ww-validated:deploy' "$WORKFLOW" || fail 'deploy boot gate missing'
grep -Fq 'bash tests/test_container_boot.sh ww-validated:release' "$WORKFLOW" || fail 'release boot gate missing'
grep -Fq 'bash tests/test_container_boot.sh ww-validated:source' "$WORKFLOW" || fail 'source boot gate missing'
grep -Fq 'docker load -i tested-image.tar' <<<"$release_job" || fail 'GHCR must publish the tested image'
grep -Fq "            - 'scripts/**'" "$WORKFLOW" || fail 'installer-only changes must trigger release validation'
grep -Fq 'linux/musl-x86_64' "$WORKFLOW" || fail 'IPFS must include fifth Host target'

# Freeze the existing job-level admission controls during the maintenance hold.
# This covers ordinary Rust/metrics CI, release validation, GHCR and Pages.
# YAML/action validation is separate; this check follows the workflow's layout.
actual_admission="$(awk '
  /^jobs:/ { in_jobs = 1; next }
  !in_jobs { next }
  /^  publish:/ { in_publish = 1; next }
  /^  [[:alnum:]_-]+:/ { in_publish = 0; print; in_condition = 0; next }
  in_publish { next }
  /^    (if|needs):/ { print; in_condition = ($0 == "    if: >-"); next }
  in_condition && /^      / { print; next }
  { in_condition = 0 }
' "$WORKFLOW")"
expected_admission="$({
  cat <<'ADMISSION'
  changes:
    if: >-
      (github.event_name == 'push' && github.ref == 'refs/heads/master')
      || github.event_name == 'workflow_dispatch'
  lockfiles:
  test-architecture-map:
  fmt:
  fuzz-corpus:
    needs: lockfiles
  clippy:
    needs: lockfiles
  solidity:
  build:
    needs: lockfiles
  build-wasm:
    needs: lockfiles
  test-host:
    needs: [build, build-wasm]
  test-stdlib:
    needs: lockfiles
  test:
    needs: [test-host, test-stdlib]
    if: ${{ always() }}
  build-architecture-map:
    if: >-
      github.event_name == 'workflow_dispatch'
      || (github.event_name == 'push' && github.ref == 'refs/heads/master'
          && needs.changes.outputs.architecture_map == 'true')
    needs: [changes, fmt, clippy, solidity, build, test, test-architecture-map]
  deploy-architecture-map:
    if: ${{ needs.build-architecture-map.result == 'success' }}
    needs: build-architecture-map
  build-binaries:
    if: >-
      github.event_name == 'workflow_dispatch'
      || (github.event_name == 'push' && github.ref == 'refs/heads/master'
          && (needs.changes.outputs.std == 'true' || needs.changes.outputs.host == 'true'))
    needs: [changes, fmt, clippy, solidity, build, build-wasm]
  validate-release:
    if: >-
      github.ref == 'refs/heads/master'
      && (github.event_name == 'workflow_dispatch'
          || needs.changes.outputs.std == 'true' || needs.changes.outputs.host == 'true')
    needs: [changes, test, build-binaries, build-wasm]
  release_image:
    if: >-
      !cancelled() && github.ref == 'refs/heads/master'
      && needs.test.result == 'success' && needs.validate-release.result == 'success'
      && (github.event_name == 'workflow_dispatch'
          || (github.event_name == 'push' && github.ref == 'refs/heads/master'
              && (needs.changes.outputs.std == 'true' || needs.changes.outputs.host == 'true')))
    needs: [changes, test, validate-release]
ADMISSION
})"
[[ "$actual_admission" == "$expected_admission" ]] \
  || fail 'non-publish job admission controls changed during the publication hold'

echo "PASS: ww release workflow builds images without directly deploying ww-master"
