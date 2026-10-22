#!/usr/bin/env bash
# Assemble an installable release from this run's five Host artifacts and FHS.
set -euo pipefail
artifacts="${1:?artifact directory required}"
stage="${2:?empty destination required}"
revision="${3:?source revision required}"
root="$(cd "$(dirname "$0")/.." && pwd)"
checksum() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi
}

validator="${WW_FHS_VALIDATOR:-$artifacts/wasm-artifacts/target/debug/examples/stage_fhs}"
kernel="$artifacts/wasm-artifacts/std/kernel/bin/main.wasm"
status="$artifacts/wasm-artifacts/std/status/bin/status.wasm"
fhs="$artifacts/wasm-artifacts/target/fhs"
[ ! -e "$stage" ] || { echo "Release destination already exists: $stage" >&2; exit 1; }
[[ "$revision" =~ ^[0-9a-f]{40}$ ]] || { echo 'Invalid source revision' >&2; exit 1; }
# Validation occurs before creating the destination or any external mutation.
"$validator" verify "$kernel" "$status" "$fhs"
for entry in linux-x86_64:linux/x86_64 linux-aarch64:linux/aarch64 macos-aarch64:macos/aarch64 macos-x86_64:macos/x86_64 linux-musl-x86_64:linux/musl-x86_64; do
  artifact="$artifacts/ww-${entry%%:*}"
  [ -f "$artifact/ww" ] && [ ! -L "$artifact/ww" ] && [ -s "$artifact/ww" ] || { echo "Missing, empty, or symlink Host: $artifact/ww" >&2; exit 1; }
  [ -f "$artifact/REVISION" ] && [ ! -L "$artifact/REVISION" ] \
    && [ "$(cat "$artifact/REVISION")" = "$revision" ] \
    || { echo "Stale or missing source revision: $artifact" >&2; exit 1; }
  [ -f "$artifact/SHA256SUMS" ] && [ ! -L "$artifact/SHA256SUMS" ] \
    || { echo "Missing Host checksum receipt: $artifact" >&2; exit 1; }
  expected="$(awk 'NR == 1 && NF == 2 && $2 == "ww" && length($1) == 64 && $1 !~ /[^0-9a-f]/ { hash=$1 } END { if (NR != 1 || hash == "") exit 1; print hash }' "$artifact/SHA256SUMS")"
  actual="$(checksum "$artifact/ww" | awk '{print $1}')"
  [ "$actual" = "$expected" ] || { echo "Host checksum mismatch: $artifact" >&2; exit 1; }
  "$validator" verify "$kernel" "$status" "$artifact/fhs"
done
mkdir -p "$stage"
cp -R "$fhs" "$stage/fhs"
for entry in linux-x86_64:linux/x86_64 linux-aarch64:linux/aarch64 macos-aarch64:macos/aarch64 macos-x86_64:macos/x86_64 linux-musl-x86_64:linux/musl-x86_64; do
  mkdir -p "$stage/bin/ww/${entry#*:}"
  cp "$artifacts/ww-${entry%%:*}/ww" "$stage/bin/ww/${entry#*:}/ww"
  chmod +x "$stage/bin/ww/${entry#*:}/ww"
done
sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -1 > "$stage/VERSION"
printf '%s\n' "$revision" > "$stage/REVISION"
cp "$root/CHANGELOG.md" "$stage/"
mkdir -p "$stage/scripts"
cp "$root/scripts/install.sh" "$stage/scripts/install.sh"
# Preserve published examples, without copying source or private build state.
for example in "$artifacts"/wasm-artifacts/examples/*/bin; do
  [ -d "$example" ] || continue
  name="$(basename "$(dirname "$example")")"
  mkdir -p "$stage/examples/$name/bin"
  cp "$example"/*.wasm "$stage/examples/$name/bin/"
done
(cd "$stage"; printf '# sha256\n'; find bin/ww fhs -type f | LC_ALL=C sort | while IFS= read -r path; do checksum "$path"; done) > "$stage/CHECKSUMS.txt"
