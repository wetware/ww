#!/usr/bin/env bash
# Official release publication uses the dedicated Rust owner and restricted worker.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
workflow="$root/.github/workflows/rust.yml"
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
for script in ipfs_publish_release.sh publish_local_release.sh; do
  test ! -e "$root/scripts/$script" || fail 'obsolete publisher entry point remains'
done
grep -Fq 'queue: max' "$workflow" || fail 'release concurrency queue missing'
grep -Fq 'scripts/check_release_ancestry.sh' "$workflow" || fail 'source ancestry admission missing'
grep -Fq 'ww-release-publisher' "$workflow" || fail 'dedicated publisher not built or validated'
grep -Fq 'Refuse unintegrated infrastructure publication' "$workflow" || fail 'infrastructure handoff refusal missing'
if grep -Eq '\b(kubectl|rsync)\b|pin rm|repo gc|ww-release-pins.txt' "$workflow"; then
  fail 'workflow retains legacy publication or pruning'
fi
bash "$root/tests/test_ipfs_release_publish_runtime.sh"
echo 'PASS: official release publisher boundary'
