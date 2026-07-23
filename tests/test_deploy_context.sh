#!/usr/bin/env bash
# Static checks for the production image's status-cell packaging contract.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORKFLOW="$ROOT_DIR/.github/workflows/rust.yml"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

line_number() {
  local pattern="$1"
  awk -v pat="$pattern" 'index($0, pat) { print NR; exit }' "$WORKFLOW"
}

bash -n "$0"

grep -Fq 'mkdir -p wetware/kernel/bin wetware/kernel/etc/init.d' "$WORKFLOW" \
  || fail "deploy context must create the kernel init.d directory"
grep -Fq 'cp wasm/std/status/bin/status.wasm wetware/kernel/bin/status.wasm' "$WORKFLOW" \
  || fail "deploy context must package the status WASM"
grep -Fq 'cp ../std/status/etc/init.d/05-status.glia wetware/kernel/etc/init.d/' "$WORKFLOW" \
  || fail "deploy context must package the status init script"

if grep -Fq './ww compile' "$WORKFLOW" || grep -Fq 'cwasm/' "$WORKFLOW"; then
  fail "deploy context must contain portable WASM only"
fi

echo "PASS: deploy context packages the status route without native artifacts"
