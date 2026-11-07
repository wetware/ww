#!/usr/bin/env bash
# Official release publication uses the dedicated Rust owner and restricted worker.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
workflow="$root/.github/workflows/rust.yml"
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
for script in ipfs_publish_release.sh publish_local_release.sh; do
  test ! -e "$root/scripts/$script" || fail 'obsolete publisher entry point remains'
done
for path in src/bin/ww-release-publisher.rs src/release/package.rs src/release/publisher.rs crates/ipfs/src/release/mod.rs tests/test_release_publisher.py; do
  test ! -e "$root/$path" || fail "private publication source remains public: $path"
done
if grep -Eq '^pub mod release;' "$root/crates/ipfs/src/lib.rs"; then
  fail 'IPFS still exports the private release Owner'
fi
python3 - "$root/Cargo.toml" <<'PY'
import json
import subprocess
import sys

metadata = json.loads(subprocess.check_output([
    'cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1',
    '--manifest-path', sys.argv[1],
]))
members = set(metadata['workspace_members'])
for package in metadata['packages']:
    if package['id'] not in members:
        continue
    for target in package['targets']:
        assert target['name'] != 'ww-release-publisher', 'public publisher target remains'
    for dependency in package['dependencies']:
        assert dependency['name'] != 'ww-release-publisher', 'public package depends on private publisher'
        assert 'wetware/infra' not in (dependency.get('source') or ''), 'public package depends on private infrastructure'
PY
grep -Fq 'queue: max' "$workflow" || fail 'release concurrency queue missing'
grep -Fq 'ww-release-package' "$workflow" || fail 'public offline package tool not built or validated'
if grep -Fq 'ww-release-publisher' "$workflow"; then
  fail 'public workflow retains the private publisher'
fi
grep -Fq 'Refuse unintegrated infrastructure publication' "$workflow" || fail 'infrastructure handoff refusal missing'
if grep -Eq '\b(kubectl|rsync)\b|pin rm|repo gc|ww-release-pins.txt' "$workflow"; then
  fail 'workflow retains legacy publication or pruning'
fi
bash "$root/tests/test_ipfs_release_publish_runtime.sh"
echo 'PASS: official release publisher boundary'
