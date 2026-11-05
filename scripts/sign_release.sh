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

# Defense in depth: no PR, fork, tag, or arbitrary dispatch branch can sign.
# Workflow admission must also exclude these contexts before secret injection.
if [[ "${GITHUB_ACTIONS:-false}" == true ]]; then
  [[ "${GITHUB_REPOSITORY:-}" == wetware/ww && "${GITHUB_REF:-}" == refs/heads/master ]] \
    || { echo 'Release signing refused outside wetware/ww master' >&2; exit 1; }
  case "${GITHUB_EVENT_NAME:-}" in
    push|workflow_dispatch) ;;
    *) echo 'Release signing refused for this event' >&2; exit 1 ;;
  esac
fi
[[ -f "$manifest" && ! -L "$manifest" && -s "$manifest" ]] || exit 1
[[ -f "$private_key" && ! -L "$private_key" ]] || exit 1
[[ ! -e "$signature" && ! -L "$signature" ]] || { echo 'Signature destination already exists' >&2; exit 1; }
release_dir="$(cd "$(dirname "$manifest")" && pwd -P)"
key_dir="$(cd "$(dirname "$private_key")" && pwd -P)"
case "$key_dir/" in "$release_dir/"*) echo 'Private signing key must be outside the release tree' >&2; exit 1 ;; esac
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
# Ed25519 signs the message itself, not a prehash. No key content is printed.
openssl pkeyutl -sign -rawin -inkey "$private_key" -in "$manifest" -out "$work/signature"
sh "$root/scripts/install.sh" --verify-manifest "$manifest" --signature "$work/signature" \
  --trusted-release-keys "$trusted_keys"
# Move only after verification against independently provisioned public keys.
mv "$work/signature" "$signature"
