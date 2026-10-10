#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
cd "$repo_root"

build_only=false
if [[ "${1:-}" == --build-only && $# == 1 ]]; then
  build_only=true
elif [[ $# != 0 ]]; then
  echo "usage: scripts/check_guest_session_cancel.sh [--build-only]" >&2
  exit 2
fi

target_dir="${P3_SESSION_TARGET_DIR:-$repo_root/target/guest-session-cancel}"
fixture=tests/fixtures/guest-session-cancel
observer=wetware:session-cancel/observer@0.1.0

for component in child parent; do
  imports=(
    --allow-import "$observer"
    --allow-import wasi:cli/environment@0.3.0
    --allow-import wasi:cli/exit@0.3.0
    --allow-import wasi:cli/types@0.3.0
    --allow-import wasi:cli/stdin@0.3.0
    --allow-import wasi:cli/stdout@0.3.0
    --allow-import wasi:cli/stderr@0.3.0
    --allow-import wasi:cli/terminal-input@0.3.0
    --allow-import wasi:cli/terminal-output@0.3.0
    --allow-import wasi:cli/terminal-stdin@0.3.0
    --allow-import wasi:cli/terminal-stdout@0.3.0
    --allow-import wasi:cli/terminal-stderr@0.3.0
    --allow-import wasi:clocks/types@0.3.0
    --allow-import wasi:clocks/monotonic-clock@0.3.0
    --allow-import wasi:clocks/system-clock@0.3.0
    --allow-import wasi:random/insecure-seed@0.3.0
  )
  if [[ "$component" == child ]]; then
    imports+=(--allow-import wetware:transport/connection@0.2.0)
  else
    imports+=(--allow-import wetware:session-cancel/child@0.1.0)
  fi
  bash "$script_dir/build_wasip3_component.sh" \
    --name "guest-session-cancel-$component" \
    --manifest "$fixture/$component/Cargo.toml" \
    --artifact "guest_session_cancel_$component" \
    --target-dir "$target_dir" \
    --output "$target_dir/$component.wasm" \
    "${imports[@]}"
done

wasm_tools="${WASM_TOOLS:-wasm-tools}"
artifact="$target_dir/composed.wasm"
"$wasm_tools" compose "$target_dir/parent.wasm" -d "$target_dir/child.wasm" -o "$artifact"
"$wasm_tools" validate --features all "$artifact"
"$wasm_tools" component wit "$artifact"
test -s "$artifact"

if [[ "$build_only" == false ]]; then
  WW_RPC_SESSION_P3_FIXTURE="$artifact" \
    cargo test -p cell p3_guest_session_ --locked -- --ignored --test-threads=1 --nocapture
fi
