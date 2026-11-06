#!/usr/bin/env bash
# Exercise the real offline CLI with ephemeral trust and existing matrix artifacts.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
publisher="$(cd "$(dirname "${1:?publisher executable}")" && pwd)/$(basename "$1")"
artifacts="${2:?producer artifacts}"
tree="${3:?validated release tree}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp -R "$tree" "$work/tree"
channel=$(python3 - <<'PY'
value = int.from_bytes(bytes.fromhex('0172002408011220') + bytes([1]) * 32, 'big')
result = ''
while value:
    value, digit = divmod(value, 36)
    result = '0123456789abcdefghijklmnopqrstuvwxyz'[digit] + result
print('k' + result)
PY
)
"$publisher" metadata --artifacts "$artifacts" --tree "$work/tree" \
  --channel "$channel" --sequence 1 --builder qualification-fixture > "$work/metadata.json"
"$publisher" prepare --tree "$work/tree" --spool "$work/spool" --metadata "$work/metadata.json"
PYTHONDONTWRITEBYTECODE=1 python3 "$root/tests/release_auth_fixture.py" "$work/keys" "$work/tree/CHECKSUMS.txt"
"$publisher" finalize --tree "$work/tree" --spool "$work/spool" --trusted-keys "$work/keys/trusted-keys" > "$work/result"
"$publisher" verify --spool "$work/spool" --trusted-keys "$work/keys/trusted-keys" > "$work/verified"
cmp "$work/result" "$work/verified"
# Existing installer verifier consumes the exact same manifest and signature.
sh "$root/scripts/install.sh" --verify-manifest "$work/tree/CHECKSUMS.txt" \
  --signature "$work/tree/CHECKSUMS.txt.sig" --trusted-release-keys "$work/keys/trusted-keys"
printf 'changed Host\n' >> "$work/tree/bin/ww/linux/x86_64/ww"
if "$publisher" finalize --tree "$work/tree" --spool "$work/spool" --trusted-keys "$work/keys/trusted-keys" > "$work/failure" 2>&1; then
  echo 'FAIL: changed signed artifact accepted' >&2
  exit 1
fi
echo 'PASS: descriptor CLI, five platforms, legacy authentication, and tampered payload refusal'
