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

grep -Fq 'release_image:' "$WORKFLOW" \
  || fail "workflow must retain the ww-master image build job"
grep -Fq 'needs: [changes, test, build-binaries, build-wasm, release_image]' "$WORKFLOW" \
  || fail "IPFS publication must wait for the image build, not a deployment"
grep -Fq 'name: Publish to IPFS' "$WORKFLOW" \
  || fail "workflow must retain IPFS publication"

! grep -Fq 'name: Deploy to VPS' "$WORKFLOW" \
  || fail "ww CI must not deploy ww-master directly"
! grep -Fq 'kubectl set image deployment/ww-master' "$WORKFLOW" \
  || fail "ww CI must not mutate the ww-master image"
! grep -Fq 'kubectl rollout status deployment/ww-master' "$WORKFLOW" \
  || fail "ww CI must not wait on a ww-master rollout"

echo "PASS: ww release workflow builds images without directly deploying ww-master"
