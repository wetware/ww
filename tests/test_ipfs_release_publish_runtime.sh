#!/usr/bin/env bash
# Behavioral tests for the IPNS compare-and-set branches in the remote
# release publisher. A fake kubectl scripts Kubo responses without a cluster.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PUBLISH_SCRIPT="$ROOT_DIR/scripts/ipfs_publish_release.sh"
TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

mkdir -p "$TEST_DIR/bin" "$TEST_DIR/release"
printf 'fixture\n' > "$TEST_DIR/release/VERSION"

FAKE_KUBECTL="$TEST_DIR/bin/kubectl"
cat > "$FAKE_KUBECTL" <<'FAKE_KUBECTL_SH'
#!/usr/bin/env bash
set -euo pipefail

args="$*"
case "$args" in
  *"ipfs repo stat --size-only"*)
    echo 1024
    ;;
  *"ipfs add --pin=false"*)
    echo "$FAKE_CANDIDATE_CID"
    ;;
  *"ipfs pin add "*)
    ;;
  *"ipfs key list -l"*)
    echo "$FAKE_CURRENT_RELEASE"
    ;;
  *"ipfs name publish "*)
    echo "publish" >> "$FAKE_CALL_LOG"
    ;;
  *"ipfs routing provide "*)
    ;;
  *"ipfs repo gc"*)
    ;;
  *" sh -s -- "*)
    cat >/dev/null
    echo "STATE_CREATED=false"
    echo "RETAINED_COUNT=1"
    echo "UNPINNED_COUNT=0"
    echo "UNPINNED_CIDS="
    echo "FAILED_UNPIN_CIDS="
    ;;
  *"tar -C "*)
    cat >/dev/null
    ;;
  *" rm -rf "*)
    ;;
  *)
    echo "unexpected fake kubectl invocation: $args" >&2
    exit 1
    ;;
esac
FAKE_KUBECTL_SH
chmod +x "$FAKE_KUBECTL"

run_publish() {
  local current="$1"
  local expected="$2"
  local candidate="$3"
  local output_file="$4"

  # The production publisher deletes the staged release tree after a
  # successful run, so each scenario needs a fresh fixture.
  rm -rf "$TEST_DIR/release"
  mkdir -p "$TEST_DIR/release"
  printf 'fixture\n' > "$TEST_DIR/release/VERSION"
  : > "$TEST_DIR/calls"
  PATH="$TEST_DIR/bin:$PATH" \
    POD=ipfs-test \
    REMOTE_RELEASE_TREE="$TEST_DIR/release" \
    POD_RELEASE_TREE=/tmp/ww-release-tree-test \
    WW_RELEASE_EXPECTED_CURRENT="$expected" \
    FAKE_CURRENT_RELEASE="$current" \
    FAKE_CANDIDATE_CID="$candidate" \
    FAKE_CALL_LOG="$TEST_DIR/calls" \
    "$PUBLISH_SCRIPT" >"$output_file" 2>&1
}

# Idempotent retry: the pointer already targets the candidate. Do not publish
# again, but finish pin-state bookkeeping so a prior partial run can recover.
run_publish /ipfs/bcandidate /ipfs/bexpected bcandidate "$TEST_DIR/idempotent.out"
grep -Fq "IPNS_UPDATED=false" "$TEST_DIR/idempotent.out" \
  || fail "idempotent retry must report that IPNS was not updated"
[ ! -s "$TEST_DIR/calls" ] \
  || fail "idempotent retry must not invoke ipfs name publish"

# Normal compare-and-set success: the observed pointer still matches.
run_publish /ipfs/bexpected /ipfs/bexpected bcandidate "$TEST_DIR/publish.out"
grep -Fq "IPNS_UPDATED=true" "$TEST_DIR/publish.out" \
  || fail "matching compare-and-set must report an IPNS update"
[ "$(grep -Fc publish "$TEST_DIR/calls")" -eq 1 ] \
  || fail "matching compare-and-set must publish exactly once"

# Pointer changed during staging: fail closed and never publish.
: > "$TEST_DIR/calls"
if run_publish /ipfs/bother /ipfs/bexpected bcandidate "$TEST_DIR/mismatch.out"; then
  fail "compare-and-set mismatch must fail"
fi
grep -Fq "expected /ipfs/bexpected, found /ipfs/bother" "$TEST_DIR/mismatch.out" \
  || fail "compare-and-set mismatch must explain the conflicting pointers"
[ ! -s "$TEST_DIR/calls" ] \
  || fail "compare-and-set mismatch must not invoke ipfs name publish"

echo "PASS: IPFS release publish runtime checks"
