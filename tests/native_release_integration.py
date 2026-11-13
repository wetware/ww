#!/usr/bin/env python3
"""Validate native qualification evidence offline; live execution requires PR 3.

Completion means only the existing integration subset completed. The unchanged
native qualification report remains PARTIAL and full native acceptance is false.
Root owns account/service-manager provisioning and final account cleanup.
"""
import json
import os
from pathlib import Path
import platform
import pwd
import re
import shutil
import sys

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / 'scripts'))
import qualify_native_upgrade as native


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
    return native.unavailable()


def main(argv=None):
    return native.unavailable()


if __name__ == '__main__':
    raise SystemExit(main())
