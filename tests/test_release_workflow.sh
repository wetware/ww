#!/usr/bin/env bash
# Static regression checks for the publication hold and manual promotion boundary.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORKFLOW="${1:-$ROOT_DIR/.github/workflows/rust.yml}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

bash -n "$0"

release_job="$({
  awk '
    /^  release_image:/ { in_job = 1 }
    /^  publish:/ { in_job = 0 }
    in_job { print }
  ' "$WORKFLOW"
})"
non_publish_workflow="$({
  awk '
    /^  publish:/ { in_publish = 1; next }
    in_publish && /^  [[:alnum:]_-]+:/ { in_publish = 0 }
    !in_publish { print }
  ' "$WORKFLOW"
})"

[[ -n "$release_job" ]] \
  || fail "workflow must retain the ww-master image build job"

# `ww-master` must be named only by the image-release job. Any additional
# reference is a likely reintroduction of a deployment, rollout, or mutation
# path that this repository must not own.
ww_master_refs="$(grep -F 'ww-master' "$WORKFLOW" || true)"
[[ "$ww_master_refs" == '    name: Build and publish ww-master image' ]] \
  || fail "workflow must not reference ww-master outside the image-release job"

# The release job is allowed to assemble a deploy image, but it must remain an
# image publisher—not an indirect deployment wrapper.
if grep -Eq '\b(kubectl|ssh|scp|rsync|rollout|patch|scale)\b|set[[:space:]]+image' <<<"$release_job"; then
  fail "image-release job must not contain a deployment transport or mutation command"
fi

# Public jobs must not acquire infrastructure transport authority.
if grep -Eq '\b(kubectl|ssh|scp|rsync)\b' <<<"$non_publish_workflow"; then
  fail "public workflow must not use remote or Kubernetes transport"
fi

grep -Fq "github.event_name == 'workflow_dispatch'" <<<"$release_job" \
  || fail "image-release job must remain reachable via workflow dispatch"
grep -Fq "github.event_name == 'push' && github.ref == 'refs/heads/master'" <<<"$release_job" \
  || fail "image-release job must remain reachable on master pushes"
grep -Fq 'docker push ghcr.io/wetware/ww:master' <<<"$release_job" \
  || fail "image-release job must push the published image"
grep -Fq 'ghcr.io/wetware/ww:master' <<<"$release_job" \
  || fail "image-release job must retain the master image tag"
grep -Fq "ghcr.io/wetware/ww:master-\${{ github.sha }}" <<<"$release_job" \
  || fail "image-release job must retain the commit-addressable image tag"

grep -Fq 'needs: [changes, test, validate-release, release_image]' "$WORKFLOW" \
  || fail "IPFS publication must wait for the image build, not a deployment"
grep -Fq 'name: Publish to IPFS' "$WORKFLOW" \
  || fail "workflow must retain IPFS publication"
grep -Fq 'Refuse unintegrated infrastructure publication' "$WORKFLOW" \
  || fail 'IPFS publisher must refuse until infra integration is reviewed'
handoff_step="$(awk '
  /^    - name: Refuse unintegrated infrastructure publication$/ { capture = 1; next }
  capture && /^    - / { exit }
  capture { print }
' "$WORKFLOW")"
grep -Eq '^        exit 1$' <<<"$handoff_step" \
  || fail 'unintegrated publication must fail closed'
if grep -Eq '\b(ssh|scp|rsync|kubectl)\b|WW_RELEASE_SSH_' "$WORKFLOW"; then
  fail 'unintegrated infrastructure transport remains in the workflow'
fi
if grep -Eq '\b(kubectl|rsync)\b' "$WORKFLOW"; then
  fail "legacy cluster publication transport remains"
fi
if grep -Fq 'fetch_previous_binaries' "$WORKFLOW"; then
  fail "std-only releases must consume the current run's Host binaries"
fi
for condition in "needs.test.result == 'success'" "needs.validate-release.result == 'success'"; do
  grep -Fq "$condition" <<<"$release_job" || fail "GHCR must fail closed on $condition"
done
# The hold supersedes the former success-dependent publication condition.
# Match the job-level condition, not a comment or a step-level `if`.
publish_condition="$(awk '
  /^  publish:/ { in_job = 1; next }
  in_job && /^  [[:alnum:]_-]+:/ { exit }
  in_job && /^    if:/ { print }
' "$WORKFLOW")"
# Match the literal GitHub expression; Bash must not expand it.
# shellcheck disable=SC2016
[[ "$publish_condition" == '    if: ${{ false }}' ]] \
  || fail 'IPFS publish must remain unconditionally disabled for every event'
grep -Fq 'name: Validate actual packaged installation and startup' "$WORKFLOW" || fail 'installer boot validation missing'
grep -Fq 'bash tests/test_container_boot.sh ww-validated:deploy' "$WORKFLOW" || fail 'deploy boot gate missing'
grep -Fq 'bash tests/test_container_boot.sh ww-validated:release' "$WORKFLOW" || fail 'release boot gate missing'
grep -Fq 'bash tests/test_container_boot.sh ww-validated:source' "$WORKFLOW" || fail 'source boot gate missing'
grep -Fq 'docker load -i tested-image.tar' <<<"$release_job" || fail 'GHCR must publish the tested image'
grep -Fq "            - 'scripts/**'" "$WORKFLOW" || fail 'installer-only changes must trigger release validation'
grep -Fq 'test_release_descriptor_package.sh' "$WORKFLOW" || fail 'descriptor package validation missing'

# Official signing and publication policy belong to private wetware/infra.
# Public CI builds artifacts and verifies them with disposable fixture keys.
if grep -Eq 'secrets\.WW_RELEASE_SIGNING_KEY|environment: release-signing|wetware/infra|request-release\.py|ww-release-publisher' "$WORKFLOW"; then
  fail 'public workflow retains private signing, source, or publisher access'
fi
grep -Fq 'cargo build --locked -p ww-release --features package --bin ww-release-package' "$WORKFLOW" \
  || fail 'public offline package tool build missing'
grep -Fq 'name: Validate public release package library' "$WORKFLOW" \
  || fail 'public package feature tests missing'

# The private handoff receives one raw CAR; the tested image uses a separate raw artifact.
python3 - "$WORKFLOW" <<'PYGUARD'
from pathlib import Path
import re
import sys

def require(condition, message):
    if not condition:
        raise AssertionError(message)

def validate_car_boundary(text):
    validation = text.split('  validate-release:\n', 1)[1].split('  release_image:\n', 1)[0]
    image = text.split('  release_image:\n', 1)[1].split('  publish:\n', 1)[0]
    steps = re.split(r'^    - ', validation, flags=re.M)[1:]
    staging = [step for step in steps if step.startswith('name: Archive validated release inputs as CAR\n')]
    require(len(staging) == 1, 'release input CAR construction missing')
    staging = staging[0]
    require('for platform in linux-x86_64 linux-aarch64 macos-aarch64 macos-x86_64 linux-musl-x86_64; do' in staging,
            'release input must retain exactly five original receipt directories')
    require('for receipt in REVISION SHA256SUMS PROVENANCE.json; do' in staging,
            'release input receipt set changed')
    copies = [line.strip() for line in staging.splitlines() if line.strip().startswith('cp ')]
    require(copies == ['cp -R release-tree release-input/release-tree',
                      'cp "artifacts/ww-$platform/$receipt" "release-input/artifacts/ww-$platform/$receipt"'],
            'release input must contain only the release tree and original receipts')
    require('./ww-release-package pack-tree --tree release-input --spool release-input-spool' in staging
            and './ww-release-package export-car --spool release-input-spool --car release-input.car' in staging,
            'release input must use the public content encoder')
    uploads = [step for step in steps if 'uses: actions/upload-artifact@' in step]
    require(len(uploads) == 2, 'release inputs and image must use separate artifacts')
    for path in ('release-input.car', 'tested-image.tar'):
        upload = [step for step in uploads if re.search(r'^        path: '+re.escape(path)+r'$', step, re.M)]
        require(len(upload) == 1, 'missing single-file artifact: '+path)
        require('actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9' in upload[0]
                and '        archive: false\n' in upload[0] and '        if-no-files-found: error\n' in upload[0],
                'release handoff must use pinned raw upload: '+path)
    require('validated-release' not in validation+image, 'obsolete mixed release artifact remains')
    require('uses: actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333' in image
            and '        name: tested-image.tar\n' in image
            and '        skip-decompress: true\n' in image
            and '        digest-mismatch: error\n' in image,
            'image consumer must receive the exact tested raw image')

text = Path(sys.argv[1]).read_text()
validate_car_boundary(text)
mutations = [
    ('archive: false', 'archive: true'),
    ('actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9', 'actions/upload-artifact@v7'),
    ('name: tested-image.tar', 'name: release-input.car'),
    ('skip-decompress: true', 'skip-decompress: false'),
    ('digest-mismatch: error', 'digest-mismatch: warn'),
    ('for receipt in REVISION SHA256SUMS PROVENANCE.json; do', 'for receipt in REVISION SHA256SUMS; do'),
    ('cp -R release-tree release-input/release-tree', 'cp -R release-tree release-input/release-tree\n        cp ww-release-package release-input/tool'),
    ('./ww-release-package export-car', './ww-release-package verify'),
    ('./ww-release-package pack-tree', './ww-release-package verify'),
    ('path: release-input.car', 'path: release-input-spool/'),
    ('cp "artifacts/ww-$platform/$receipt"', 'cp "replacement/$receipt"'),
]
for old, new in mutations:
    require(old in text, 'mutation target missing: '+old)
    try:
        validate_car_boundary(text.replace(old, new))
    except AssertionError:
        continue
    raise AssertionError('raw CAR boundary accepted mutation: '+old)
print(f'PASS: raw CAR source contract and {len(mutations)} mutation controls')
PYGUARD

# Freeze the existing job-level admission controls during the maintenance hold.
# This covers ordinary Rust/metrics CI, release validation, GHCR and Pages.
# YAML/action validation is separate; this check follows the workflow's layout.
actual_admission="$(awk '
  /^jobs:/ { in_jobs = 1; next }
  !in_jobs { next }
  /^  publish:/ { in_publish = 1; next }
  /^  [[:alnum:]_-]+:/ { in_publish = 0; print; in_condition = 0; next }
  in_publish { next }
  /^    (if|needs):/ { print; in_condition = ($0 == "    if: >-"); next }
  in_condition && /^      / { print; next }
  { in_condition = 0 }
' "$WORKFLOW")"
expected_admission="$({
  cat <<'ADMISSION'
  changes:
    if: >-
      (github.event_name == 'push' && github.ref == 'refs/heads/master')
      || github.event_name == 'workflow_dispatch'
  lockfiles:
  test-architecture-map:
  fmt:
  fuzz-corpus:
    needs: lockfiles
  clippy:
    needs: lockfiles
  solidity:
  build:
    needs: lockfiles
  build-wasm:
    needs: lockfiles
  test-original-native-arm64:
    if: >-
      github.event_name == 'pull_request'
      && github.repository == 'wetware/ww'
      && github.event.pull_request.head.repo.full_name == 'wetware/ww'
      && github.event.pull_request.head.ref == 'lthibault/pasted-text-v38'
    needs: lockfiles
  test-current-native-arm64:
    needs: [lockfiles, build-wasm]
  test-host:
    needs: [build, build-wasm]
  test-stdlib:
    needs: lockfiles
  test:
    needs: [test-host, test-stdlib, test-current-native-arm64, test-original-native-arm64]
    if: ${{ always() }}
  build-architecture-map:
    if: >-
      github.event_name == 'workflow_dispatch'
      || (github.event_name == 'push' && github.ref == 'refs/heads/master'
          && needs.changes.outputs.architecture_map == 'true')
    needs: [changes, fmt, clippy, solidity, build, test, test-architecture-map]
  deploy-architecture-map:
    if: ${{ needs.build-architecture-map.result == 'success' }}
    needs: build-architecture-map
  build-binaries:
    if: >-
      github.event_name == 'workflow_dispatch'
      || (github.event_name == 'push' && github.ref == 'refs/heads/master'
          && (needs.changes.outputs.std == 'true' || needs.changes.outputs.host == 'true'))
    needs: [changes, fmt, clippy, solidity, build, build-wasm]
  validate-release:
    if: >-
      github.ref == 'refs/heads/master'
      && (github.event_name == 'workflow_dispatch'
          || needs.changes.outputs.std == 'true' || needs.changes.outputs.host == 'true')
    needs: [changes, test, build-binaries, build-wasm]
  release_image:
    if: >-
      !cancelled() && github.ref == 'refs/heads/master'
      && needs.test.result == 'success' && needs.validate-release.result == 'success'
      && (github.event_name == 'workflow_dispatch'
          || (github.event_name == 'push' && github.ref == 'refs/heads/master'
              && (needs.changes.outputs.std == 'true' || needs.changes.outputs.host == 'true')))
    needs: [changes, test, validate-release]
ADMISSION
})"
[[ "$actual_admission" == "$expected_admission" ]] \
  || fail 'non-publish job admission controls changed during the publication hold'

echo "PASS: ww release workflow builds images without directly deploying ww-master"
