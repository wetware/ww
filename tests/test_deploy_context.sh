#!/usr/bin/env bash
# Static complement to test_container_boot.sh (which executes real images).
set -euo pipefail
cd "$(dirname "$0")/.."
fail() { echo "FAIL: $*" >&2; exit 1; }
for recipe in Containerfile Containerfile.deploy Containerfile.release; do
  grep -Fq 'ENV HOME=/home/ww' "$recipe" || fail "$recipe lacks explicit HOME"
  grep -Fq 'USER 1000:1000' "$recipe" || fail "$recipe lacks non-root UID/GID"
  grep -Fq '/usr/share/wetware/kernel/' "$recipe" || fail "$recipe lacks canonical FHS"
  grep -Fq 'VOLUME ["/home/ww"]' "$recipe" || fail "$recipe lacks private-state volume"
  grep -Fq -- '--chown=1000:1000' "$recipe" || fail "$recipe lacks writable HOME"
done
for recipe in Containerfile Containerfile.deploy; do
  grep -Fq 'CMD ["run", "/usr/share/wetware/kernel"]' "$recipe" || fail "$recipe changed active CMD"
done
! grep -q '^CMD ' Containerfile.release || fail 'release recipe changed caller-owned CMD'
grep -Fq 'cargo run --locked --example stage_fhs -- stage' Containerfile || fail 'source recipe bypasses staging'
! grep -Eq '^[[:space:]]*(&& )?make (std|host|fhs)|build_wasip3_component' Containerfile || fail 'source recipe rebuilds canonical components'
grep -Fq 'container-build: fhs' Makefile || fail 'source build lacks canonical producer prerequisite'
grep -Fq 'cp release-tree/fhs/boot/kernel.wasm std/kernel/bin/main.wasm' .github/workflows/rust.yml || fail 'CI source context lacks canonical kernel'
grep -Fq 'cp release-tree/fhs/bin/status.wasm std/status/bin/status.wasm' .github/workflows/rust.yml || fail 'CI source context lacks canonical status'
grep -Fq 'COPY fhs/ /usr/share/wetware/kernel/' Containerfile.deploy || fail 'deploy recipe bypasses staging'
grep -Fq 'COPY fhs/ /usr/share/wetware/kernel/' Containerfile.release || fail 'release recipe bypasses staging'
grep -Fq '**/.ww/' .dockerignore || fail 'Host private state enters build context'
grep -Fq '**/.ipfs/' .dockerignore || fail 'Kubo private state enters build context'
echo 'PASS: all recipes use canonical FHS outside writable HOME'
