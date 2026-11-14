#!/usr/bin/env python3
"""Run and validate bounded native qualification in an owned disposable account.

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
from prepare_current_native_release import PAIR_POLICY, verify_component_binding, validate_pair_roles
from kubo_fixture import isolated_kubo


def verify_input_policy(verified):
    require(verified.get('schema') == 2 and verified.get('policy') == PAIR_POLICY
            and verified.get('status') == 'VERIFIED_DIAGNOSTIC_INPUTS'
            and verified.get('native_acceptance') is False
            and verified.get('publication_allowed') is False
            and verified.get('required_checks_success') is False,
            'input verifier did not preserve the reduced diagnostic-only policy')
    validate_pair_roles(verified['current_builds'], verified['source_roles'])
    require(verified['slot_identities'] == {'a': 'reduced-current-R1', 'b': 'reduced-current-R0'},
            'current pair slot identities differ')
    require(verified['a']['sha256'] != verified['b']['sha256'], 'current pair requires distinct executable bytes')
    for slot in ('a', 'b'):
        build, receipt = verified['current_builds'][slot], verified[slot]
        source = build['source']
        require(source['runtime_source_equal'] is True and verify_component_binding(source)
                and source['checkout_revision'] == source['artifact_revision'] == receipt['revision']
                and build['ci']['revision'] == receipt['revision']
                and build['host']['sha256'] == receipt['sha256']
                and build['builder'] == receipt['builder'] and build['profile'] == 'release'
                and build['ci']['completed_job_receipt'] is False
                and build['ci']['uploaded_producer_artifact'] is False
                and verified['producer_evidence'][slot]['fhs'] == build['fhs'],
                'current candidate identity or build evidence differs')
    return {slot: 'not-a-completed-CI-check' for slot in ('a', 'b')}


EXPECTED_CHECKS = [
    'authenticated current R1 installs without startup admission',
    'ordinary systemd R1 starts with loopback serving readiness',
    'missing and zero changed-upgrade budgets refuse without native or service mutation',
    'current R1 to R0 commits matching provenance and readiness',
    'retained rollback restores R1 with measured readiness',
    'enabled R1 to R0 to R1 preserves enablement and custom unit bytes',
    *[fault + (': exact candidate signaled through pidfd; V3 intent retained; installed CLI recovery restored ready R1' if fault == 'updater-interrupted' else ': exact candidate signaled through pidfd; bounded live rollback restored ready R1')
      for fault in ('readiness-timeout', 'failed-start', 'updater-interrupted')],
    'recovery retry without pending transaction preserves state',
    'exact-artifact no-op preserves provenance',
    'disabled stopped upgrade and rollback remain disabled and stopped',
    'custom configuration, identity, FHS and private bytes preserved',
    'corrupt retained bytes refuse rollback without replacement',
    'ordinary startup ignores damaged native journal and mutation refuses',
    'acknowledged baseline repair archives evidence without activation or invented rollback',
    'next upgrade after repair retains authenticated current bytes for rollback',
]
# Readiness probes may repeat; all operations must appear once in this order.
EXPECTED_COMMANDS = '''initial-service install-a daemon-register daemon-reload fault-stop-policy
 enable-before-startup enabled-before-startup start-a enabled-a upgrade-check
 budget-before-manager missing-upgrade-budget zero-upgrade-budget budget-after-manager
 upgrade-a-b enabled-b rollback-b-a enabled-restored-a noop-before-manager exact-noop
 noop-after-manager readiness-timeout-original-manager readiness-timeout
 readiness-timeout-enabled-after-fault
 readiness-timeout-recover readiness-timeout-enabled-after-recovery
 failed-start-original-manager failed-start
 failed-start-enabled-after-fault failed-start-recover failed-start-enabled-after-recovery
 updater-interrupted-original-manager updater-interrupted updater-interrupted-enabled-after-fault
 updater-interrupted-recover updater-interrupted-enabled-after-recovery recovery-idempotent
 stop-before-stopped-upgrade disable-before-stopped-upgrade disabled-before-stopped-upgrade
 stopped-upgrade stopped-pid disabled-after-stopped-upgrade
 stopped-rollback stopped-rollback-pid disabled-after-stopped-rollback
 corrupt-rollback-service-before corrupt-rollback corrupt-rollback-service-after ordinary-damaged-start ordinary-damaged-stop damaged-mutation-refusal
 repair-missing-ack repair-baseline repair-stopped-pid repair-disabled
 post-repair-upgrade post-repair-rollback cleanup-stop cleanup-disable cleanup-reload'''.split()
COMMAND_EXITS = {'initial-service': (0, 4), 'missing-upgrade-budget': (1,),
                 'zero-upgrade-budget': (2,), 'readiness-timeout': (1,), 'failed-start': (1,),
                 'updater-interrupted': (-9,), 'readiness-timeout-recover': (1,), 'failed-start-recover': (1,), 'recovery-idempotent': (1,), 'corrupt-rollback': (1,),
                 'damaged-mutation-refusal': (1,), 'repair-missing-ack': (1,)}
BOOT_MODES = ['configured', 'bare', 'local-target', 'immutable-target', 'explicit-root']
READINESS_STAGES = [('a', 'a'), ('b', 'b'), ('restored-a', 'a'),
                    ('readiness-timeout-restored-a', 'a'), ('failed-start-restored-a', 'a'),
                    ('updater-interrupted-restored-a', 'a'), ('damaged-ordinary', 'a')]
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
    require(report.get('version') == 2 and report.get('platform') == 'linux-aarch64'
            and report.get('status') == 'PARTIAL', 'native report must remain terminal PARTIAL')
    require('error' not in report and 'cleanup_error' not in report, 'native report contains an error field')
    require(report.get('checks') == EXPECTED_CHECKS and report.get('executed_checks') == len(EXPECTED_CHECKS),
            'native checks are missing, reordered or unexpected')
    require(report.get('cleanup') == NATIVE_CLEANUP, 'native cleanup did not complete')
    require(report.get('production_activation') is False and report.get('native_acceptance') is False and report.get('remaining')
            and report.get('wrong_executable_interception') == 'UNQUALIFIED; not attempted',
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
        allowed = COMMAND_EXITS.get(name, (0,))
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
    initiator = read_json(output / 'upgrade-a-b-initiator.json')
    upgrade = next(command for command in commands if command['name'] == 'upgrade-a-b')
    require(initiator.get('pid', 0) > 0 and initiator.get('uid') == os.geteuid()
            and initiator.get('pidfd_live') is True and initiator.get('process_start')
            and initiator.get('artifact_size', 0) > 0
            and initiator.get('argv') == upgrade['argv']
            and initiator.get('sha256') == marker['a']['sha256']
            and initiator.get('revision') == marker['a']['revision'],
            'forward upgrade lacks exact initiating executable evidence')
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
                and readiness['response'].get('status') == 200
                and readiness['response']['body'].get('ready') is True
                and readiness['response']['body'].get('phase') == 'ready'
                and readiness.get('installed_file')
                and readiness['artifact']['digest'] == marker[producer]['sha256'],
                f'native readiness evidence is incomplete: {name}')
    for fault in ('readiness-timeout', 'failed-start', 'updater-interrupted'):
        injection = read_json(output / (fault + '-watcher/result.json'))
        require(injection.get('injected') is True and 'error' not in injection,
                f'native fault injection did not complete: {fault}')
        state = read_json(output / (fault + '-state.json'))
        if fault == 'updater-interrupted':
            require(state['journal']['phase'] in ('Starting', 'Verified')
                    and state['journal']['operation'] == 'Upgrade'
                    and state['journal']['stop_intent'] is True
                    and state['journal']['transaction_id'] == injection.get('transaction_id')
                    and state['installed_sha256'] == marker['b']['sha256'],
                    'native interrupted transaction evidence is incomplete')
            require(injection.get('updater_pid', 0) > 0 and injection.get('updater_signal') == 'SIGKILL',
                    'updater interruption evidence is incomplete')
        else:
            require(state['journal'] is None and state['installed_sha256'] == marker['a']['sha256'],
                    'native live rollback evidence is incomplete')
    corruption = read_json(output / 'corrupt-rollback.json')
    require(corruption['before_sha256'] == marker['b']['sha256']
            and corruption['after_sha256'] != corruption['before_sha256']
            and corruption['installed_sha256'] == marker['a']['sha256']
            and corruption['result']['status'] == 'failed'
            and corruption['service_before'] == corruption['service_after']
            and set(corruption['service_before']) == {'MainPID', 'InvocationID', 'ActiveState', 'SubState', 'UnitFileState'},
            'corrupt rollback evidence is incomplete')
    scratch = corruption['scratch_stage']
    if scratch is not None:
        require(0 < corruption['source_size'] <= 1024 ** 3
                and scratch['size'] == corruption['source_size']
                and scratch['sha256'] == corruption['after_sha256'],
                'corrupt rollback scratch evidence differs')
    repair = read_json(output / 'repair-baseline.json')
    result = repair['result']
    require(result['status'] == 'completed' and result['outcome']['kind'] == 'repaired-baseline'
            and result['outcome']['historical_rollback_available'] is False
            and result['outcome']['live_ready'] is False and result['activation'] is False
            and result['changed_paths'] == [] and repair['manifest']['committed'] is True
            and repair['manifest']['historical_rollback_available'] is False
            and repair['archived_journal_sha256'] == repair['damaged_journal_sha256']
            and repair['retained_corruption_sha256'] == corruption['after_sha256'],
            'baseline repair evidence is incomplete')
    return len(EXPECTED_CHECKS)


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
                and mode['launch']['argv'][1] == 'run'
                and mode['launch']['stdin'] == 'DEVNULL'
                and 'admission' not in mode
                and mode['native_metadata_before'] == mode['native_metadata_after']
                and mode['ready'].get('ready') is True and mode['ready'].get('phase') == 'ready'
                and mode['http_status'].get('status') == 'ok',
                f'installed boot mode is incomplete: {name}')
    commands = report.get('commands', [])
    require([item.get('name') for item in commands] == ['install']
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
              'producer_required_test': {},
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
        report['qualification_scope'] = 'final-head R1 forward to current-compatible R0 in a later fixture envelope; retained rollback to R1'
        report['slot_identities'] = verified['slot_identities']
        require(package_record.get('private_signer_removed') is True
                and package_record.get('publication') is False,
                'fixture signer custody or publication policy is missing')
        require({key: verified[key] for key in ('a', 'b')}
                == {key: marker[key] for key in ('a', 'b')}, 'verified producer receipts differ from root marker')
        require(package_record['a']['identity']['sequence'] == 1
                and package_record['b']['identity']['sequence'] == 2
                and package_record['a']['version'] == package_record['b']['version']
                and package_record['a']['host_sha256'] == marker['a']['sha256']
                and package_record['b']['host_sha256'] == marker['b']['sha256'],
                'current forward fixture must preserve same-version changed bytes and increasing release sequences')
        for key in ('release_a', 'release_b'):
            require(package_record[key] == marker[key], 'fixture release roots differ from root marker')
        for key in ('a', 'b'):
            require(Path(marker[key]['path']) == inputs / key / 'ww', 'marker does not name verified Host')
        trust = packages / 'trusted-keys'
        native.custody(trust, 0)
        require(package_record.get('trust_sha256') == native.sha256(trust)
                and package_record.get('package_tool_sha256') == native.sha256(paths['package_tool']),
                'fixture public trust or package tool differs from preparation receipt')
        # Reuse the existing executable and account custody gate before
        # executing either Host. The placeholder endpoint is only syntax-checked
        # here; the actual run below receives the isolated Kubo endpoint.
        preflight = argparse.Namespace(platform='linux-aarch64', home=str(home),
                    service_manager='systemd', release_a=package_record['release_a'],
                    release_b=package_record['release_b'], trusted_release_keys=str(trust),
                    ipfs_url='http://127.0.0.1:1', timeout_secs=args.timeout_secs,
                    output=str(output / 'native'))
        _, _, _, checked_marker = native.validate_arguments(preflight)
        require(marker['input_manifest_sha256'] == native.sha256(inputs / 'verified-inputs.json'),
                'root marker does not bind the input manifest')
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
            artifacts.extend([inputs / key / name for name in ('ww', 'REVISION', 'PROVENANCE.json', 'SHA256SUMS', 'BUILD-RECEIPT.json', 'candidate.json')])
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
        binary, fhs = inputs / 'a/ww', inputs / 'a/fhs'
        runner.env.update(WW_TEST_BINARY=str(binary), WW_TEST_FHS=str(fhs))
        suite('host-install', [sys.executable, REPO / 'tests/test_host_install.py', '-v'], 21, 900)
        del runner.env['WW_TEST_BINARY'], runner.env['WW_TEST_FHS']
        suite('installer', [sys.executable, REPO / 'tests/test_install.py', '--binary', binary,
                           '--fhs', fhs, '--package-tool', paths['package_tool'], '-v'], 14, 900)
        boot_stdout = run('installed-boot', [sys.executable, REPO / 'tests/test_installed_boot.py', binary, fhs,
                          '--authenticated', '--package-tool', paths['package_tool'],
                          '--producer-provenance', inputs / 'a/PROVENANCE.json',
                          '--current-build-receipt', inputs / 'a/BUILD-RECEIPT.json', '--build-profile', 'release',
                          '--startup-timeout-secs', args.timeout_secs, '--evidence', output / 'boot'],
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
            run('native-qualification', native_argv, 900 + 22 * args.timeout_secs, expected=2)
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
