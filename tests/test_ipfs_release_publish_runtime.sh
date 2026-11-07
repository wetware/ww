#!/usr/bin/env bash
# Legacy official release entry points must fail before any external tool call.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
test_dir="$(mktemp -d)"
trap 'rm -rf "$test_dir"' EXIT
mkdir "$test_dir/bin"
for tool in ipfs kubectl ssh rsync curl cargo python3; do
  cat > "$test_dir/bin/$tool" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$0 $*" >> "$WW_TEST_EXTERNAL_CALLS"
exit 97
STUB
  chmod +x "$test_dir/bin/$tool"
done
export WW_TEST_EXTERNAL_CALLS="$test_dir/calls"
for script in ipfs_publish_release.sh publish_local_release.sh; do
  test ! -e "$ROOT_DIR/scripts/$script"
done
if PATH="$test_dir/bin:$PATH" make -C "$ROOT_DIR" publish > "$test_dir/output" 2>&1; then
  echo 'FAIL: legacy official publisher succeeded' >&2
  exit 1
fi
grep -Fq 'ww-release-publisher' "$test_dir/output"
test ! -e "$test_dir/calls"
echo 'PASS: legacy publication refuses before build or external calls'
