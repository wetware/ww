#!/usr/bin/env bash
set -euo pipefail

readonly NIGHTLY="nightly-2026-08-30"
readonly EXPECTED_SDK_VERSION="34.0"
readonly EXPECTED_LLVM_VERSION="23.1.0"
readonly EXPECTED_COMPONENT_LD_VERSION="wasm-component-ld 0.5.30"
readonly EXPECTED_WASM_TOOLS_VERSION="wasm-tools 1.258.0"
readonly EXPECTED_WASM_TOOLS_VERSION_PATTERN='^wasm-tools 1\.258\.0( \([0-9a-f]{9} [0-9]{4}-[0-9]{2}-[0-9]{2}\))?$'

usage() {
  cat <<'EOF'
Usage: scripts/build_wasip3_component.sh \
  --name NAME \
  --manifest PATH \
  --artifact FILE_STEM \
  [--package PACKAGE] \
  [--target-dir PATH] \
  [--output PATH] \
  --allow-import VERSIONED_INTERFACE [...]

Builds one native wasm32-wasip3 component with the pinned toolchain. The
validator rejects WASI 0.2, socket imports, non-WASI-P3 imports, and imported
interfaces outside the component-specific allowlist.
EOF
}

name=""
manifest=""
artifact_stem=""
package=""
target_dir=""
output=""
allowed_imports=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name)
      name="${2:-}"
      shift 2
      ;;
    --manifest)
      manifest="${2:-}"
      shift 2
      ;;
    --artifact)
      artifact_stem="${2:-}"
      shift 2
      ;;
    --package)
      package="${2:-}"
      shift 2
      ;;
    --target-dir)
      target_dir="${2:-}"
      shift 2
      ;;
    --output)
      output="${2:-}"
      shift 2
      ;;
    --allow-import)
      allowed_imports+=("${2:-}")
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$name" || -z "$manifest" || -z "$artifact_stem" ]]; then
  usage >&2
  exit 2
fi

if [[ ${#allowed_imports[@]} -eq 0 ]]; then
  echo "$name must declare an import allowlist" >&2
  exit 2
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
cd "$repo_root"

if [[ ! -f "$manifest" ]]; then
  echo "$name manifest is missing: $manifest" >&2
  exit 1
fi

if [[ -z "${WASI_SDK_PATH:-}" ]]; then
  echo "WASI_SDK_PATH must name an extracted WASI SDK 34.0 directory" >&2
  exit 1
fi

wasm_tools="${WASM_TOOLS:-wasm-tools}"
component_ld="$WASI_SDK_PATH/bin/wasm-component-ld"
wasm_ld="$WASI_SDK_PATH/bin/wasm-ld"
p3_lib="$WASI_SDK_PATH/share/wasi-sysroot/experimental-coop-threads/lib/wasm32-wasip3"

if [[ -z "$target_dir" ]]; then
  target_slug="${name//[^A-Za-z0-9_.-]/-}"
  target_dir="$repo_root/target/wasip3/$target_slug"
fi

artifact="$target_dir/wasm32-wasip3/release/$artifact_stem.wasm"

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
cargo_args=(
  "+$NIGHTLY"
  build
  --locked
  --manifest-path "$manifest"
  -Z "build-std=std,panic_abort"
  --target wasm32-wasip3
  --release
)

if [[ -n "$package" ]]; then
  cargo_args+=(--package "$package")
fi

env \
  WASI_SDK_PATH="$WASI_SDK_PATH" \
  CARGO_TARGET_WASM32_WASIP3_LINKER="$component_ld" \
  CARGO_ENCODED_RUSTFLAGS="$encoded_flags" \
  CARGO_TARGET_DIR="$target_dir" \
  cargo "${cargo_args[@]}"

if [[ ! -s "$artifact" ]]; then
  echo "$name artifact is missing or empty: $artifact" >&2
  exit 1
fi

"$wasm_tools" validate --features all "$artifact"

component_wit="$("$wasm_tools" component wit "$artifact")"
printf '%s\n' "$component_wit"

if printf '%s\n' "$component_wit" | grep -Eq 'wasi:[^[:space:];]*@0\.2\.'; then
  echo "$name contains a WASI 0.2 reference" >&2
  exit 1
fi

if printf '%s\n' "$component_wit" | grep -Eq '(^|[[:space:]])wasi:sockets[/@:]'; then
  echo "$name unexpectedly imports WASI sockets" >&2
  exit 1
fi

imports="$({
  printf '%s\n' "$component_wit" |
    sed -nE 's/^[[:space:]]*import[[:space:]]+([^;]+);$/\1/p'
})"

rendered_import_count="$(
  printf '%s\n' "$component_wit" |
    awk '/^[[:space:]]*import[[:space:]]+/ { count++ } END { print count + 0 }'
)"
extracted_import_count=0
if [[ -n "$imports" ]]; then
  extracted_import_count="$(printf '%s\n' "$imports" | wc -l | tr -d ' ')"
fi

if [[ "$extracted_import_count" -ne "$rendered_import_count" ]]; then
  printf '%s import parser extracted %s of %s rendered imports\n' \
    "$name" "$extracted_import_count" "$rendered_import_count" >&2
  exit 1
fi

unexpected_wasi="$({
  printf '%s\n' "$imports" |
    grep '^wasi:' |
    grep -Ev '@0\.3\.[0-9]+$' || true
})"

if [[ -n "$unexpected_wasi" ]]; then
  printf '%s has non-P3 WASI imports:\n%s\n' "$name" "$unexpected_wasi" >&2
  exit 1
fi

unexpected_imports=()
while IFS= read -r imported; do
  [[ -z "$imported" ]] && continue
  allowed=false
  for expected in "${allowed_imports[@]}"; do
    if [[ "$imported" == "$expected" ]]; then
      allowed=true
      break
    fi
  done
  if [[ "$allowed" == false ]]; then
    unexpected_imports+=("$imported")
  fi
done <<<"$imports"

if [[ ${#unexpected_imports[@]} -ne 0 ]]; then
  printf '%s has imports outside its allowlist:\n' "$name" >&2
  printf '  %s\n' "${unexpected_imports[@]}" >&2
  exit 1
fi

if [[ -n "$output" ]]; then
  mkdir -p "$(dirname -- "$output")"
  cp "$artifact" "$output"
fi

echo "$name validated with $extracted_import_count allowed imports"
