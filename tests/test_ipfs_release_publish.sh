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
# shellcheck disable=SC2016
grep -Fq 'tar -C "$REMOTE_RELEASE_TREE" -cf - .' "$PUBLISH_SCRIPT" \
  || fail "release script must stream the release tree without kubectl cp"
# shellcheck disable=SC2016
grep -Fq 'k exec -i "$POD"' "$PUBLISH_SCRIPT" \
  || fail "release script must unpack the release tree through kubectl exec stdin"
grep -Fq 'WW_RELEASE_EXPECTED_CURRENT is required' "$PUBLISH_SCRIPT" \
  || fail "release script must require the previously observed IPNS release"
# shellcheck disable=SC2016
grep -Fq 'current_release="$(resolve_ww_release' "$PUBLISH_SCRIPT" \
  || fail "release script must re-resolve IPNS immediately before publishing"
# shellcheck disable=SC2016
grep -Fq 'current_release" != "$EXPECTED_CURRENT' "$PUBLISH_SCRIPT" \
  || fail "release script must fail when the IPNS compare-and-set precondition changes"

pin_add_line="$(line_number "ipfs pin add '\$CID'" "$PUBLISH_SCRIPT")"
# shellcheck disable=SC2016
cas_line="$(line_number 'current_release="$(resolve_ww_release' "$PUBLISH_SCRIPT")"
publish_line="$(line_number "ipfs name publish --key=ww-release '/ipfs/\$CID'" "$PUBLISH_SCRIPT")"
pin_rm_line="$(line_number "ipfs pin rm \"\$stale\"" "$PUBLISH_SCRIPT")"

[ -n "$pin_add_line" ] || fail "release script missing explicit pin add"
[ -n "$cas_line" ] || fail "release script missing IPNS compare-and-set check"
[ -n "$publish_line" ] || fail "release script missing IPNS publish"
[ -n "$pin_rm_line" ] || fail "release script missing stale pin removal"
[ "$pin_add_line" -lt "$publish_line" ] \
  || fail "release script must pin the new CID before publishing IPNS"
[ "$cas_line" -lt "$publish_line" ] \
  || fail "release script must re-check the current IPNS release before publishing"
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
grep -Fq 'queue: max' "$WORKFLOW" \
  || fail "release concurrency must retain every pending master run"
publish_job="$({
  awk '
    /^  publish:/ { in_job = 1 }
    in_job { print }
  ' "$WORKFLOW"
})"
grep -Fq 'fetch-depth: 0' <<<"$publish_job" \
  || fail "release ancestry checks require full Git history"
grep -Fq 'scripts/check_release_ancestry.sh' "$WORKFLOW" \
  || fail "workflow must enforce Git ancestry before publishing IPNS"
# shellcheck disable=SC2016
grep -Fq 'echo "${{ github.sha }}" > "$STAGE/REVISION"' "$WORKFLOW" \
  || fail "published release trees must record their Git revision"
grep -Fq "WW_RELEASE_EXPECTED_CURRENT='\$CURRENT_RELEASE'" "$WORKFLOW" \
  || fail "workflow must pass the observed IPNS release into the remote compare-and-set check"

grep -Fq 'ipfs add --pin=false -r --cid-version=1 -Q' "$MAKEFILE" \
  || fail "publish-std must disable implicit ipfs add pinning"
grep -Fq 'ipfs add --pin=false -rQ --cid-version=1' "$MAKEFILE" \
  || fail "publish must disable implicit ipfs add pinning"
grep -Fq "ipfs pin add \"\$\$CID\"" "$MAKEFILE" \
  || fail "Makefile durable publish paths must pin explicitly"

echo "PASS: IPFS release publish checks"
