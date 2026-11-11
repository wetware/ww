#!/usr/bin/env python3
"""Run all five admitted development Host boot modes in an owned ARM64 account."""

import argparse
import json
import os
from pathlib import Path
import platform
import pwd
import shutil
import sys
import tempfile
import time
import uuid

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tests'))
from native_release_integration import validate_boot_report
from prepare_current_native_release import verify_build_files, verify_candidate
from run_native_release_integration import (
    Provisioner, copy_evidence, quiesce_account, regular_tree, write_json,
)
from qualify_native_upgrade import custody, require, sha256


def verify_runner_inputs(checkout, inputs, revision, tools):
    """Validate runner-owned inputs before making fresh root-owned copies."""
    owner = checkout.stat().st_uid
    custody(checkout, owner, directory=True)
    custody(inputs, owner, directory=True)
    for path in tools:
        custody(path, owner)
    # The source verifier scopes Git safe.directory to this canonical checkout
    # in each read-only command; no root-global Git setting changes.
    return verify_candidate(checkout, inputs, revision, 'development', owner=owner)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--inputs', type=Path, required=True)
    parser.add_argument('--revision', required=True)
    parser.add_argument('--package-tool', type=Path, required=True)
    parser.add_argument('--ipfs', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--timeout-secs', type=int, required=True)
    parser.add_argument('--disposable-runner', action='store_true', required=True)
    args = parser.parse_args()
    inherited_umask = os.umask(0o022)
    require(os.geteuid() == 0 and platform.system() == 'Linux'
            and platform.machine() == 'aarch64', 'requires root on disposable native GNU ARM64')
    require(Path('/proc/1/comm').read_text().strip() == 'systemd', 'requires actual systemd PID 1')
    require(0 < args.timeout_secs <= 120, 'explicit admission budget must be 1..120 seconds')
    for path in (args.inputs, args.package_tool, args.ipfs):
        require(path.is_absolute() and path.resolve(strict=True) == path,
                'inputs must be canonical absolute paths')
    require(args.output.is_absolute() and not args.output.exists(), 'output must be a new absolute directory')
    checkout = Path(__file__).resolve().parents[1]
    verify_runner_inputs(checkout, args.inputs, args.revision, (args.package_tool, args.ipfs))
    args.output.mkdir(mode=0o755, parents=True)
    worker = Provisioner(args.output)
    # Leave a separate three-minute cleanup window inside the CI step's bound.
    worker.deadline = time.monotonic() + 15 * 60
    work = Path(tempfile.mkdtemp(prefix='w', dir='/tmp'))
    work.chmod(0o755)
    username = 'ww739-' + uuid.uuid4().hex[:12]
    account = user_output = None
    report = {'status': 'FAILED', 'profile': 'development', 'revision': args.revision,
              'release_acceptance': False, 'wrong_executable_interception': 'UNQUALIFIED; not attempted',
              'inherited_umask': oct(inherited_umask), 'fixture_setup_umask': '0o022', 'work': str(work)}
    try:
        code = work / 'code'
        code.mkdir(mode=0o755)
        for folder in ('scripts', 'tests'):
            regular_tree(checkout / folder, code / folder)
        inputs = work / 'inputs'
        regular_tree(args.inputs, inputs)
        verify_build_files(inputs, 'development', owner=0)
        tools = work / 'tools'
        tools.mkdir(mode=0o755)
        for source, name in ((args.package_tool, 'ww-release-package'), (args.ipfs, 'ipfs')):
            shutil.copyfile(source, tools / name)
            (tools / name).chmod(0o755)
            require(sha256(source) == sha256(tools / name), 'tool copy changed')
        report['build_receipt'] = json.loads((inputs / 'BUILD-RECEIPT.json').read_text())
        report['tools_sha256'] = {p.name: sha256(p) for p in tools.iterdir()}
        home = work / 'h'
        try:
            pwd.getpwnam(username)
        except KeyError:
            pass
        else:
            raise RuntimeError('generated fixture account already exists')
        try:
            worker.run('create-account', ['useradd', '--create-home', '--home-dir', home,
                                          '--shell', '/bin/sh', username])
        finally:
            try:
                created = pwd.getpwnam(username)
            except KeyError:
                pass
            else:
                require(created.pw_uid >= 1000 and created.pw_dir == str(home), 'created account differs')
                account = created
        require(account is not None, 'useradd did not create the fixture account')
        home.chmod(0o700)
        temporary = work / 't'
        temporary.mkdir(mode=0o700)
        os.chown(temporary, account.pw_uid, account.pw_gid)
        report.update(uid=account.pw_uid, username=username, home=str(home))
        worker.run('start-user-manager', ['systemctl', 'start', f'user@{account.pw_uid}.service'])
        runtime = f'/run/user/{account.pw_uid}'
        deadline = time.monotonic() + 15
        while True:
            result = worker.run('user-bus-ready', ['runuser', '-u', username, '--', 'busctl',
                '--address=unix:path=' + runtime + '/bus', 'call', 'org.freedesktop.DBus',
                '/org/freedesktop/DBus', 'org.freedesktop.DBus', 'NameHasOwner', 's',
                'org.freedesktop.systemd1'], timeout=5, check=False)
            if result.returncode == 0 and result.stdout.strip() == b'b true':
                break
            require(time.monotonic() < deadline, 'user manager did not acquire its bus name')
            time.sleep(0.1)
        worker.run('environment', ['sh', '-c', 'uname -a; ldd --version; systemd --version; cat /etc/os-release'])
        user_output = home / 'evidence'
        command = ['runuser', '-u', username, '--', 'env', '-i', 'HOME=' + str(home),
            'USER=' + username, 'LOGNAME=' + username, 'LANG=C.UTF-8',
            'XDG_RUNTIME_DIR=' + runtime, 'PATH=' + str(tools) + ':/usr/bin:/bin',
            'DBUS_SESSION_BUS_ADDRESS=unix:path=' + runtime + '/bus',
            'TMPDIR=' + str(temporary), 'PYTHONDONTWRITEBYTECODE=1',
            'sh', '-c', 'cd "$HOME" && exec "$@"', 'ww739-development-boot',
            'python3', str(code / 'tests/test_installed_boot.py'), str(inputs / 'ww'), str(inputs / 'fhs'),
            '--guarded', '--package-tool', str(tools / 'ww-release-package'),
            '--producer-provenance', str(inputs / 'PROVENANCE.json'),
            '--current-build-receipt', str(inputs / 'BUILD-RECEIPT.json'),
            '--build-profile', 'development',
            '--admission-timeout-secs', str(args.timeout_secs), '--evidence', str(user_output)]
        result = worker.run('development-boot', command, timeout=15 * 60, check=False)
        report['user_exit'] = result.returncode
        require(result.returncode == 0, 'development boot failed; retained result remains failed')
        report['status'] = 'COMPLETED_AWAITING_COPIED_EVIDENCE'
    except Exception as error:
        report['error'] = str(error)
    finally:
        worker.deadline = time.monotonic() + 180
        cleanup = []
        if account is not None:
            try:
                require(pwd.getpwnam(username) == account, 'fixture account identity changed')
                worker.run('stop-user-manager', ['systemctl', 'stop', f'user@{account.pw_uid}.service'])
                report['cleanup_killed_pids'] = quiesce_account(account)
                if user_output is not None and user_output.exists():
                    report['evidence_sha256'] = copy_evidence(user_output, args.output / 'user')
                worker.run('delete-account', ['userdel', username])
                if report['status'] == 'COMPLETED_AWAITING_COPIED_EVIDENCE':
                    # The account is quiescent and the bounded copy belongs to
                    # root. Validate that retained receipt under root custody.
                    report['boot'] = validate_boot_report(args.output / 'user')
                    report['status'] = 'PASS_CURRENT_SOURCE_DEVELOPMENT_BOOT'
            except Exception as error:
                cleanup.append(str(error))
        if not cleanup:
            shutil.rmtree(work)
        report['cleanup_errors'] = cleanup
        report['fixture_directory_removed'] = not work.exists()
        if cleanup:
            report['status'] = 'FAILED'
        write_json(args.output / 'provision-result.json', report)
    print(json.dumps({'status': report['status'], 'report': str(args.output / 'provision-result.json')}))
    return 0 if report['status'] == 'PASS_CURRENT_SOURCE_DEVELOPMENT_BOOT' else 1


if __name__ == '__main__':
    sys.exit(main())
