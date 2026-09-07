#!/usr/bin/env bash
# Regression tests for native-P3 tool version validation.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CHECK_SCRIPT="$ROOT_DIR/scripts/check_native_p3_fixture.sh"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "$TEST_ROOT"' EXIT

SDK_DIR="$TEST_ROOT/wasi-sdk"
BIN_DIR="$TEST_ROOT/bin"
WASM_TOOLS="$TEST_ROOT/wasm-tools"
TARGET_DIR="$TEST_ROOT/target"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

mkdir -p \
  "$SDK_DIR/bin" \
  "$SDK_DIR/share/wasi-sysroot/experimental-coop-threads/lib/wasm32-wasip3" \
  "$BIN_DIR"
printf '%s\n' '34.0' >"$SDK_DIR/VERSION"

cat >"$SDK_DIR/bin/wasm-component-ld" <<'EOF'
#!/usr/bin/env bash
echo 'wasm-component-ld 0.5.30'
EOF

cat >"$SDK_DIR/bin/wasm-ld" <<'EOF'
#!/usr/bin/env bash
echo 'LLD 23.1.0 (compatible with GNU linkers)'
EOF

cat >"$BIN_DIR/rustup" <<'EOF'
#!/usr/bin/env bash
echo 'rust-src'
EOF

cat >"$BIN_DIR/rustc" <<'EOF'
#!/usr/bin/env bash
echo 'rustc 1.100.0-nightly'
echo 'LLVM version: 23.1.0'
EOF

cat >"$BIN_DIR/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ " $* " == *" --target wasm32-wasip3 "* ]]; then
  artifact="$CARGO_TARGET_DIR/wasm32-wasip3/release/native_p3_fixture.wasm"
  mkdir -p "$(dirname "$artifact")"
  : >"$artifact"
fi
EOF

cat >"$WASM_TOOLS" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  --version)
    printf '%s\n' "$MOCK_WASM_TOOLS_VERSION"
    ;;
  validate)
    ;;
  component)
    cat <<'WIT'
package root:component;

world root {
  import wasi:cli/environment@0.3.0;
}
WIT
    ;;
  *)
    exit 2
    ;;
esac
EOF

chmod +x \
  "$SDK_DIR/bin/wasm-component-ld" \
  "$SDK_DIR/bin/wasm-ld" \
  "$BIN_DIR/rustup" \
  "$BIN_DIR/rustc" \
  "$BIN_DIR/cargo" \
  "$WASM_TOOLS"

run_check() {
  local version="$1"

  env \
    PATH="$BIN_DIR:$PATH" \
    WASI_SDK_PATH="$SDK_DIR" \
    WASM_TOOLS="$WASM_TOOLS" \
    P3_TARGET_DIR="$TARGET_DIR" \
    MOCK_WASM_TOOLS_VERSION="$version" \
    "$CHECK_SCRIPT"
}

run_check 'wasm-tools 1.258.0' >/dev/null \
  || fail 'Cargo-installed version output was rejected'
run_check 'wasm-tools 1.258.0 (5c6d31c78 2026-08-24)' >/dev/null \
  || fail 'official release version output was rejected'

if output="$(run_check 'wasm-tools 1.258.1' 2>&1)"; then
  fail 'a different wasm-tools version was accepted'
fi
grep -Fq 'must report wasm-tools 1.258.0' <<<"$output" \
  || fail "unexpected rejection message: $output"

if run_check 'wasm-tools 1.258.0 (untrusted metadata)' >/dev/null 2>&1; then
  fail 'malformed release metadata was accepted'
fi

echo 'PASS: native-P3 tool version checks'
