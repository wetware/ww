#!/usr/bin/env bash
# Resolve every tracked lockfile's workspace without changing dependency state.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

# Materialize the list so a failed git command cannot hide in process substitution.
lockfiles="$(mktemp)"
trap 'rm -f "$lockfiles"' EXIT
git ls-files -z -- Cargo.lock '*/Cargo.lock' >"$lockfiles"
if [[ ! -s "$lockfiles" ]]; then
  echo 'No tracked Cargo.lock files found' >&2
  exit 1
fi

while IFS= read -r -d '' lockfile; do
  manifest="${lockfile%Cargo.lock}Cargo.toml"
  if [[ ! -f "$lockfile" || ! -f "$manifest" ]]; then
    echo "Missing lockfile or owning manifest: $lockfile / $manifest" >&2
    exit 1
  fi
  echo "Checking $lockfile"
  # --no-deps skips resolution and can accept a stale lockfile. Full metadata
  # checks the graph without compiling dependencies or executing build scripts.
  cargo metadata --locked --format-version 1 --manifest-path "$manifest" >/dev/null
done <"$lockfiles"
