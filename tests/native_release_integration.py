#!/usr/bin/env python3
"""Consume verified original GNU ARM64 releases in a provisioned ww739-* account.

Completion means only the existing integration subset completed. The unchanged
native qualification report remains PARTIAL and full native acceptance is false.
Root owns account/service-manager provisioning and final account cleanup.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import pwd
import re
import shutil
import sys
import tempfile
import time
from urllib.parse import urlsplit

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / 'scripts'))
import qualify_native_upgrade as native
from kubo_fixture import isolated_kubo


def verify_input_policy(verified):
    require(verified.get('status') == 'VERIFIED_DIAGNOSTIC_INPUTS'
            and verified.get('native_acceptance') is False
            and verified.get('publication_allowed') is False
            and verified.get('required_checks_success') is False
            and verified['source'].get('runtime_source_equal') is True,
            'input verifier did not preserve the diagnostic-only policy')
    policy = verified.get('policy')
    if policy == 'pr808-current-source-native-diagnostic-v1':
        historical = verified['historical_baseline']
        build = verified['current_build']
        require(verified['slot_identities'] == {'a': 'original-producer-A', 'b': 'current-source-candidate-C'}
                and build['source'] == verified['source']
                and verified['source']['checkout_revision'] == verified['b']['revision']
                and verified['source']['artifact_revision'] == verified['b']['revision']
                and build['ci']['revision'] == verified['b']['revision']
                and build['host']['sha256'] == verified['b']['sha256']
                and build['builder'] == verified['b']['builder']
                and build['profile'] == 'release'
                and build['ci']['completed_job_receipt'] is False
                and build['ci']['uploaded_producer_artifact'] is False,
                'current candidate identity or build evidence differs')
        require(historical['source']['runtime_source_equal'] is True
                and historical['source']['checkout_revision'] == historical['source']['artifact_revision'],
                'historical baseline lacks immutable source equality')
    else:
        require(policy == 'pr808-original-pair-diagnostic-only', 'unknown integration input policy')
        historical = verified
    require(historical['policy'] == 'pr808-original-pair-diagnostic-only'
            and historical['native_acceptance'] is False and historical['publication_allowed'] is False
            and historical['required_checks_success'] is False,
            'historical producer failures must retain their original policy')
    failures = {key: historical['producer_evidence'][key]['required_checks']['Test']['conclusion']
                for key in ('a', 'b')}
    require(all(value == 'failure' for value in failures.values()),
            'original producer required-Test failures were not retained')
    return failures


EXPECTED_CHECKS = [
    'authenticated fresh A remains unbound',
    'stopped admission check preserves native acceptance files',
    'actual systemd A starts with HTTP admin disabled',
    'missing and zero changed-upgrade budgets refuse without native or service mutation',
    'actual changed A to B commits matching provenance and readiness',
    'actual retained rollback restores A with measured readiness',
    'enabled A to B to A preserves enablement and custom unit bytes',
    'readiness-timeout: exact candidate signaled through pidfd; blocked intent retained; installed CLI recovery restored ready A',
    'failed-start: exact candidate signaled through pidfd; blocked intent retained; installed CLI recovery restored ready A',
    'exact-artifact no-op preserves provenance',
    'disabled stopped upgrade and rollback remain disabled and stopped',
    'custom configuration, identity, FHS and private bytes preserved',
]
# Readiness polling adds additional receipts between these ordered operations.
EXPECTED_COMMANDS = '''initial-service install-a daemon-register daemon-reload fault-stop-policy
enable-before-admission enabled-before-admission admit-a-check admit-a start-a enabled-a
budget-before-manager missing-upgrade-budget zero-upgrade-budget budget-after-manager
upgrade-a-b enabled-b rollback-b-a enabled-restored-a noop-before-manager exact-noop
noop-after-manager readiness-timeout-original-manager readiness-timeout
readiness-timeout-contained-pid readiness-timeout-enabled-after-fault
readiness-timeout-recover readiness-timeout-enabled-after-recovery
failed-start-original-manager failed-start failed-start-contained-pid
failed-start-enabled-after-fault failed-start-recover failed-start-enabled-after-recovery
stop-before-stopped-upgrade disable-before-stopped-admission disabled-before-stopped-admission
admit-stopped-disabled-a stopped-upgrade stopped-pid disabled-after-stopped-upgrade
stopped-rollback stopped-rollback-pid disabled-after-stopped-rollback
cleanup-stop cleanup-disable cleanup-reload'''.split()
BOOT_MODES = ['configured', 'bare', 'local-target', 'immutable-target', 'explicit-root']
READINESS_STAGES = [('a', 'a'), ('b', 'b'), ('restored-a', 'a'),
                    ('readiness-timeout-restored-a', 'a'), ('failed-start-restored-a', 'a')]
NATIVE_CLEANUP = 'fixture service stopped and removed; installation evidence retained'
OVERALL_TIMEOUT_SECONDS = 45 * 60
require = native.require


def read_json(path):
    """Only consume bounded, owned ordinary evidence files without aliases."""
    native.custody(path, os.geteuid())
    with path.open('rb') as stream:
        raw = stream.read(4 * 1024 * 1024 + 1)
    require(len(raw) <= 4 * 1024 * 1024, f'JSON evidence exceeds bound: {path}')
    return json.loads(raw)


def validate_native_result(output, exit_code, marker, trust_sha256):
    """Accept exactly the implemented PARTIAL contract, never a generic exit 2."""
    require(type(exit_code) is int and exit_code == 2, 'native runner must preserve exit 2')
    report = read_json(output / 'report.json')
    require(report.get('version') == 1 and report.get('platform') == 'linux-aarch64'
            and report.get('status') == 'PARTIAL', 'native report must remain terminal PARTIAL')
    require('error' not in report and 'cleanup_error' not in report, 'native report contains an error field')
    require(report.get('checks') == EXPECTED_CHECKS and report.get('executed_checks') == 12,
            'native checks are missing, reordered or unexpected')
    require(report.get('cleanup') == NATIVE_CLEANUP, 'native cleanup did not complete')
    require(report.get('production_activation') is False and report.get('remaining'),
            'native report omitted unresolved acceptance controls')
    require(report.get('producer_receipts') == {key: marker[key] for key in ('a', 'b')},
            'native producer bindings changed')
    require(report.get('trust_sha256') == trust_sha256, 'native public trust binding changed')
    commands = read_json(output / 'commands.json')
    require(isinstance(commands, list) and commands, 'native command ledger is missing')
    required = []
    probes = {stage + suffix for stage, _ in READINESS_STAGES
              for suffix in ('-process', '-process-after')}
    observed = set()
    for command in commands:
        name = command.get('name')
        require(name in EXPECTED_COMMANDS or name in probes, f'unexpected native command: {name}')
        observed.add(name)
        if name in EXPECTED_COMMANDS:
            required.append(name)
        allowed = {'initial-service': (0, 4), 'missing-upgrade-budget': (1,),
                   'zero-upgrade-budget': (2,), 'readiness-timeout': (1,),
                   'failed-start': (1,)}.get(name, (0,))
        require(type(command.get('exit')) is int and command['exit'] in allowed,
                f'unexpected native command exit: {name}')
        require(isinstance(command.get('argv'), list) and command['argv']
                and all(isinstance(arg, str) for arg in command['argv']),
                f'native command ledger lacks argv: {name}')
        for stream in ('stdout', 'stderr'):
            filename = command.get(stream)
            require(isinstance(filename, str) and Path(filename).name == filename,
                    'native command output path escapes evidence')
            info = native.custody(output / filename, os.geteuid())
            require(info.st_size <= native.MAX_COMMAND_OUTPUT + 65536,
                    'native command output exceeds bound')
    require(required == EXPECTED_COMMANDS, 'native command ledger is incomplete or out of order')
    require(probes <= observed, 'native command ledger lacks readiness observations')
    original = read_json(output / 'original-producer-verification.json')
    for name in ('a', 'b'):
        require(original[name]['sha256'] == marker[name]['sha256']
                and original[name]['archive_sha256'] == marker[name]['archive_sha256']
                and original[name]['embedded_revision_bytes'] == marker[name]['revision'],
                'original producer verification changed')
    for name, producer in READINESS_STAGES:
        readiness = read_json(output / (name + '-readiness.json'))
        require(readiness.get('pid', 0) > 0 and readiness.get('invocation')
                and readiness.get('process_start')
                and readiness['response'].get('serving_ready') is True
                and readiness['response']['artifact']['digest'] == marker[producer]['sha256'],
                f'native readiness evidence is incomplete: {name}')
    for fault in ('readiness-timeout', 'failed-start'):
        injection = read_json(output / (fault + '-watcher/result.json'))
        require(injection.get('injected') is True and 'error' not in injection,
                f'native fault injection did not complete: {fault}')
        state = read_json(output / (fault + '-state.json'))
        require(state['state']['disposition']['kind'] == 'blocked'
                and state['journal']['phase'] == 'containing'
                and state['journal']['disposition']['stop_intent'] is True,
                f'native containment evidence is incomplete: {fault}')
    return 12


def validate_unittest_output(output, count):
    passed = re.findall(r'^test_\S+ \(.+\) \.\.\. ok$', output, re.MULTILINE)
    require(len(passed) == count and len(set(passed)) == count
            and re.findall(r'^Ran (\d+) tests? in .+$', output, re.MULTILINE) == [str(count)]
            and re.findall(r'^OK.*$', output, re.MULTILINE) == ['OK'],
            f'expected exactly {count} unittest cases, all successful without skips')
    return count


def validate_boot_output(output):
    modes = re.findall(r'^PASS: installed ([^,]+), /readyz, /status,', output, re.MULTILINE)
    require(modes == BOOT_MODES, 'installed boot did not complete all five original modes')
    return len(modes)


def validate_boot_report(output):
    report = read_json(output / 'report.json')
    require(report.get('status') == 'completed' and 'error' not in report,
            'installed boot report did not complete')
    require(list(report.get('modes', {})) == BOOT_MODES, 'installed boot receipt lacks the original modes')
    for name, mode in report['modes'].items():
        require(mode.get('status') == 'completed' and mode.get('private_paths_absent') is True
                and mode.get('service_files') == [] and type(mode.get('host_returncode')) is int
                and mode['admission'].get('status') == 'completed'
                and mode['admission'].get('activation') is False
                and mode['admission']['result'].get('kind') == 'admitted'
                and mode['ready'].get('ready') is True and mode['ready'].get('phase') == 'ready'
                and mode['http_status'].get('status') == 'ok',
                f'installed boot mode is incomplete: {name}')
    commands = report.get('commands', [])
    require([item.get('name') for item in commands] == ['install', *['admit-' + name for name in BOOT_MODES]]
            and all(item.get('status') == 'completed' and item.get('returncode') == 0
                    and 'error' not in item for item in commands),
            'installed boot command ledger is incomplete')
    return 5


def validate_arguments(args):
    require(0 < args.timeout_secs <= 3600, 'operation timeout must be positive and at most 3600 seconds')
    require(platform.system() == 'Linux' and platform.machine() == 'aarch64',
            'integration requires native GNU ARM64 Linux')
    uid = os.geteuid()
    account = pwd.getpwuid(uid)
    home = Path(account.pw_dir)
    require(os.getuid() == uid and uid >= 1000 and account.pw_name.startswith('ww739-')
            and home == home.resolve(strict=True) and os.environ.get('HOME') == str(home)
            and Path.cwd() == home, 'integration requires the dedicated account and exact HOME/cwd')
    native.custody(home, uid, directory=True)
    paths = {}
    for name in ('inputs', 'packages', 'package_tool', 'output', 'temporary_root'):
        path = native.absolute_path(getattr(args, name))
        require(path.parent == path.parent.resolve(strict=True), f'{name} parent has aliases')
        if name == 'temporary_root':
            require(path == home.parent / 't' and path == path.resolve(strict=True),
                    'temporary root must be the explicitly provisioned fixture directory')
            native.custody(path, uid, directory=True)
            require(not any(path.iterdir()) and len(os.fsencode(path)) <= 16,
                    'temporary root must be empty and fit the native socket path budget')
        elif name != 'output':
            require(path == path.resolve(strict=True), f'{name} has aliases')
            native.custody(path, 0, directory=name in ('inputs', 'packages'))
        paths[name] = path
    output = paths['output']
    require(home in output.parents and output != home, 'evidence must be inside the disposable HOME')
    native.custody(output.parent, uid, directory=True)
    if output.exists() or output.is_symlink():
        native.custody(output, uid, directory=True)
        require(not any(output.iterdir()), 'evidence directory must be empty')
    output.mkdir(mode=0o700, exist_ok=True)
    output.chmod(0o700)
    return home, paths


def fingerprints(paths):
    return {str(path): {'sha256': native.sha256(path), 'size': path.stat().st_size}
            for path in paths}


def clear_temporary_root(path):
    # Root owns the parent so this account cannot replace sibling inputs/tools.
    # Remove owned contents; the provisioner removes the directory itself later.
    native.custody(path, os.geteuid(), directory=True)
    for child in path.iterdir():
        if child.is_dir() and not child.is_symlink():
            shutil.rmtree(child)
        else:
            child.unlink()
    require(not any(path.iterdir()), 'private fixture contents survived cleanup')


def run_lane(args, home, paths):
    output, inputs, packages = paths['output'], paths['inputs'], paths['packages']
    inherited_umask = os.umask(0o022)
    runner = native.Runner(home, output, args.timeout_secs)
    report = {'version': 1, 'status': 'RUNNING', 'integration_checks_passed': False,
              'native_acceptance': False, 'production_activation': False,
              'producer_required_test': {'a': 'failure', 'b': 'failure'},
              'development_profile_result': 'separate required development integration receipt; not inferred from release execution',
              'native_status': None, 'native_exit': None, 'completed': {},
              'overall_timeout_seconds': OVERALL_TIMEOUT_SECONDS,
              'inherited_umask': oct(inherited_umask), 'fixture_setup_umask': '0o022',
              'operation_timeout_seconds': args.timeout_secs}
    runner.save('user-result.json', report)
    deadline = time.monotonic() + OVERALL_TIMEOUT_SECONDS
    temporary = None
    original_env, original_tempdir = os.environ.copy(), tempfile.tempdir
    try:
        marker_dir = Path('/run/ww739-qualification')
        native.custody(marker_dir, 0, directory=True)
        marker = native.record(marker_dir / f'{os.geteuid()}.json', 0)
        verified = native.record(inputs / 'verified-inputs.json', 0)
        package_record = native.record(packages / 'packages.json', 0)
        report['producer_required_test'] = verify_input_policy(verified)
        report['input_policy'] = verified['policy']
        report['slot_identities'] = verified.get('slot_identities', {'a': 'original-producer-A', 'b': 'original-producer-B'})
        require(package_record.get('private_signer_removed') is True
                and package_record.get('publication') is False,
                'fixture signer custody or publication policy is missing')
        require({key: verified[key] for key in ('a', 'b')}
                == {key: marker[key] for key in ('a', 'b')}, 'verified producer receipts differ from root marker')
        for key in ('release_a', 'release_b'):
            require(package_record[key] == marker[key], 'fixture release roots differ from root marker')
        for key in ('a', 'b'):
            require(Path(marker[key]['path']) == inputs / key / 'ww', 'marker does not name verified Host')
        trust = packages / 'trusted-keys'
        native.custody(trust, 0)
        require(package_record.get('trust_sha256') == native.sha256(trust)
                and package_record.get('package_tool_sha256') == native.sha256(paths['package_tool']),
                'fixture public trust or package tool differs from preparation receipt')
        # Reuse the existing original-producer and account custody gate before
        # executing either Host. The placeholder endpoint is only syntax-checked
        # here; the actual run below receives the isolated Kubo endpoint.
        preflight = argparse.Namespace(platform='linux-aarch64', home=str(home),
                    service_manager='systemd', release_a=package_record['release_a'],
                    release_b=package_record['release_b'], trusted_release_keys=str(trust),
                    ipfs_url='http://127.0.0.1:1', timeout_secs=args.timeout_secs,
                    output=str(output / 'native'))
        _, _, _, checked_marker = native.validate_arguments(preflight)
        require(checked_marker == marker, 'qualification marker changed during preflight')
        ipfs = shutil.which('ipfs')
        require(ipfs is not None, 'required pinned ARM64 Kubo is unavailable')
        ipfs = Path(ipfs).resolve(strict=True)
        native.custody(ipfs, 0)
        require(os.access(paths['package_tool'], os.X_OK), 'package utility is not executable')
        sources = [Path(__file__).resolve(), REPO / 'scripts/qualify_native_upgrade.py',
                   REPO / 'scripts/install.sh', *[REPO / 'tests' / name for name in
                   ('test_host_install.py', 'test_install.py', 'test_installed_boot.py',
                    'release_auth_fixture.py', 'kubo_fixture.py')]]
        artifacts = [inputs / 'verified-inputs.json', packages / 'packages.json', trust]
        for key in ('a', 'b'):
            artifacts.extend([inputs / key / name for name in ('ww', 'REVISION', 'PROVENANCE.json', 'SHA256SUMS')])
            artifacts.extend(sorted(path for path in (inputs / key / 'fhs').rglob('*') if path.is_file()))
            artifacts.append(packages / key / 'release.car')
        runner.save('source-and-tool-hashes.json', {'sources': fingerprints(sources),
                    'tools': fingerprints([ipfs, paths['package_tool'], Path(sys.executable).resolve()]),
                    'artifacts': fingerprints(artifacts)})
        runner.save('verified-inputs.json', verified)
        runner.save('packages.json', package_record)
        runner.save('root-marker.json', marker)
        private = paths['temporary_root']
        temporary = private
        report['temporary_root'] = str(private)
        runner.env.update(TMPDIR=str(private), PYTHONUNBUFFERED='1', PYTHONDONTWRITEBYTECODE='1', NO_COLOR='1')
        os.environ.clear()
        os.environ.update(runner.env)
        tempfile.tempdir = str(private)

        def run(name, argv, budget, expected=0):
            # The unmodified Runner independently bounds output and kills/reaps
            # the owned child process group on timeout. Each whole-stage budget
            # remains separate from the Host's unchanged 30-second guard.
            runner.timeout = budget
            return runner.run(name, argv, expected=expected, deadline=deadline)

        def suite(name, argv, count, budget):
            stdout = run(name, argv, budget)
            stderr = (output / runner.commands[-1]['stderr']).read_text()
            report['completed'][name] = validate_unittest_output(stdout + '\n' + stderr, count)
            runner.save('user-result.json', report)

        run('kubo-version', [ipfs, 'version', '--all'], 30)
        binary, fhs = inputs / 'b/ww', inputs / 'b/fhs'
        runner.env.update(WW_TEST_BINARY=str(binary), WW_TEST_FHS=str(fhs))
        suite('host-install', [sys.executable, REPO / 'tests/test_host_install.py', '-v'], 21, 900)
        del runner.env['WW_TEST_BINARY'], runner.env['WW_TEST_FHS']
        suite('installer', [sys.executable, REPO / 'tests/test_install.py', '--binary', binary,
                           '--fhs', fhs, '--package-tool', paths['package_tool'], '-v'], 14, 900)
        boot_stdout = run('installed-boot', [sys.executable, REPO / 'tests/test_installed_boot.py', binary, fhs,
                          '--guarded', '--package-tool', paths['package_tool'],
                          '--producer-provenance', inputs / 'b/PROVENANCE.json',
                          *(['--current-build-receipt', inputs / 'b/BUILD-RECEIPT.json', '--build-profile', 'release']
                            if verified['policy'] == 'pr808-current-source-native-diagnostic-v1' else []),
                          '--admission-timeout-secs', args.timeout_secs, '--evidence', output / 'boot'],
                          600 + 5 * args.timeout_secs)
        validate_boot_output(boot_stdout)
        report['completed']['installed-boot'] = validate_boot_report(output / 'boot')
        runner.save('user-result.json', report)
        # Existing suites use temporary HOME fixtures, leaving the real account
        # empty for the native qualifier's custody checks.
        with isolated_kubo() as kubo:
            endpoint = urlsplit(kubo['IPFS_API'])
            require(endpoint.scheme == 'http' and endpoint.hostname == '127.0.0.1'
                    and endpoint.port, 'Kubo fixture did not yield a loopback endpoint')
            api = f'/ip4/127.0.0.1/tcp/{endpoint.port}'
            runner.save('kubo-fixture.json', {'endpoints': kubo, 'offline': True,
                        'helper': 'tests/kubo_fixture.py:isolated_kubo', 'cleanup': 'pending'})
            for key in ('a', 'b'):
                run('import-' + key, [ipfs, '--api', api, 'dag', 'import', packages / key / 'release.car'], 120)
            native_argv = [sys.executable, REPO / 'scripts/qualify_native_upgrade.py',
                           '--platform', 'linux-aarch64', '--home', home, '--service-manager', 'systemd',
                           '--release-a', package_record['release_a'], '--release-b', package_record['release_b'],
                           '--trusted-release-keys', trust, '--ipfs-url', kubo['IPFS_API'],
                           '--timeout-secs', args.timeout_secs, '--output', output / 'native']
            run('native-qualification', native_argv, 600 + 18 * args.timeout_secs, expected=2)
            report['native_exit'] = runner.commands[-1]['exit']
            report['native_status'] = read_json(output / 'native/report.json').get('status')
            report['completed']['native'] = validate_native_result(output / 'native', report['native_exit'],
                                                                  marker, native.sha256(trust))
        runner.save('kubo-fixture.json', {'endpoints': kubo, 'offline': True,
                    'helper': 'tests/kubo_fixture.py:isolated_kubo', 'cleanup': 'completed'})
        unit = home / '.config/systemd/user/ww.service'
        require(not unit.exists() and not unit.is_symlink(), 'native service definition survived cleanup')
        clear_temporary_root(private)
        temporary = None
        report.update(status='COMPLETED', integration_checks_passed=True,
                      private_fixture_cleanup='completed; temporary contents and Kubo removed; root removes empty directory',
                      native_installation='retained privately for root cleanup; not public evidence')
    except BaseException as error:
        report.update(status='FAILED', error=f'{type(error).__name__}: {error}')
        # Preserve the native terminal state even when the outer command fails.
        commands = [item for item in runner.commands if item['name'] == 'native-qualification']
        if commands:
            report['native_exit'] = commands[-1]['exit']
        if (output / 'native/report.json').is_file():
            try:
                report['native_status'] = read_json(output / 'native/report.json').get('status')
            except (OSError, RuntimeError, ValueError):
                pass
    finally:
        if temporary is not None:
            try:
                private = temporary
                clear_temporary_root(private)
                report['private_fixture_cleanup'] = 'completed after failure'
            except BaseException as error:
                report.update(status='FAILED', integration_checks_passed=False, cleanup_error=str(error))
        os.environ.clear()
        os.environ.update(original_env)
        tempfile.tempdir = original_tempdir
        os.umask(inherited_umask)
        runner.save('user-result.json', report)
    return 0 if report['integration_checks_passed'] else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('inputs', 'packages', 'package-tool', 'output', 'temporary-root'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--timeout-secs', type=int, required=True)
    args = parser.parse_args(argv)
    home, paths = validate_arguments(args)
    result = run_lane(args, home, paths)
    print((paths['output'] / 'user-result.json').read_text())
    return result


if __name__ == '__main__':
    raise SystemExit(main())
