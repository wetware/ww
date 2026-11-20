#!/usr/bin/env bash
# Sign exact CHECKSUMS.txt bytes. The private key must stay outside the release.
set +x
set -euo pipefail
umask 077
manifest="${1:?manifest required}"
signature="${2:?signature destination required}"
private_key="${3:?private key file required}"
trusted_keys="${4:?independently provisioned public keyring required}"
root="$(cd "$(dirname "$0")/.." && pwd)"

# The calling workflow owns repository/event admission and secret access.
# This reusable offline tool validates explicit files and independent trust.
[[ -f "$manifest" && ! -L "$manifest" && -s "$manifest" ]] || exit 1
[[ -f "$private_key" && ! -L "$private_key" ]] || exit 1
[[ ! -e "$signature" && ! -L "$signature" ]] || { echo 'Signature destination already exists' >&2; exit 1; }
release_dir="$(cd "$(dirname "$manifest")" && pwd -P)"
key_dir="$(cd "$(dirname "$private_key")" && pwd -P)"
case "$key_dir/" in "$release_dir/"*) echo 'Private signing key must be outside the release tree' >&2; exit 1 ;; esac
# Use the offline package tool; no running Host or OpenSSL private-key conversion.
if [[ -n "${WW_RELEASE_PACKAGE:-}" ]]; then
  tool="$WW_RELEASE_PACKAGE"
elif [[ -x "$root/target/release/ww-release-package" ]]; then
  tool="$root/target/release/ww-release-package"
else
  tool="$root/target/debug/ww-release-package"
fi
exec "$tool" sign --manifest "$manifest" --signature "$signature" \
  --private-key "$private_key" --trusted-keys "$trusted_keys"
