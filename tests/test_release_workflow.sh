#!/usr/bin/env bash
# Static regression checks for the manual ww-master promotion boundary.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORKFLOW="$ROOT_DIR/.github/workflows/rust.yml"

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
grep -Fq 'push: true' <<<"$release_job" \
  || fail "image-release job must push the published image"
grep -Fq 'ghcr.io/wetware/ww:master' <<<"$release_job" \
  || fail "image-release job must retain the master image tag"
grep -Fq "ghcr.io/wetware/ww:master-\${{ github.sha }}" <<<"$release_job" \
  || fail "image-release job must retain the commit-addressable image tag"

grep -Fq 'needs: [changes, test, build-binaries, build-wasm, release_image]' "$WORKFLOW" \
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
pod_selection='POD="$(select_ready_ipfs_pod)"'
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
grep -Fq 'fetch_previous_binaries()' "$WORKFLOW" \
  || fail "IPFS publisher must retry prior-release binary staging"
grep -Fq 'if ! fetch_previous_binaries; then' "$WORKFLOW" \
  || fail "IPFS publisher must fail closed when prior-release staging cannot complete"
staged_binary='"$STAGE/bin/ww/$platform/ww.next"'
grep -Fq "$staged_binary" "$WORKFLOW" \
  || fail "IPFS publisher must stage prior-release binaries atomically"

echo "PASS: ww release workflow builds images without directly deploying ww-master"
