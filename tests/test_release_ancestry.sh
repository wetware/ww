#!/usr/bin/env bash
# Behavioral regression tests for the IPNS release ancestry gate.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CHECK_SCRIPT="$ROOT_DIR/scripts/check_release_ancestry.sh"
TEST_REPO="$(mktemp -d)"
trap 'rm -rf "$TEST_REPO"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

expect_output() {
  local expected="$1"
  shift
  local actual

  actual="$("$CHECK_SCRIPT" "$@")" \
    || fail "expected success for: $*"
  [ "$actual" = "$expected" ] \
    || fail "expected '$expected', got '$actual' for: $*"
}

expect_failure() {
  local pattern="$1"
  shift
  local output

  if output="$("$CHECK_SCRIPT" "$@" 2>&1)"; then
    fail "expected failure for: $*"
  fi
  grep -Fq "$pattern" <<<"$output" \
    || fail "failure did not contain '$pattern': $output"
}

bash -n "$CHECK_SCRIPT"
bash -n "$0"

git -C "$TEST_REPO" init -q
git -C "$TEST_REPO" config user.name "Release Test"
git -C "$TEST_REPO" config user.email "release-test@example.invalid"
git -C "$TEST_REPO" commit --allow-empty -q -m base
base="$(git -C "$TEST_REPO" rev-parse HEAD)"

git -C "$TEST_REPO" branch -M master
git -C "$TEST_REPO" commit --allow-empty -q -m advance
advance="$(git -C "$TEST_REPO" rev-parse HEAD)"

git -C "$TEST_REPO" checkout -q -b divergent "$base"
git -C "$TEST_REPO" commit --allow-empty -q -m divergent
divergent="$(git -C "$TEST_REPO" rev-parse HEAD)"

git -C "$TEST_REPO" update-ref refs/remotes/origin/master "$advance"

(
  cd "$TEST_REPO"
  expect_output publish "$advance" ""
  expect_output skip "$advance" "$advance"
  expect_output publish "$advance" "$base"
  expect_failure "older than the published revision" "$base" "$advance"
  expect_failure "not contained in origin/master" "$divergent" "$base"
  expect_failure "not contained in origin/master" "$divergent" ""
  expect_failure "diverges from the published revision" "$advance" "$divergent"
  expect_failure "candidate revision must be" "not-a-sha" "$base"
  expect_failure "candidate revision is not available" "ffffffffffffffffffffffffffffffffffffffff" "$base"
  expect_failure "published revision is malformed" "$advance" "not-a-sha"
  expect_failure "published revision is not available" "$advance" "0000000000000000000000000000000000000000"

  if output="$(WW_RELEASE_MASTER_REF=refs/remotes/origin/missing "$CHECK_SCRIPT" "$advance" "$base" 2>&1)"; then
    fail "expected a missing master reference to fail"
  fi
  grep -Fq "master reference is not available" <<<"$output" \
    || fail "missing-master failure was not explicit: $output"
)

echo "PASS: release ancestry checks"
