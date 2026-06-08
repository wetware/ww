#!/usr/bin/env bash
# Static checks for the release IPFS publish script and workflow wiring.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PUBLISH_SCRIPT="$ROOT_DIR/scripts/ipfs_publish_release.sh"
WORKFLOW="$ROOT_DIR/.github/workflows/rust.yml"
MAKEFILE="$ROOT_DIR/Makefile"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

line_number() {
  local pattern="$1"
  local file="$2"
  awk -v pat="$pattern" 'index($0, pat) { print NR; exit }' "$file"
}

bash -n "$PUBLISH_SCRIPT"
bash -n "$0"

grep -Fq 'ipfs add --pin=false -rQ --cid-version=1' "$PUBLISH_SCRIPT" \
  || fail "release script must disable implicit pins during ipfs add"
grep -Fq 'WW_RELEASE_PIN_STATE:-/data/ipfs/ww-release-pins.txt' "$PUBLISH_SCRIPT" \
  || fail "release script must keep a durable CI-managed pin state file"
grep -Fq 'WW_RELEASE_PIN_RETAIN:-10' "$PUBLISH_SCRIPT" \
  || fail "release script must default to a small rollback window"
grep -Fq "[ ! -f \"\$state_file\" ]" "$PUBLISH_SCRIPT" \
  || fail "release script must handle first run without bulk cleanup"
grep -Fq 'ipfs repo gc' "$PUBLISH_SCRIPT" \
  || fail "release script must run repo gc after stale unpins"
# shellcheck disable=SC2016
grep -Fq 'POD_RELEASE_TREE:-/tmp/ww-release-tree-publish-$(date +%s)-$$' "$PUBLISH_SCRIPT" \
  || fail "release script must use a unique pod staging path"
grep -Fq 'k cp --retries=3' "$PUBLISH_SCRIPT" \
  || fail "release script must retry kubectl cp under slow k3s API behavior"

pin_add_line="$(line_number "ipfs pin add \"\$CID\"" "$PUBLISH_SCRIPT")"
publish_line="$(line_number "ipfs name publish --key=ww-release \"/ipfs/\$CID\"" "$PUBLISH_SCRIPT")"
pin_rm_line="$(line_number "ipfs pin rm \"\$stale\"" "$PUBLISH_SCRIPT")"

[ -n "$pin_add_line" ] || fail "release script missing explicit pin add"
[ -n "$publish_line" ] || fail "release script missing IPNS publish"
[ -n "$pin_rm_line" ] || fail "release script missing stale pin removal"
[ "$pin_add_line" -lt "$publish_line" ] \
  || fail "release script must pin the new CID before publishing IPNS"
[ "$publish_line" -lt "$pin_rm_line" ] \
  || fail "release script must not remove stale pins before IPNS publish succeeds"

grep -Fq 'scripts/ipfs_publish_release.sh' "$WORKFLOW" \
  || fail "workflow must call the checked-in release publish script"
grep -Fq 'Retained release CIDs' "$WORKFLOW" \
  || fail "workflow summary must include retained release CID count"
grep -Fq 'Stale release CIDs unpinned' "$WORKFLOW" \
  || fail "workflow summary must include stale unpin count"
grep -Fq 'Failed managed unpins' "$WORKFLOW" \
  || fail "workflow summary must include failed managed unpins"
grep -Fq 'Repo size before' "$WORKFLOW" \
  || fail "workflow summary must include repo stat before publish"
grep -Fq 'Repo size after' "$WORKFLOW" \
  || fail "workflow summary must include repo stat after cleanup"

grep -Fq 'ipfs add --pin=false -r --cid-version=1 -Q' "$MAKEFILE" \
  || fail "publish-std must disable implicit ipfs add pinning"
grep -Fq 'ipfs add --pin=false -rQ --cid-version=1' "$MAKEFILE" \
  || fail "publish must disable implicit ipfs add pinning"
grep -Fq "ipfs pin add \"\$\$CID\"" "$MAKEFILE" \
  || fail "Makefile durable publish paths must pin explicitly"

echo "PASS: IPFS release publish checks"
