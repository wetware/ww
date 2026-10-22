#!/usr/bin/env bash
# Exercise release assembly with real validated WASM. Host placeholders isolate
# assembly checks; CI separately installs and boots the actual matrix Hosts.
set -euo pipefail
cd "$(dirname "$0")/.."
validator="$(cd "$(dirname "${1:?validator required}")" && pwd)/$(basename "$1")"
fhs="${2:?canonical FHS required}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
artifacts="$work/artifacts"
revision=1111111111111111111111111111111111111111
fail() { echo "FAIL: $*" >&2; exit 1; }
mkdir -p "$artifacts/wasm-artifacts/std/kernel/bin" "$artifacts/wasm-artifacts/std/status/bin" "$artifacts/wasm-artifacts/target"
cp "$fhs/boot/kernel.wasm" "$artifacts/wasm-artifacts/std/kernel/bin/main.wasm"
cp "$fhs/bin/status.wasm" "$artifacts/wasm-artifacts/std/status/bin/status.wasm"
cp -R "$fhs" "$artifacts/wasm-artifacts/target/fhs"
platforms=(linux-x86_64 linux-aarch64 macos-aarch64 macos-x86_64 linux-musl-x86_64)
for platform in "${platforms[@]}"; do
  mkdir -p "$artifacts/ww-$platform"
  printf 'Host assembly fixture\n' > "$artifacts/ww-$platform/ww"
  cp -R "$fhs" "$artifacts/ww-$platform/fhs"
  printf '%s\n' "$revision" > "$artifacts/ww-$platform/REVISION"
  (cd "$artifacts/ww-$platform" && shasum -a 256 ww > SHA256SUMS)
done
assemble() { WW_FHS_VALIDATOR="$validator" bash scripts/assemble_release.sh "$artifacts" "$work/release" "$revision"; }
assemble
(cd "$work/release" && sed '/^# sha256$/d' CHECKSUMS.txt | sha256sum --check --strict -)
[ "$(find "$work/release/bin/ww" -type f | wc -l | tr -d ' ')" = 5 ] || fail 'release lacks five Hosts'
[ "$(cat "$work/release/REVISION")" = "$revision" ] || fail 'revision missing'
cmp "$fhs/boot/kernel.wasm" "$work/release/fhs/boot/kernel.wasm"
cmp "$fhs/bin/status.wasm" "$work/release/fhs/bin/status.wasm"
rm -rf "$work/release"
for platform in "${platforms[@]}"; do
  mv "$artifacts/ww-$platform/ww" "$work/host"
  if assemble >"$work/failure.log" 2>&1; then fail "missing $platform Host accepted"; fi
  [ ! -e "$work/release" ] || fail 'invalid input created release destination'
  mv "$work/host" "$artifacts/ww-$platform/ww"
done
mv "$artifacts/ww-linux-x86_64/ww" "$work/host"
ln -s "$work/host" "$artifacts/ww-linux-x86_64/ww"
if assemble >"$work/failure.log" 2>&1; then fail 'symlink Host accepted'; fi
rm "$artifacts/ww-linux-x86_64/ww"
: > "$artifacts/ww-linux-x86_64/ww"
if assemble >"$work/failure.log" 2>&1; then fail 'empty Host accepted'; fi
mv "$work/host" "$artifacts/ww-linux-x86_64/ww"
printf '%040d\n' 2 > "$artifacts/ww-macos-x86_64/REVISION"
if assemble >"$work/failure.log" 2>&1; then fail 'stale foreign Host source accepted'; fi
printf '%s\n' "$revision" > "$artifacts/ww-macos-x86_64/REVISION"
printf 'stale foreign Host\n' > "$artifacts/ww-macos-x86_64/ww"
if assemble >"$work/failure.log" 2>&1; then fail 'changed foreign Host accepted'; fi
printf 'Host assembly fixture\n' > "$artifacts/ww-macos-x86_64/ww"
for damage in missing empty corrupt private; do
  payload="$artifacts/ww-linux-x86_64/fhs/bin/status.wasm"
  case "$damage" in
    missing) rm "$payload" ;;
    empty) : > "$payload" ;;
    corrupt) printf 'corrupt\n' > "$payload" ;;
    private) printf 'private\n' > "$artifacts/ww-linux-x86_64/fhs/identity" ;;
  esac
  if assemble >"$work/failure.log" 2>&1; then fail "$damage package accepted"; fi
  [ ! -e "$work/release" ] || fail 'invalid package created release destination'
  cp "$fhs/bin/status.wasm" "$payload"
  rm -f "$artifacts/ww-linux-x86_64/fhs/identity"
done
echo 'PASS: five-target release assembly, parity, checksums, and invalid-payload refusal'
