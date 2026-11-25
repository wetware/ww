#!/usr/bin/env bash
# Static regression checks for the publication hold and manual promotion boundary.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORKFLOW="${1:-$ROOT_DIR/.github/workflows/rust.yml}"
FAST_WORKFLOW="${2:-$ROOT_DIR/.github/workflows/fast.yml}"

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

grep -Fq 'docker push ghcr.io/wetware/ww:master' <<<"$release_job" \
  || fail "image-release job must push the published image"
grep -Fq 'ghcr.io/wetware/ww:master' <<<"$release_job" \
  || fail "image-release job must retain the master image tag"
grep -Fq "ghcr.io/wetware/ww:master-\${{ github.sha }}" <<<"$release_job" \
  || fail "image-release job must retain the commit-addressable image tag"

grep -Fq 'needs: [test, validate-release, release_image]' "$WORKFLOW" \
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
for condition in \
  "github.ref == 'refs/heads/master'" \
  "needs.test.result == 'success'" \
  "needs.validate-release.result == 'success'" \
  "needs.validate-source-container.result == 'success'"; do
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
grep -Fq 'producer_artifacts.py validate-hosts' "$WORKFLOW" || fail 'producer validation missing'

# Official signing and publication policy belong to private wetware/infra.
# Public CI builds artifacts and verifies them with disposable fixture keys.
if grep -Eq 'secrets\.WW_RELEASE_SIGNING_KEY|environment: release-signing|wetware/infra|request-release\.py|ww-release-publisher' "$WORKFLOW"; then
  fail 'public workflow retains private signing, source, or publisher access'
fi
if grep -Fq 'ww-release-package' "$WORKFLOW"; then fail 'private packager remains public'; fi
grep -Fq 'name: Validate public release package library' "$WORKFLOW" \
  || fail 'public package feature tests missing'

# The fast workflow is callable by the producer, but routine events must
# have no path to image publication, signing, or producer artifacts.
if grep -Eq 'packages: write|ww-master|release-source|tested-image.tar|producer_artifacts.py record|docker push|release-signing|WW_RELEASE_SIGNING_KEY|wetware/infra' "$FAST_WORKFLOW"; then
  fail 'fast workflow contains a release operation'
fi

# Six immutable data artifacts feed private assembly; the tested image stays raw.
python3 - "$WORKFLOW" <<'PYGUARD'
from pathlib import Path
import re
import sys

def require(condition, message):
    if not condition:
        raise AssertionError(message)

def validate_boundary(text):
    producer = re.search(r'(?ms)^  build-binaries:\n(.*?)(?=^  [a-z][a-z0-9_-]*:|\Z)', text)[1]
    validation = text.split('  validate-release:\n', 1)[1].split('  release_image:\n', 1)[0]
    image = text.split('  release_image:\n', 1)[1].split('  publish:\n', 1)[0]
    for artifact in ('ww-linux-x86_64', 'ww-linux-aarch64', 'ww-macos-aarch64', 'ww-macos-x86_64', 'ww-linux-musl-x86_64'):
        require(f'artifact: {artifact}\n' in producer, 'missing producer target')
    require('producer_artifacts.py record --root ${{ matrix.bin_path }}' in producer, 'Host receipts missing')
    require('--attempt "${{ github.run_attempt }}" --target "${{ matrix.target }}"' in producer, 'Host provenance missing')
    require('producer_artifacts.py validate-hosts --root artifacts' in validation, 'Host/FHS validation missing')
    require('--attempt "${{ github.run_attempt }}" --target release-source' in validation, 'source provenance missing')
    identity = '--revision "${{ github.sha }}" --run "${{ github.run_id }}"'
    require(identity in producer, 'Host recorder must bind current revision and run')
    require(validation.count(identity) == 2, 'Host validator and source recorder must bind current revision and run')
    require('cp CHANGELOG.md release-source/CHANGELOG.md' in validation and 'cp scripts/install.sh release-source/scripts/install.sh' in validation, 'source inputs missing')
    require('cp "$example"/*.wasm "release-source/examples/$name/bin/"' in validation, 'published examples missing')
    require('ww-release-package' not in text and 'release-input.car' not in text, 'public assembly remains')
    steps = re.split(r'^    - ', validation, flags=re.M)[1:]
    uploads = [step for step in steps if 'uses: actions/upload-artifact@' in step]
    require(len(uploads) == 2, 'source and image need separate artifacts')
    source = [step for step in uploads if '        name: release-source\n' in step]
    require(len(source) == 1 and '        path: release-source/\n' in source[0] and 'archive: false' not in source[0], 'source must be standard ZIP')
    raw = [step for step in uploads if '        path: tested-image.tar\n' in step]
    require(len(raw) == 1 and '        archive: false\n' in raw[0], 'tested image must remain raw')
    for step in uploads:
        require('actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9' in step and '        if-no-files-found: error\n' in step, 'pinned upload required')
    require('uses: actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333' in image and '        name: tested-image.tar\n' in image and '        skip-decompress: true\n' in image and '        digest-mismatch: error\n' in image, 'image consumer must verify tested raw image')

text = Path(sys.argv[1]).read_text()
validate_boundary(text)
mutations = [
    ('--run "${{ github.run_id }}"', '--run "1"'),
    ('--revision "${{ github.sha }}"', '--revision "stale"'),
    ('archive: false', 'archive: true'),
    ('actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9', 'actions/upload-artifact@v7'),
    ('name: tested-image.tar', 'name: other-image.tar'),
    ('skip-decompress: true', 'skip-decompress: false'),
    ('digest-mismatch: error', 'digest-mismatch: warn'),
    ('--target release-source', '--target unknown'),
    ('producer_artifacts.py validate-hosts', 'producer_artifacts.py record'),
    ('cp CHANGELOG.md release-source/CHANGELOG.md', 'true'),
    ('name: release-source', 'name: different-source'),
    ('cp "$example"/*.wasm', 'true'),
    ('artifact: ww-linux-aarch64', 'artifact: older-linux-aarch64'),
]
for old, new in mutations:
    require(old in text, 'mutation target missing: '+old)
    try:
        validate_boundary(text.replace(old, new))
    except AssertionError:
        continue
    raise AssertionError('producer boundary accepted mutation: '+old)
print(f'PASS: producer artifact contract and {len(mutations)} mutation controls')
PYGUARD

# Execute the workflow's actual join gate, including failure and partial reruns.
python3 - "$WORKFLOW" "$FAST_WORKFLOW" <<'PYQUALIFICATION'
import json
from itertools import product
from pathlib import Path
import os
import re
import subprocess
import sys

def require(condition, message):
    if not condition:
        raise AssertionError(message)

def jobs_of(workflow):
    body = workflow.split('\njobs:\n', 1)[1]
    return dict(re.findall(r'^  ([a-z][a-z0-9_-]*):\n(.*?)(?=^  [a-z][a-z0-9_-]*:|\Z)', body, re.M | re.S))

def verify_needs(jobs, job, expected):
    match = re.search(r'^    needs: (.+)$', jobs[job], re.M)
    actual = set(match[1].strip('[]').split(', ')) if match else set()
    require(actual == set(expected.split()), f'{job} has unexpected prerequisites: {actual}')

def run_script(script, **values):
    result = subprocess.run(['bash', '-euo', 'pipefail', '-c', script],
                            env=dict(os.environ, **values), capture_output=True)
    return result.returncode == 0

def run_block(step):
    run = step.split('      run: ', 1)[1]
    if run.startswith('|\n'):
        return '\n'.join(line[8:] for line in run[2:].splitlines() if line.strip())
    return run.splitlines()[0]

def events(workflow):
    header = workflow.split('\non:\n', 1)[1]
    triggers = re.split(r'^[^ #\s]', header, maxsplit=1, flags=re.M)[0]
    return set(re.findall(r'^  (\w+):', triggers, re.M))

text = Path(sys.argv[1]).read_text()
fast = Path(sys.argv[2]).read_text()
jobs, fast_jobs = jobs_of(text), jobs_of(fast)

require(events(text) == {'workflow_dispatch'}, 'the producer must require a manual dispatch')
require(events(fast) == {'push', 'pull_request', 'workflow_call'}, 'fast checks must run for PRs, pushes and producer calls')
require(fast.count('branches: [ "master", "staging" ]') == 2, 'both fast triggers must cover master and staging')
for job, name in {'fmt': 'Check formatting', 'clippy': 'Clippy', 'solidity': 'Solidity (Foundry)'}.items():
    require(f'    name: {name}\n' in fast_jobs[job], 'required PR check renamed: ' + name)
require('\npermissions:\n  contents: read\n\n' in fast, 'shared checks must default to read-only')
for job, body in fast_jobs.items():
    if job != 'pages':
        require('pages: write' not in body and 'id-token: write' not in body, 'publishing permissions on ' + job)
checks = 'lockfiles fmt clippy solidity fast-rust fast-p3 fuzz-corpus test-stdlib test-architecture-map'
verify_needs(fast_jobs, 'test', checks)
require(set(fast_jobs) == set(checks.split()) | {'test', 'changes', 'pages'}, 'unexpected job in fast tier')
for check in checks.split():
    require('\n    if:' not in fast_jobs[check], f'fast check may be skipped: {check}')
verify_needs(fast_jobs, 'fast-p3', 'lockfiles')
# The dispatch caller executes both suites in the required WASM producer.
# On PRs and pushes the fast job must execute both in order, on one runner.
p3_commands = ['make test-p3-fixture', 'make test-guest-session-cancel']
for body, expected_conditions in [(fast_jobs['fast-p3'], ["github.event_name != 'workflow_dispatch'"]),
                                  (jobs['build-wasm'], [])]:
    steps = re.split(r'^    - ', body, flags=re.M)[1:]
    fixture_steps = [step for step in steps if any(f'      run: {cmd}\n' in step for cmd in p3_commands)]
    require([run_block(step) for step in fixture_steps] == p3_commands, 'incomplete or reordered P3 coverage')
    for step in fixture_steps:
        require(re.findall(r'^      if: (.+)$', step, re.M) == expected_conditions,
                'incorrect P3 fixture condition')
        require('continue-on-error:' not in step, 'P3 fixture failures must fail the job')
verify_needs(fast_jobs, 'pages', 'changes test')
require("if: github.event_name == 'push' && github.ref == 'refs/heads/master' && needs.changes.outputs.architecture_map == 'true'" in fast_jobs['pages'],
        'fast workflow Pages deployment requires a matching master push')
require('uses: ./.github/workflows/architecture-map.yml' in fast_jobs['pages'] and
        'uses: ./.github/workflows/architecture-map.yml' in jobs['pages'], 'Pages renderer must remain shared')
require('if: ${{ !inputs.validation_only }}' in jobs['pages'], 'validation-only dispatch must skip Pages')
verify_needs(jobs, 'pages', 'test')

# Verify both the event boundary and the exact revision before starting builds.
for job, expected in {
    'checks': 'admission', 'build': 'admission', 'build-wasm': 'admission',
    'test-host': 'build build-wasm', 'test': 'checks test-host',
    'build-binaries': 'build build-wasm',
}.items():
    verify_needs(jobs, job, expected)
# These Host tests can skip or build a substitute when a fixture is absent.
# Full qualification must check the downloaded inputs before starting Cargo.
host = jobs['test-host']
fixtures = ['std/kernel/bin/main.wasm', 'std/status/bin/status.wasm',
            'target/fhs/boot/kernel.wasm', 'target/fhs/bin/status.wasm']
fixtures += [f'examples/{name}/bin/{name}.wasm'
             for name in ('counter', 'discovery', 'echo', 'oracle', 'snap-hello-rs')]
for directory, binary in [('authority-probe', 'authority_probe'), ('gate2b-root', 'gate2b_root_probe'),
                          ('native-p3-fixture', 'native_p3_fixture'), ('routing-key-probe', 'routing_key_probe'),
                          ('stream-lifecycle-probe', 'stream_lifecycle_probe')]:
    fixtures.append(f'target/{directory}/wasm32-wasip3/release/{binary}.wasm')
before_tests, test_step = host.split('    - name: Run host workspace tests\n', 1)
for path in fixtures:
    require(f'        test -s {path}\n' in before_tests, 'missing full-tier fixture requirement: ' + path)
for flag in ('WW_TEST_REQUIRE_KUBO', 'WW_REQUIRE_P3_FIXTURES', 'WW_USE_PREBUILT_AUTHORITY_PROBE'):
    require(f'{flag}: "1"' in test_step.split('    - name:', 1)[0], 'Host test flag missing: ' + flag)
require('uses: ./.github/workflows/fast.yml' in jobs['checks'], 'full qualification must include fast checks')
admission = re.split(r'^    - ', jobs['admission'], flags=re.M)[1]
require('EXPECTED_SHA: ${{ inputs.expected_sha }}' in admission and
        'VALIDATION_ONLY: ${{ inputs.validation_only }}' in admission, 'dispatch inputs must bind the admission gate')
revision = 'a' * 40
good = dict(GITHUB_EVENT_NAME='workflow_dispatch', GITHUB_RUN_ATTEMPT='1', GITHUB_SHA=revision,
            EXPECTED_SHA=revision, GITHUB_REF='refs/heads/master', VALIDATION_ONLY='false')
for overrides, accepted in (
    ({}, True),
    ({'GITHUB_REF': 'refs/heads/measurement', 'VALIDATION_ONLY': 'true'}, True),
    ({'EXPECTED_SHA': 'b' * 40}, False), ({'EXPECTED_SHA': ''}, False),
    ({'GITHUB_RUN_ATTEMPT': '2'}, False), ({'GITHUB_EVENT_NAME': 'pull_request'}, False),
    ({'GITHUB_EVENT_NAME': 'push'}, False), ({'GITHUB_REF': 'refs/heads/staging'}, False),
    ({'GITHUB_REF': 'refs/tags/v1', 'VALIDATION_ONLY': 'true'}, False),
    ({'VALIDATION_ONLY': 'true'}, False),
):
    require(run_script(run_block(admission), **(good | overrides)) == accepted,
            f'wrong dispatch admission for {overrides}')

for collection in (jobs, fast_jobs):
    require('name: Test\n' in collection['test'] and 'if: ${{ always() }}' in collection['test'],
            'the Test check must evaluate skipped and failed prerequisites')
statuses = ('success', 'failure', 'cancelled', 'skipped', '')
fast_gate = fast_jobs['test']
require('RESULTS: ${{ toJSON(needs) }}' in fast_gate, 'fast Test requires all dependency results')
all_pass = {check: {'result': 'success'} for check in checks.split()}
for check in all_pass:
    for status in statuses:
        results = {**all_pass, check: {'result': status}}
        require(run_script(run_block(fast_gate), RESULTS=json.dumps(results)) == (status == 'success'),
                f'fast Test admitted incorrect result for {check}={status!r}')
require(not run_script(run_block(fast_gate), RESULTS='{}'), 'fast Test admitted an empty result set')
for check in all_pass:
    results = {key: value for key, value in all_pass.items() if key != check}
    require(not run_script(run_block(fast_gate), RESULTS=json.dumps(results)), 'fast Test admitted a missing ' + check)
full_gate = jobs['test']
for name, node in [('HOST_RESULT', 'test-host'), ('CHECKS_RESULT', 'checks')]:
    require(f'{name}: ${{{{ needs.{node}.result }}}}' in full_gate, 'full Test result binding missing: ' + node)
for host, common in product(statuses, repeat=2):
    require(run_script(run_block(full_gate), HOST_RESULT=host, CHECKS_RESULT=common) ==
            (host == common == 'success'), 'full Test admitted incorrect dependency result')

image_gate = re.split(r'^    - ', jobs['release_image'], flags=re.M)[1]
require('SOURCE_ATTEMPT: ${{ needs.validate-source-container.outputs.qualified_attempt }}' in image_gate,
        'image gate must bind the source attempt before artifact download')
for run, source_attempt in [('1', '1'), ('1', ''), ('1', '2'), ('2', '1'), ('2', '2')]:
    require(run_script(run_block(image_gate), GITHUB_RUN_ATTEMPT=run, SOURCE_ATTEMPT=source_attempt) ==
            (run == source_attempt == '1'), 'image gate admitted incorrect attempt')

source = jobs['validate-source-container']
validation = jobs['validate-release']
require('    needs: [test, validate-release, validate-source-container]\n' in jobs['release_image'],
        'image-only reruns must directly bind source qualification')
require('    needs: build-wasm\n' in source, 'source must run independently of native producers and Host tests')
verify_needs(jobs, 'validate-release', 'admission test build-binaries build-wasm validate-source-container')
require("if: ${{ always() && needs.admission.result == 'success' }}" in validation.split('    steps:', 1)[0],
        'join requires admission and must evaluate failed or skipped producers')
require('ww-validated:source' not in validation, 'source build must leave the serial artifact-validation job')
require('name: wasm-artifacts' in source, 'source must consume canonical WASM artifacts')
require('path: artifacts/wasm-artifacts' in source, 'source inputs must remain isolated from the checkout')
for command in (
    'cp artifacts/wasm-artifacts/target/fhs/boot/kernel.wasm std/kernel/bin/main.wasm',
    'cp artifacts/wasm-artifacts/target/fhs/bin/status.wasm std/status/bin/status.wasm',
    'docker build --build-arg WW_BUILD_GIT_SHA="${{ github.sha }}" -f Containerfile -t ww-validated:source .',
    'bash tests/test_container_boot.sh ww-validated:source artifacts/wasm-artifacts/target/fhs',
):
    require(command in source, 'canonical exact-revision source qualification missing: ' + command)
dispatch = text.split('  workflow_dispatch:\n', 1)[1].split('\n#', 1)[0]
require('validation_only:' in dispatch and 'type: boolean' in dispatch and 'default: true' in dispatch,
        'full dispatch must default to validation only')
require('!inputs.validation_only' in jobs['release_image'].split('    steps:', 1)[0],
        'measurement dispatch must not publish the image')
require('qualified_attempt: ${{ steps.qualification.outputs.attempt }}' in source,
        'source must expose the completed qualification attempt')
source_steps = re.split(r'^    - ', source, flags=re.M)[1:]
boot = next(i for i, step in enumerate(source_steps) if 'bash tests/test_container_boot.sh ww-validated:source' in step)
qualification = next(i for i, step in enumerate(source_steps) if 'id: qualification' in step)
require(qualification >= boot, 'attempt receipt must follow source boot')
require('github.run_attempt' in source_steps[qualification] and '$GITHUB_OUTPUT' in source_steps[qualification],
        'attempt receipt must bind the current run attempt')
require('if:' not in source_steps[qualification], 'failed boot must not emit an attempt receipt')
first = re.split(r'^    - ', validation, flags=re.M)[1]
require('Require current-attempt full qualification' in first, 'qualification gate must precede artifact checks and uploads')
require('if:' not in first, 'qualification gate must not be conditional')
for variable, expression in {
    'SOURCE_RESULT': 'needs.validate-source-container.result',
    'SOURCE_ATTEMPT': 'needs.validate-source-container.outputs.qualified_attempt',
    'RUN_ATTEMPT': 'github.run_attempt',
    'TEST_RESULT': 'needs.test.result',
    'PRODUCER_RESULT': 'needs.build-binaries.result',
    'WASM_RESULT': 'needs.build-wasm.result',
}.items():
    require(f'{variable}: ${{{{ {expression} }}}}' in first, 'incorrect gate binding: ' + variable)
script = run_block(first)
variables = ('SOURCE_RESULT', 'TEST_RESULT', 'PRODUCER_RESULT', 'WASM_RESULT')
cases = 0
for results in product(statuses, repeat=len(variables)):
    passed = run_script(script, SOURCE_ATTEMPT='1', RUN_ATTEMPT='1', **dict(zip(variables, results)))
    require(passed == all(result == 'success' for result in results), 'incorrect gate outcome: ' + repr(results))
    cases += 1
# A rerun can reuse successful prerequisites from an earlier attempt. Require
# a fresh dispatch even if the source job reports the new attempt.
for run, source_attempt in [('1', ''), ('1', '2'), ('2', '1'), ('2', '2'), ('', '')]:
    require(not run_script(script, SOURCE_ATTEMPT=source_attempt, RUN_ATTEMPT=run, **dict.fromkeys(variables, 'success')),
            f'gate accepted attempts run={run!r} source={source_attempt!r}')
    cases += 1
print(f'PASS: parallel source dependency graph and {cases} executed qualification cases')
PYQUALIFICATION

echo "PASS: ww release workflow builds images without directly deploying ww-master"
