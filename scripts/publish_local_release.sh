#!/usr/bin/env bash
# Called by make publish after package, installation, and boot validation.
# Uses the same canonical paths and SHA-256 section as the CI release tree.
set -euo pipefail

artifacts="${RELEASE_ARTIFACTS:?set RELEASE_ARTIFACTS to the complete five-target artifact directory}"
binary="${1:?host executable is required}"
fhs="${2:?canonical FHS directory is required}"
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

die() { printf 'local-publish: %s\n' "$*" >&2; exit 1; }
[[ -z "$(git status --porcelain)" ]] || die "commit reviewed local changes before publishing"
[[ -f "$binary" && ! -L "$binary" && -s "$binary" && -x "$binary" ]] || die "missing executable: $binary"
revision="$(git rev-parse --verify HEAD)"

case "$(uname -s)" in
  Darwin) platform=macos ;;
  Linux) platform=linux ;;
  *) die "unsupported local release platform" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) architecture=aarch64 ;;
  x86_64) architecture=x86_64 ;;
  *) die "unsupported local release architecture" ;;
esac

if [[ -z "${SKIP_PIN:-}" ]]; then
  expected="${WW_RELEASE_EXPECTED_CURRENT:?set WW_RELEASE_EXPECTED_CURRENT=/ipfs/<cid>}"
  [[ "$expected" =~ ^/ipfs/[A-Za-z0-9]+$ ]] || die "invalid expected release CID"
  key="$(ipfs key list -l | awk '$2 == "ww-release" {print $1; exit}')"
  [[ -n "$key" ]] || die "ww-release key is not available"
  current="$(ipfs name resolve "/ipns/$key")"
  [[ "$current" == "$expected" ]] || die "release changed; resolve and review the current release"
  previous_revision="$(ipfs cat "$current/REVISION")"
  action="$(bash scripts/check_release_ancestry.sh "$revision" "$previous_revision")"
  if [[ "$action" == skip ]]; then
    printf 'Revision %s is already published\n' "$revision"
    exit 0
  fi
  [[ "$action" == publish ]] || die "unexpected ancestry decision"
fi

temporary="$(mktemp -d)"
trap 'rm -rf -- "$temporary"' EXIT
stage="$temporary/release"
# The same assembly function requires every supported Host target. A native-only
# local build cannot accidentally replace the shared release with a partial tree.
cargo run --locked --example stage_fhs -- verify std/kernel/bin/main.wasm std/status/bin/status.wasm "$fhs"
cmp std/kernel/bin/main.wasm "$artifacts/wasm-artifacts/std/kernel/bin/main.wasm"
cmp std/status/bin/status.wasm "$artifacts/wasm-artifacts/std/status/bin/status.wasm"
WW_FHS_VALIDATOR="${CARGO_TARGET_DIR:-target}/debug/examples/stage_fhs" \
  bash scripts/assemble_release.sh "$artifacts" "$stage" "$revision"
# The installer and readiness tests in make publish ran this native binary.
# Require those exact bytes in the release candidate.
cmp "$binary" "$stage/bin/ww/$platform/$architecture/ww"

cid="$(ipfs add --pin=false -rQ --cid-version=1 "$stage")"
[[ "$cid" =~ ^[A-Za-z0-9]+$ ]] || die "invalid imported release CID"
if [[ -z "${SKIP_PIN:-}" ]]; then
  # Compare immediately before the externally visible name mutation. No old
  # release pins are removed by this local publisher.
  [[ "$(ipfs name resolve "/ipns/$key")" == "$expected" ]] || die "release changed during staging"
  ipfs pin add "$cid"
  ipfs name publish --key=ww-release "/ipfs/$cid"
fi
mkdir -p target
printf '%s\n' "$cid" > target/release.cid
printf 'Release CID: %s\n' "$cid"
