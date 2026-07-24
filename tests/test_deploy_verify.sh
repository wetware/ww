#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
VERIFY_SCRIPT="$ROOT_DIR/scripts/deploy_verify.sh"
TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

cat > "$TEST_DIR/kubectl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

case "$*" in
  *"rollout status deployment/ww-master"*)
    echo "deployment successfully rolled out"
    ;;
  *"get pods -l app=ww-master"*)
    printf 'old-pod|2026-07-24T00:00:00Z|True\n'
    printf 'ww-master-abc||True\n'
    ;;
  *"get pod ww-master-abc"*"imageID"*)
    echo "${FAKE_IMAGE_ID}"
    ;;
  *"exec ww-master-abc -c ww -- /usr/local/bin/ww healthcheck --ready --expect-git-sha"*)
    [ "${FAKE_HEALTHCHECK:-ok}" = "ok" ] || exit 1
    echo ok
    ;;
  *)
    echo "unexpected kubectl invocation: $*" >&2
    exit 70
    ;;
esac
EOF
chmod +x "$TEST_DIR/kubectl"

run_verify() {
  KUBECTL="$TEST_DIR/kubectl" \
    WW_EXPECTED_IMAGE_DIGEST="sha256:expected" \
    WW_EXPECTED_GIT_SHA="0123456789abcdef" \
    "$VERIFY_SCRIPT"
}

FAKE_IMAGE_ID="docker-pullable://ghcr.io/wetware/ww@sha256:expected"
export FAKE_IMAGE_ID
run_verify > "$TEST_DIR/success.out"
grep -Fq "verified deployment/ww-master" "$TEST_DIR/success.out" \
  || fail "successful verification did not report its result"

FAKE_IMAGE_ID="docker-pullable://ghcr.io/wetware/ww@sha256:wrong"
export FAKE_IMAGE_ID
if run_verify > "$TEST_DIR/mismatch.out" 2>&1; then
  fail "digest mismatch must fail closed"
fi
grep -Fq "image digest mismatch" "$TEST_DIR/mismatch.out" \
  || fail "digest mismatch did not explain the failure"

FAKE_IMAGE_ID="docker-pullable://ghcr.io/wetware/ww@sha256:expected"
FAKE_HEALTHCHECK="fail"
export FAKE_IMAGE_ID FAKE_HEALTHCHECK
if run_verify > "$TEST_DIR/health.out" 2>&1; then
  fail "failed in-container health/provenance check must fail closed"
fi

echo "PASS: deploy verification checks immutable image and source provenance"
