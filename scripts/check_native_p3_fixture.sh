#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
cd "$repo_root"

target_dir="${P3_TARGET_DIR:-$repo_root/target/native-p3-fixture}"
artifact="$target_dir/wasm32-wasip3/release/native_p3_fixture.wasm"

"$script_dir/build_wasip3_component.sh" \
  --name "native P3 fixture" \
  --manifest tests/fixtures/native-p3/Cargo.toml \
  --artifact native_p3_fixture \
  --target-dir "$target_dir" \
  --allow-import wetware:transport/connection@0.2.0 \
  --allow-import wasi:cli/environment@0.3.0 \
  --allow-import wasi:cli/exit@0.3.0 \
  --allow-import wasi:cli/types@0.3.0 \
  --allow-import wasi:cli/stdin@0.3.0 \
  --allow-import wasi:cli/stdout@0.3.0 \
  --allow-import wasi:cli/stderr@0.3.0 \
  --allow-import wasi:cli/terminal-input@0.3.0 \
  --allow-import wasi:cli/terminal-output@0.3.0 \
  --allow-import wasi:cli/terminal-stdin@0.3.0 \
  --allow-import wasi:cli/terminal-stdout@0.3.0 \
  --allow-import wasi:cli/terminal-stderr@0.3.0 \
  --allow-import wasi:clocks/types@0.3.0 \
  --allow-import wasi:clocks/monotonic-clock@0.3.0 \
  --allow-import wasi:clocks/system-clock@0.3.0 \
  --allow-import wasi:filesystem/types@0.3.0 \
  --allow-import wasi:filesystem/preopens@0.3.0

WW_NATIVE_P3_FIXTURE="$artifact" \
  cargo test -p cell 'p3::tests::p3_' -- --ignored --test-threads=1
