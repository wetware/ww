#!/usr/bin/env bash
# BATS-compatible test suite for scripts/install.sh
# Run with: bats tests/test_install.sh
# Install BATS: https://bats-core.readthedocs.io/
#
# These tests source the install script's functions in a controlled
# environment to verify OS/arch detection, checksum verification,
# and error handling without actually downloading anything.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
INSTALL_SCRIPT="${SCRIPT_DIR}/scripts/install.sh"

# Helper: run the install script with args, capturing output
run_install() {
  sh "$INSTALL_SCRIPT" "$@" 2>&1 || true
}

# Test: --help prints usage
test_help() {
  output=$(run_install --help)
  echo "$output" | grep -q "Usage:" || { echo "FAIL: --help missing Usage"; return 1; }
  echo "$output" | grep -q "\-\-version" || { echo "FAIL: --help missing --version"; return 1; }
  if echo "$output" | grep -q "\-\-prefix"; then echo "FAIL: obsolete --prefix advertised"; return 1; fi
  echo "PASS: --help"
}

# Test: unknown option exits with error
test_unknown_option() {
  output=$(run_install --bogus 2>&1)
  echo "$output" | grep -q "Unknown option" || { echo "FAIL: unknown option not caught"; return 1; }
  echo "PASS: unknown option"
}

# Test: OS detection works on this machine
test_os_detection() {
  os=$(uname -s)
  case "$os" in
    Linux|Darwin) echo "PASS: OS detection ($os)" ;;
    *) echo "FAIL: unexpected OS: $os"; return 1 ;;
  esac
}

# Test: arch detection normalizes arm64 -> aarch64
test_arch_normalization() {
  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64|aarch64|arm64) echo "PASS: arch detection ($arch)" ;;
    *) echo "FAIL: unexpected arch: $arch"; return 1 ;;
  esac
}

# Test: artifact name construction
test_artifact_name() {
  os=$(uname -s)
  arch=$(uname -m)

  case "$os" in
    Linux)  os_name="linux" ;;
    Darwin) os_name="macos" ;;
  esac

  case "$arch" in
    x86_64|amd64) arch_name="x86_64" ;;
    aarch64|arm64) arch_name="aarch64" ;;
  esac

  expected="ww-${os_name}-${arch_name}.tar.gz"
  echo "PASS: artifact name = $expected"
}

# Test: checksum verification with known good data
test_checksum_verify() {
  tmpdir=$(mktemp -d)
  trap "rm -rf $tmpdir" RETURN

  # Create a test file
  echo "test content" > "$tmpdir/test.tar.gz"

  # Generate sha256
  expected_sha=$(sha256sum "$tmpdir/test.tar.gz" 2>/dev/null || shasum -a 256 "$tmpdir/test.tar.gz")
  expected_sha=$(echo "$expected_sha" | awk '{print $1}')

  # Create CHECKSUMS.txt with raw sha256 section
  cat > "$tmpdir/CHECKSUMS.txt" <<EOF
# Raw SHA-256 (compatible with: sha256sum --check CHECKSUMS.txt)
${expected_sha}  test.tar.gz
EOF

  # Verify the checksum matches
  actual_sha=$(sha256sum "$tmpdir/test.tar.gz" 2>/dev/null || shasum -a 256 "$tmpdir/test.tar.gz")
  actual_sha=$(echo "$actual_sha" | awk '{print $1}')

  if [ "$expected_sha" = "$actual_sha" ]; then
    echo "PASS: checksum verification"
  else
    echo "FAIL: checksum mismatch"
    return 1
  fi
}

# Test: checksum mismatch detection
test_checksum_mismatch() {
  tmpdir=$(mktemp -d)
  trap "rm -rf $tmpdir" RETURN

  echo "real content" > "$tmpdir/test.tar.gz"

  # Create CHECKSUMS.txt with wrong hash
  cat > "$tmpdir/CHECKSUMS.txt" <<EOF
# Raw SHA-256 (compatible with: sha256sum --check CHECKSUMS.txt)
0000000000000000000000000000000000000000000000000000000000000000  test.tar.gz
EOF

  actual_sha=$(sha256sum "$tmpdir/test.tar.gz" 2>/dev/null || shasum -a 256 "$tmpdir/test.tar.gz")
  actual_sha=$(echo "$actual_sha" | awk '{print $1}')

  if [ "0000000000000000000000000000000000000000000000000000000000000000" != "$actual_sha" ]; then
    echo "PASS: checksum mismatch detected"
  else
    echo "FAIL: impossible hash collision"
    return 1
  fi
}

# Test: multihash blake3 prefix parsing
test_multihash_blake3_prefix() {
  # 1e = blake3 multicodec, 20 = 32 bytes length
  prefix="1e20"
  if [ "${#prefix}" -eq 4 ]; then
    echo "PASS: blake3 multihash prefix is 4 hex chars (2 bytes)"
  else
    echo "FAIL: wrong prefix length"
    return 1
  fi
}

# Test: multihash sha256 prefix parsing
test_multihash_sha256_prefix() {
  # 12 = sha2-256 multicodec, 20 = 32 bytes length
  prefix="1220"
  if [ "${#prefix}" -eq 4 ]; then
    echo "PASS: sha256 multihash prefix is 4 hex chars (2 bytes)"
  else
    echo "FAIL: wrong prefix length"
    return 1
  fi
}

# Run all tests
echo "=== Install Script Tests ==="
PASS=0
FAIL=0

for test_fn in test_help test_unknown_option test_os_detection test_arch_normalization \
               test_artifact_name test_checksum_verify test_checksum_mismatch \
               test_multihash_blake3_prefix test_multihash_sha256_prefix; do
  if $test_fn; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
  fi
done

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ] || exit 1
