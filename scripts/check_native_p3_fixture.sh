#!/usr/bin/env bash
set -euo pipefail

readonly NIGHTLY="nightly-2026-08-30"
readonly FIXTURE_MANIFEST="tests/fixtures/native-p3/Cargo.toml"
readonly EXPECTED_SDK_VERSION="34.0"
readonly EXPECTED_LLVM_VERSION="23.1.0"
readonly EXPECTED_COMPONENT_LD_VERSION="wasm-component-ld 0.5.30"
readonly EXPECTED_WASM_TOOLS_VERSION="wasm-tools 1.258.0"
readonly EXPECTED_WASM_TOOLS_VERSION_PATTERN='^wasm-tools 1\.258\.0( \([0-9a-f]{9} [0-9]{4}-[0-9]{2}-[0-9]{2}\))?$'

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
cd "$repo_root"

if [[ -z "${WASI_SDK_PATH:-}" ]]; then
  echo "WASI_SDK_PATH must name an extracted WASI SDK 34.0 directory" >&2
  exit 1
fi

wasm_tools="${WASM_TOOLS:-wasm-tools}"
component_ld="$WASI_SDK_PATH/bin/wasm-component-ld"
wasm_ld="$WASI_SDK_PATH/bin/wasm-ld"
p3_lib="$WASI_SDK_PATH/share/wasi-sysroot/experimental-coop-threads/lib/wasm32-wasip3"
target_dir="${P3_TARGET_DIR:-$repo_root/target/native-p3-fixture}"
artifact="$target_dir/wasm32-wasip3/release/native_p3_fixture.wasm"

for path in "$WASI_SDK_PATH/VERSION" "$component_ld" "$wasm_ld"; do
  if [[ ! -f "$path" ]]; then
    echo "required WASI SDK file is missing: $path" >&2
    exit 1
  fi
done

if [[ ! -d "$p3_lib" ]]; then
  echo "WASI SDK 34 P3 library directory is missing: $p3_lib" >&2
  exit 1
fi

if [[ "$(head -n 1 "$WASI_SDK_PATH/VERSION")" != "$EXPECTED_SDK_VERSION" ]]; then
  echo "WASI SDK must be version $EXPECTED_SDK_VERSION" >&2
  exit 1
fi

if [[ "$("$component_ld" --version)" != "$EXPECTED_COMPONENT_LD_VERSION" ]]; then
  echo "$component_ld must be $EXPECTED_COMPONENT_LD_VERSION" >&2
  exit 1
fi

if ! "$wasm_ld" --version | grep -q "^LLD $EXPECTED_LLVM_VERSION "; then
  echo "$wasm_ld must be LLD $EXPECTED_LLVM_VERSION" >&2
  exit 1
fi

wasm_tools_version="$("$wasm_tools" --version)"
# Release binaries include their short Git hash and build date; Cargo-installed
# binaries report only the package version.
if [[ ! "$wasm_tools_version" =~ $EXPECTED_WASM_TOOLS_VERSION_PATTERN ]]; then
  printf '%s must report %s (reported: %s)\n' \
    "$wasm_tools" "$EXPECTED_WASM_TOOLS_VERSION" "$wasm_tools_version" >&2
  exit 1
fi

if ! rustup component list --toolchain "$NIGHTLY" --installed | grep -qx 'rust-src'; then
  echo "$NIGHTLY must have the rust-src component" >&2
  exit 1
fi

if ! rustc "+$NIGHTLY" --version --verbose | grep -qx "LLVM version: $EXPECTED_LLVM_VERSION"; then
  echo "$NIGHTLY must use LLVM $EXPECTED_LLVM_VERSION" >&2
  exit 1
fi

encoded_flags="-Clink-arg=--wasm-ld-path=$wasm_ld"$'\x1f'"-Lnative=$p3_lib"

env \
  WASI_SDK_PATH="$WASI_SDK_PATH" \
  CARGO_TARGET_WASM32_WASIP3_LINKER="$component_ld" \
  CARGO_ENCODED_RUSTFLAGS="$encoded_flags" \
  CARGO_TARGET_DIR="$target_dir" \
  cargo "+$NIGHTLY" build \
    --locked \
    --manifest-path "$FIXTURE_MANIFEST" \
    -Z build-std=std,panic_abort \
    --target wasm32-wasip3 \
    --release

"$wasm_tools" validate --features all "$artifact"

component_wit="$("$wasm_tools" component wit "$artifact")"
printf '%s\n' "$component_wit"

if printf '%s\n' "$component_wit" | grep -Eq '@0\.2\.'; then
  echo "native P3 fixture contains a WASI 0.2 reference" >&2
  exit 1
fi

wasi_imports="$({
  printf '%s\n' "$component_wit" |
    sed -nE 's/^[[:space:]]*import[[:space:]]+(wasi:[^;]+);$/\1/p'
})"

if [[ -z "$wasi_imports" ]]; then
  echo "native P3 fixture has no versioned WASI imports" >&2
  exit 1
fi

rendered_import_count="$(
  printf '%s\n' "$component_wit" |
    awk '/^[[:space:]]*import[[:space:]]+wasi:/ { count++ } END { print count + 0 }'
)"
extracted_import_count="$(printf '%s\n' "$wasi_imports" | wc -l | tr -d ' ')"

if [[ "$extracted_import_count" -ne "$rendered_import_count" ]]; then
  printf 'native P3 fixture import parser extracted %s of %s rendered WASI imports\n' \
    "$extracted_import_count" "$rendered_import_count" >&2
  exit 1
fi

unexpected_imports="$({
  printf '%s\n' "$wasi_imports" |
    grep -Ev '@0\.3\.[0-9]+$' || true
})"

if [[ -n "$unexpected_imports" ]]; then
  printf 'native P3 fixture has non-P3 WASI imports:\n%s\n' "$unexpected_imports" >&2
  exit 1
fi

echo "native P3 fixture validated with $extracted_import_count WASI 0.3.x imports"
