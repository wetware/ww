#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$repo_root"
scripts/build_wasip3_component.sh \
  --name gate2b-root \
  --manifest tests/fixtures/gate2b-root/Cargo.toml \
  --artifact gate2b_root_probe \
  --target-dir "$repo_root/target/gate2b-root" \
  --allow-import wasi:cli/environment@0.3.0 \
  --allow-import wasi:cli/exit@0.3.0 \
  --allow-import wasi:cli/types@0.3.0 \
  --allow-import wasi:cli/stdin@0.3.0 \
  --allow-import wasi:cli/stdout@0.3.0 \
  --allow-import wasi:cli/stderr@0.3.0 \
  --allow-import wasi:cli/terminal-input@0.3.0 \
  --allow-import wasi:cli/terminal-stdin@0.3.0 \
  --allow-import wasi:cli/terminal-output@0.3.0 \
  --allow-import wasi:cli/terminal-stdout@0.3.0 \
  --allow-import wasi:cli/terminal-stderr@0.3.0 \
  --allow-import wasi:filesystem/types@0.3.0 \
  --allow-import wasi:filesystem/preopens@0.3.0 \
  --allow-import wasi:clocks/types@0.3.0 \
  --allow-import wasi:clocks/system-clock@0.3.0 \
  --allow-import wasi:clocks/monotonic-clock@0.3.0 \
  --allow-import wasi:random/random@0.3.0 \
  --allow-import wasi:random/insecure-seed@0.3.0 \
  --allow-import wetware:transport/connection@0.2.0 \
  --allow-import wetware:kernel-runtime/readiness@1.0.0
