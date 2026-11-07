#!/usr/bin/env bash
# Exercise the real offline CLI with ephemeral trust and existing matrix artifacts.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
package_tool="$(cd "$(dirname "${1:?public package executable}")" && pwd)/$(basename "$1")"
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
# Explicit test-only Metadata input. Receipt admission and sequence allocation
# are private operator policy; this fixture only supplies protocol fields.
python3 - "$artifacts" "$work/tree" "$channel" > "$work/metadata.json" <<'PY_METADATA'
import json
import pathlib
import sys
artifacts, tree = map(pathlib.Path, sys.argv[1:3])
platforms = {
    "linux-x86_64": "x86_64-unknown-linux-gnu",
    "linux-aarch64": "aarch64-unknown-linux-gnu",
    "macos-aarch64": "aarch64-apple-darwin",
    "macos-x86_64": "x86_64-apple-darwin",
    "linux-musl-x86_64": "x86_64-unknown-linux-musl",
}
json.dump({
    "channel": sys.argv[3], "sequence": 1,
    "version": (tree / "VERSION").read_text().strip(),
    "source": {"repository": "https://github.com/wetware/ww",
               "revision": (tree / "REVISION").read_text().strip(),
               "builder": "qualification-fixture"},
    "hosts": {target: json.loads((artifacts / ("ww-" + platform) / "PROVENANCE.json").read_text())
              for platform, target in platforms.items()},
}, sys.stdout)
PY_METADATA
"$package_tool" prepare --tree "$work/tree" --spool "$work/spool" --metadata "$work/metadata.json"
PYTHONDONTWRITEBYTECODE=1 python3 "$root/tests/release_auth_fixture.py" "$work/keys" "$work/tree/CHECKSUMS.txt"
"$package_tool" finalize --tree "$work/tree" --spool "$work/spool" --trusted-keys "$work/keys/trusted-keys" > "$work/result"
"$package_tool" verify --spool "$work/spool" --trusted-keys "$work/keys/trusted-keys" > "$work/verified"
cmp "$work/result" "$work/verified"
# Existing installer verifier consumes the exact same manifest and signature.
sh "$root/scripts/install.sh" --verify-manifest "$work/tree/CHECKSUMS.txt" \
  --signature "$work/tree/CHECKSUMS.txt.sig" --trusted-release-keys "$work/keys/trusted-keys"
printf 'changed Host\n' >> "$work/tree/bin/ww/linux/x86_64/ww"
if "$package_tool" finalize --tree "$work/tree" --spool "$work/spool" --trusted-keys "$work/keys/trusted-keys" > "$work/failure" 2>&1; then
  echo 'FAIL: changed signed artifact accepted' >&2
  exit 1
fi
echo 'PASS: descriptor CLI, five platforms, legacy authentication, and tampered payload refusal'
