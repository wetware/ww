#!/usr/bin/env bash
# Exercise lockfile discovery and stale-lock rejection with real, offline Cargo.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CHECK_SCRIPT="$ROOT_DIR/scripts/check_cargo_lockfiles.sh"
TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT
export CARGO_NET_OFFLINE=true

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

expect_success() {
  local output
  if ! output="$(bash "$CHECK_SCRIPT" 2>&1)"; then
    fail "expected valid lockfiles: $output"
  fi
  for owner in "${owners[@]}"; do
    grep -Fq "Checking ${owner:+$owner/}Cargo.lock" <<<"$output" \
      || fail "did not check owner: $owner"
  done
}

expect_failure() {
  local pattern="$1" output
  if output="$(bash "$CHECK_SCRIPT" 2>&1)"; then
    fail "expected failure containing: $pattern"
  fi
  grep -Fq -- "$pattern" <<<"$output" \
    || fail "missing failure reason '$pattern': $output"
}

make_crate() {
  local directory="$1" name="$2"
  mkdir -p "$directory/src"
  cat >"$directory/Cargo.toml" <<EOF
[package]
name = "$name"
version = "0.1.0"
edition = "2021"

[workspace]
exclude = ["dependency"]
EOF
  printf 'pub fn fixture() {}\n' >"$directory/src/lib.rs"
}

mkdir "$TEST_DIR/repo"
cd "$TEST_DIR/repo"
git init -q
expect_failure 'No tracked Cargo.lock files found'

# The child owns its lockfile even though its parent is also a workspace.
owners=('' 'components/parent' 'components/parent/nested child')
names=(lock_root lock_parent lock_child)
dependency_paths=(dependency ../../dependency ../../../dependency)
for index in 0 1 2; do
  owner="${owners[$index]}"
  make_crate "${owner:-.}" "${names[$index]}"
  cargo generate-lockfile --manifest-path "${owner:+$owner/}Cargo.toml"
done
make_crate dependency lock_dependency
git add .

# An untracked lockfile has no owning manifest and must not enter discovery.
mkdir untracked
printf 'invalid lockfile\n' >untracked/Cargo.lock
expect_success

for index in 0 1 2; do
  owner="${owners[$index]}"
  manifest="${owner:+$owner/}Cargo.toml"
  lockfile="${owner:+$owner/}Cargo.lock"
  cp "$manifest" "$TEST_DIR/manifest"
  cp "$lockfile" "$TEST_DIR/lockfile"
  cat >>"$manifest" <<EOF

[dependencies]
lock_dependency = { path = "${dependency_paths[$index]}" }
EOF
  expect_failure '--locked'
  cmp -s "$lockfile" "$TEST_DIR/lockfile" \
    || fail "stale check modified $lockfile"
  cp "$TEST_DIR/manifest" "$manifest"
  expect_success
done

mv Cargo.lock "$TEST_DIR/missing-lock"
expect_failure 'Missing lockfile or owning manifest: Cargo.lock / Cargo.toml'
mv "$TEST_DIR/missing-lock" Cargo.lock
expect_success

mv 'components/parent/nested child/Cargo.toml' "$TEST_DIR/missing-manifest"
expect_failure 'Missing lockfile or owning manifest: components/parent/nested child/Cargo.lock'
mv "$TEST_DIR/missing-manifest" 'components/parent/nested child/Cargo.toml'
expect_success

git diff --exit-code -- . ':!untracked' >/dev/null \
  || fail 'lockfile checks modified tracked fixture files'
echo 'PASS: tracked root and nested lockfiles, stale manifests, missing owners, untracked exclusion'
