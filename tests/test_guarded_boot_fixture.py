#!/usr/bin/env python3
"""Fixture controls only; these do not qualify native startup or systemd."""
from contextlib import contextmanager, redirect_stderr, redirect_stdout
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import urllib.request

import release_auth_fixture as auth
import test_installed_boot as boot


class GuardedBootFixture(unittest.TestCase):
    def test_guarded_supervision_preserves_context_and_uses_a_private_group(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            evidence = boot.GuardedEvidence(root / 'evidence')
            env = dict(os.environ, GUARD_TEST_VALUE='exact-context')
            code = ('import json,os; print(json.dumps(dict(cwd=os.getcwd(), '
                    'value=os.environ["GUARD_TEST_VALUE"], stdin=os.read(0,1).decode(), '
                    'pid=os.getpid(), group=os.getpgrp(), session=os.getsid(0))))')
            value = json.loads(evidence.run('context', [sys.executable, '-c', code], root, env, 5))
            self.assertEqual(value['cwd'], str(root))
            self.assertEqual(value['value'], 'exact-context')
            self.assertEqual(value['stdin'], '')
            self.assertEqual(value['group'], value['pid'])
            self.assertEqual(value['session'], value['pid'])

    def test_guarded_command_output_overflow_is_capped_and_refused(self):
        self.assertTrue(hasattr(boot, 'MAX_COMMAND_OUTPUT'), 'bounded command capture is missing')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = boot.GuardedEvidence(root / 'evidence')
            command = [sys.executable, '-c', 'import os,time; os.write(1,b"x"*100000); time.sleep(30)']
            with patch.object(boot, 'MAX_COMMAND_OUTPUT', 4096):
                with self.assertRaisesRegex(RuntimeError, 'output exceeds'), evidence:
                    evidence.run('overflow', command, root, dict(os.environ), 5)
            self.assertEqual((evidence.path / 'overflow.stdout').stat().st_size, 4096)
            record = evidence.report['commands'][0]
            self.assertEqual(record['status'], 'failed')
            self.assertIsNotNone(record['returncode'])

    def test_guarded_timeout_kills_descendants_even_after_the_leader_exits(self):
        self.assertTrue(hasattr(boot, 'BoundedProcess'), 'process-group supervision is missing')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            # The descendant inherits the capture pipes. It must never reach its
            # delayed write after either a live-leader timeout or early leader exit.
            for exits in (False, True):
                with self.subTest(leader_exits=exits):
                    sentinel = root / f'escaped-{exits}'
                    code = ('import os,time\n'
                            'pid=os.fork()\n'
                            'if pid == 0:\n'
                            ' time.sleep(1.0)\n'
                            f' open({str(sentinel)!r},"w").write("escaped")\n'
                            ' os._exit(0)\n'
                            + ('os._exit(0)\n' if exits else 'time.sleep(30)\n'))
                    child = boot.BoundedProcess([sys.executable, '-c', code], root, dict(os.environ),
                                                root / f'{exits}.stdout', root / f'{exits}.stderr',
                                                timeout=0.2, limit=4096)
                    if exits:
                        self.assertEqual(child.wait(), 0)
                    else:
                        with self.assertRaises(subprocess.TimeoutExpired):
                            child.wait()
                    self.assertIsNotNone(child.returncode)
            time.sleep(1.1)
            self.assertEqual(list(root.glob('escaped-*')), [])

    def test_guarded_host_capture_stays_bounded_while_caller_is_busy(self):
        self.assertTrue(hasattr(boot, 'BoundedProcess'), 'background Host capture is missing')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / 'host.log'
            child = boot.BoundedProcess(
                [sys.executable, '-c', 'import os,time; os.write(2,b"x"*100000); time.sleep(30)'],
                root, dict(os.environ), log, None, timeout=5, limit=4096)
            time.sleep(0.15)
            with self.assertRaisesRegex(RuntimeError, 'output exceeds'):
                child.stop()
            self.assertEqual(log.stat().st_size, 4096)
            self.assertIsNotNone(child.returncode)

    def test_guarded_package_failures_retain_bounded_diagnostics(self):
        self.assertTrue(hasattr(boot, 'BoundedProcess'), 'guarded package supervision is missing')
        for stage in ('prepare', 'finalize'):
            with self.subTest(stage=stage), tempfile.TemporaryDirectory() as directory:
                root = Path(directory).resolve()
                evidence = boot.GuardedEvidence(root / 'evidence')
                with tempfile.TemporaryDirectory() as temporary:
                    fixture_root = Path(temporary).resolve()
                    binary, tool, sidecar = [fixture_root / name for name in ('ww', 'tool', 'PROVENANCE.json')]
                    binary.write_bytes(b'not executed')
                    payload = fixture_root / 'fhs'
                    payload.mkdir()
                    sidecar.write_text(json.dumps({'repository': 'https://github.com/wetware/ww',
                        'revision': 'd' * 40,
                        'builder': 'github-actions:42648050888:1:aarch64-unknown-linux-gnu'}))
                    tool.write_text(f'#!{sys.executable}\n'
                        'import pathlib,sys\n'
                        f'if sys.argv[1] == {stage!r}:\n'
                        ' print("package stdout diagnostic", flush=True)\n'
                        ' print("package stderr diagnostic", file=sys.stderr, flush=True)\n'
                        ' sys.exit(7)\n'
                        'tree=pathlib.Path(sys.argv[sys.argv.index("--tree")+1])\n'
                        '(tree/"CHECKSUMS.txt").write_text("fixture checksums")\n')
                    args = SimpleNamespace(binary=binary, fhs=payload, package_tool=tool,
                                           producer_provenance=sidecar)
                    with self.assertRaises(subprocess.CalledProcessError), evidence:
                        boot.install_guarded(args, fixture_root, fixture_root / 'home', dict(os.environ), evidence)
                    self.assertFalse((fixture_root / 'bootstrap/keys/private.pem').exists())
                self.assertEqual((evidence.path / f'package-{stage}.stdout').read_text(), 'package stdout diagnostic\n')
                self.assertEqual((evidence.path / f'package-{stage}.stderr').read_text(), 'package stderr diagnostic\n')
                record = evidence.report['package_commands'][-1]
                self.assertEqual(record['status'], 'failed')
                self.assertEqual(record['returncode'], 7)

    def test_guarded_options_are_explicit_and_positive(self):
        self.assertTrue(hasattr(boot, 'parse_args'), 'guarded parser is missing')
        options = ['ww', 'fhs', '--guarded', '--package-tool', 'package',
                   '--producer-provenance', 'PROVENANCE.json',
                   '--admission-timeout-secs', '300', '--evidence', 'evidence']
        args = boot.parse_args(options)
        self.assertEqual((args.binary, args.fhs), (Path('ww'), Path('fhs')))
        self.assertEqual(args.admission_timeout_secs, 300)
        self.assertTrue(args.guarded)
        self.assertFalse(boot.parse_args(['ww', 'fhs']).guarded)
        self.assertTrue(boot.parse_args(['ww', 'fhs', '--expect-uninitialized-refusal'])
                        .expect_uninitialized_refusal)
        for profile in ('development', 'release'):
            current = boot.parse_args(options + ['--current-build-receipt', 'BUILD-RECEIPT.json',
                                                '--build-profile', profile])
            self.assertEqual(current.build_profile, profile)
        for invalid in (['ww', 'fhs', '--guarded'], options[:-2],
                        options + ['--expect-uninitialized-refusal'],
                        options + ['--current-build-receipt', 'BUILD-RECEIPT.json'],
                        options + ['--build-profile', 'development'],
                        [part for part in options if part != '--guarded'],
                        [part if part != '300' else '0' for part in options],
                        [part if part != '300' else '-1' for part in options]):
            with self.subTest(invalid=invalid), redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    boot.parse_args(invalid)

    def test_missing_initialization_control_requires_the_exact_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            home = root / 'home'
            (home / '.ww').mkdir(parents=True)
            (home / '.ww/identity').write_bytes(b'identity')
            lease = home / '.ww-host-instance.lock'
            for code, diagnostic, accepted in [
                    (1, f'Error: open startup lease {lease}\nNo such file or directory (os error 2)', True),
                    (0, '', False), (1, 'different startup failure', False)]:
                with self.subTest(code=code, diagnostic=diagnostic):
                    def launch(command, cwd, env, output, error, timeout, limit):
                        output.write_bytes(b'')
                        error.write_text(diagnostic)
                        return SimpleNamespace(wait=lambda: code)
                    with patch.object(boot, 'BoundedProcess', side_effect=launch), \
                            redirect_stdout(io.StringIO()):
                        if accepted:
                            boot.verify_uninitialized_refusal(
                                ['ww', 'run'], root, {}, home, b'identity', root)
                        else:
                            with self.assertRaises(AssertionError):
                                boot.verify_uninitialized_refusal(
                                    ['ww', 'run'], root, {}, home, b'identity', root)

    def test_admission_preserves_final_run_arguments_and_context(self):
        self.assertTrue(hasattr(boot, 'admit_guarded'), 'explicit admission is missing')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = boot.GuardedEvidence(root / 'evidence')
            env = {'HOME': str(root / 'home'), 'WW_COMPILE_WORKERS': '2',
                   'WW_CWASM_DIR': str(root / 'cwasm'), 'XDG_RUNTIME_DIR': '/run/user/1001'}
            command = ['/owned/.ww/bin/ww', 'run', '--identity', '/owned/.ww/identity',
                       '--http-listen', '127.0.0.1:43821', '--executor-threads', '1',
                       '/ipfs/bafysource:/boot/kernel.wasm']
            calls = []

            def execute(argv, cwd, env, stdout, stderr, timeout, limit):
                calls.append((argv, {'cwd': cwd, 'env': env, 'timeout': timeout}))
                stdout.write_bytes(b'{"status":"completed","activation":false,"result":{"kind":"admitted"}}\n')
                return SimpleNamespace(wait=lambda: 0, returncode=0, bytes={})

            with patch.object(boot, 'BoundedProcess', side_effect=execute):
                boot.admit_guarded(command, root, env, 300, evidence, 'immutable-target')
            argv, kwargs = calls[0]
            self.assertEqual(argv, ['/owned/.ww/bin/ww', 'startup', 'admit',
                                    '--timeout-secs', '300', '--json', 'run',
                                    '--identity', '/owned/.ww/identity', '--http-listen',
                                    '127.0.0.1:43821', '--executor-threads', '1',
                                    '/ipfs/bafysource:/boot/kernel.wasm'])
            self.assertIs(kwargs['env'], env)
            self.assertEqual(kwargs['cwd'], root)
            self.assertGreater(kwargs['timeout'], 300)
            report = json.loads((root / 'evidence/report.json').read_text())
            self.assertEqual(report['commands'][0]['stdin'], 'DEVNULL')
            self.assertEqual(report['modes']['immutable-target']['admission']['status'], 'completed')

    def test_guarded_environment_drops_injection_and_keeps_real_user_bus(self):
        self.assertTrue(hasattr(boot, 'guarded_environment'), 'guarded sanitization is missing')
        supplied = {'HOME': '/home/fixture', 'PATH': '/usr/bin',
                    'XDG_RUNTIME_DIR': '/run/user/1001',
                    'DBUS_SESSION_BUS_ADDRESS': 'unix:path=/run/user/1001/bus',
                    'LD_PRELOAD': '/inject.so', 'DYLD_INSERT_LIBRARIES': '/inject.dylib',
                    'HTTP_PROXY': 'http://proxy', 'https_proxy': 'http://proxy',
                    'TEST_INJECT': '1', 'WW_TEST_PID0_RESULT_RACE': '1'}
        actual = boot.guarded_environment(supplied)
        self.assertEqual(actual, {'HOME': '/home/fixture', 'PATH': '/usr/bin',
                                 'XDG_RUNTIME_DIR': '/run/user/1001',
                                 'DBUS_SESSION_BUS_ADDRESS': 'unix:path=/run/user/1001/bus',
                                 'WW_COMPILE_WORKERS': '2'})

    def test_package_provenance_is_opt_in_and_default_stays_synthetic(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tree = root / 'release'
            tree.mkdir()
            provenance = {'repository': 'https://github.com/wetware/ww',
                          'revision': 'd' * 40,
                          'builder': 'github-actions:42648050888:1:aarch64-unknown-linux-gnu'}
            result = subprocess.CompletedProcess([], 0, stdout='{"root":"bafyfixture"}')
            with patch.object(auth.subprocess, 'run', return_value=result), \
                    patch.object(auth, 'sign_manifest'):
                auth.package_release('tool', tree, root / 'spool', root / 'key',
                                     root / 'trust', ['aarch64-unknown-linux-gnu'])
                default = json.loads((root / 'metadata.json').read_text())
                self.assertEqual(default['source']['revision'], 'a' * 40)
                self.assertEqual(default['source']['builder'], 'installer-test-fixture')
                auth.package_release('tool', tree, root / 'spool', root / 'key',
                                     root / 'trust', ['aarch64-unknown-linux-gnu'],
                                     provenance=provenance)
            metadata = json.loads((root / 'metadata.json').read_text())
            self.assertEqual(metadata['source'], provenance)
            self.assertEqual(metadata['hosts'], {'aarch64-unknown-linux-gnu': provenance})
            self.assertEqual((tree / 'REVISION').read_text(), 'd' * 40 + '\n')

    def test_bootstrap_copies_readonly_inputs_and_serves_complete_metadata(self):
        self.assertTrue(hasattr(auth, 'packaged_bootstrap'), 'complete bootstrap is missing')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary, tool = root / 'original-ww', root / 'original-package'
            binary.write_bytes(b'original Host bytes')
            tool.write_bytes(b'original package tool bytes')
            binary.chmod(0o400)
            tool.chmod(0o400)
            payload = root / 'fhs'
            (payload / 'boot').mkdir(parents=True)
            (payload / 'bin').mkdir()
            (payload / 'boot/kernel.wasm').write_bytes(b'kernel')
            (payload / 'bin/status.wasm').write_bytes(b'status')
            provenance = {'repository': 'https://github.com/wetware/ww',
                          'revision': 'd' * 40,
                          'builder': 'github-actions:42648050888:1:aarch64-unknown-linux-gnu'}
            sidecar = root / 'PROVENANCE.json'
            sidecar.write_text(json.dumps(provenance))
            observed = {}

            def package(package_tool, tree, spool, private, trusted_keys, targets, **kwargs):
                observed.update(provenance=kwargs['provenance'], targets=targets,
                                private=private, trusted_keys=trusted_keys, tree=tree)
                self.assertEqual(package_tool.read_bytes(), tool.read_bytes())
                self.assertTrue(os.access(package_tool, os.X_OK))
                self.assertEqual((tree / 'bin/ww/linux/aarch64/ww').read_bytes(), binary.read_bytes())
                self.assertTrue(private.is_file())
                self.assertNotIn(tree, trusted_keys.parents)
                for name, raw in [('CHECKSUMS.txt', b'# sha256\nfixture'),
                                  ('CHECKSUMS.txt.sig', b's' * 64),
                                  ('RELEASE-DESCRIPTOR', b'bafydescriptor\n'),
                                  ('RELEASE-DESCRIPTOR.cbor', b'descriptor')]:
                    (tree / name).write_bytes(raw)
                (spool / 'blocks').mkdir(parents=True)
                (spool / 'blocks/bafyroot').write_bytes(b'root')
                return {'root': 'bafyroot'}

            with patch.object(auth, 'package_release', side_effect=package):
                with auth.packaged_bootstrap(root / 'fixture', binary, payload, tool, sidecar) as fixture:
                    self.assertFalse(observed['private'].exists())
                    self.assertTrue(observed['trusted_keys'].exists())
                    self.assertEqual(observed['provenance'], provenance)
                    self.assertEqual(observed['targets'], ['aarch64-unknown-linux-gnu'])
                    self.assertEqual(fixture['arguments'], [
                        '--payload', str(fixture['payload']),
                        '--manifest', str(observed['tree'] / 'CHECKSUMS.txt'),
                        '--signature', str(observed['tree'] / 'CHECKSUMS.txt.sig'),
                        '--trusted-release-keys', str(observed['trusted_keys']),
                        '--bootstrap-release', 'bafyroot'])
                    expected = {
                        ('/api/v0/cat', '/ipfs/bafyroot/CHECKSUMS.txt'): b'# sha256\nfixture',
                        ('/api/v0/cat', '/ipfs/bafyroot/CHECKSUMS.txt.sig'): b's' * 64,
                        ('/api/v0/cat', '/ipfs/bafyroot/RELEASE-DESCRIPTOR'): b'bafydescriptor\n',
                        ('/api/v0/block/get', 'bafydescriptor'): b'descriptor',
                        ('/api/v0/block/get', 'bafyroot'): b'root'}
                    self.assertEqual(fixture['metadata'].routes, expected)
                    request = urllib.request.Request(
                        fixture['metadata'].endpoint + '/api/v0/block/get?arg=bafyroot', data=b'')
                    with urllib.request.urlopen(request, timeout=2) as response:
                        self.assertEqual(response.read(), b'root')
                self.assertFalse(fixture['metadata'].thread.is_alive())
            self.assertEqual(binary.stat().st_mode & 0o777, 0o400)
            self.assertEqual(tool.stat().st_mode & 0o777, 0o400)

    def test_failure_evidence_survives_temporary_state_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = boot.GuardedEvidence(Path(directory) / 'evidence')
            with tempfile.TemporaryDirectory() as state:
                env = {'HOME': state, 'WW_COMPILE_WORKERS': '2', 'SECRET': 'do-not-publish'}
                command = ['/owned/ww', 'run', '--executor-threads', '1']

                def fail(argv, cwd, env, stdout, stderr, timeout, limit):
                    stderr.write_bytes(b'actual admission failure')
                    return SimpleNamespace(wait=lambda: 1, returncode=1, bytes={})

                with patch.object(boot, 'BoundedProcess', side_effect=fail):
                    with self.assertRaises(subprocess.CalledProcessError), evidence:
                        boot.admit_guarded(command, Path(state), env, 120, evidence, 'bare')
            report_text = (evidence.path / 'report.json').read_text()
            report = json.loads(report_text)
            self.assertEqual(report['status'], 'failed')
            self.assertEqual(report['commands'][0]['status'], 'failed')
            self.assertEqual(report['commands'][0]['returncode'], 1)
            self.assertNotIn('do-not-publish', report_text)
            self.assertEqual((evidence.path / 'admit-bare.stderr').read_bytes(), b'actual admission failure')

    def test_guarded_install_checks_selected_artifact_and_retained_proof(self):
        self.assertTrue(hasattr(boot, 'verify_guarded_install'), 'guarded installation check is missing')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home, tree = root / 'home', root / 'release'
            state = home / '.ww/bin/.ww-host-upgrade'
            (state / 'proofs').mkdir(parents=True)
            for name, raw in [('boot/kernel.wasm', b'kernel'), ('bin/status.wasm', b'status')]:
                for destination in (tree / 'fhs' / name, home / '.ww/fhs' / name):
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(raw)
            binary = tree / 'ww'
            binary.write_bytes(b'Host bytes')
            (home / '.ww/bin/ww').write_bytes(binary.read_bytes())
            proof = state / 'proofs/bafyroot'
            proof.write_bytes(b'authenticated proof')
            trust = root / 'trusted-keys'
            trust.write_bytes(b'independent public key')
            startup = {'trusted_release_keys': str(trust.resolve()),
                       'disposition': {'kind': 'required-unbound'},
                       'native': {'transport_root': 'bafyroot', 'descriptor': 'bafydescriptor',
                                  'artifact': {'algorithm': 'sha256',
                                               'digest': hashlib.sha256(binary.read_bytes()).hexdigest(),
                                               'size': binary.stat().st_size},
                                  'proof': {'sha256': hashlib.sha256(proof.read_bytes()).hexdigest(),
                                            'size': proof.stat().st_size}}}
            statefile = state / 'startup-state.json'
            statefile.write_text(json.dumps(startup))
            (home / '.ww/installed-release.json').write_text('{"version":1,"release":"bafyroot"}')
            fixture = {'trusted_keys': trust, 'binary': binary, 'payload': tree / 'fhs',
                       'root': 'bafyroot', 'descriptor': 'bafydescriptor'}
            self.assertEqual(boot.verify_guarded_install(home, fixture), startup)
            proof.write_bytes(b'tampered proof')
            with self.assertRaises(AssertionError):
                boot.verify_guarded_install(home, fixture)
            proof.write_bytes(b'authenticated proof')
            startup['native']['artifact']['digest'] = '0' * 64
            statefile.write_text(json.dumps(startup))
            with self.assertRaises(AssertionError):
                boot.verify_guarded_install(home, fixture)

    def test_all_five_guarded_modes_admit_before_launch_with_identical_context(self):
        self.assertTrue(hasattr(boot, 'install_guarded'), 'guarded boot path is missing')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary, fhs = root / 'ww', root / 'fhs'
            binary.write_bytes(b'Host bytes')
            (fhs / 'boot').mkdir(parents=True)
            (fhs / 'boot/kernel.wasm').write_bytes(b'kernel')
            evidence = root / 'evidence'
            admission_calls, launches, stopped = [], [], []
            current = {}

            def install(args, fixture_root, home, env, receipts):
                (home / '.ww/bin').mkdir(parents=True)
                (home / '.ww/fhs/boot').mkdir(parents=True)
                (home / '.ww/bin/ww').write_bytes(binary.read_bytes())
                (home / '.ww/fhs/boot/kernel.wasm').write_bytes(b'kernel')
                (home / '.ww/identity').write_bytes(b'identity')
                return binary.resolve(), fhs.resolve()

            def admit(command, cwd, env, timeout, receipts, mode):
                self.assertEqual(len(stopped), len(launches))
                admission_calls.append((command[:], cwd, env.copy(), mode))
                current.update(command=command, cwd=cwd, env=env, mode=mode)
                receipts.report['modes'][mode] = {}

            def launch(command, cwd, env, stdout, stderr, timeout, limit):
                self.assertEqual(command, current['command'])
                self.assertEqual(cwd, current['cwd'])
                self.assertIs(env, current['env'])
                self.assertEqual(env['WW_COMPILE_WORKERS'], '2')
                self.assertEqual(command[command.index('--executor-threads') + 1], '1')
                launches.append(command[:])
                return SimpleNamespace(returncode=0, bytes={}, error=None, check=lambda: None,
                                       stop=lambda: stopped.append(command))

            def http(url, process, log):
                if url.endswith('/readyz'):
                    value = {'ready': True, 'phase': 'ready'}
                elif url.endswith('/status'):
                    value = {'status': 'ok', 'version': 'test', 'peer_id': 'stable-peer',
                             'listen_addrs': ['/ip4/127.0.0.1/tcp/123'], 'peer_count': 0}
                else:
                    value = {'kernel_effective_root': 'bafyeffective', 'kernel_source_cid': 'bafysource',
                             'kernel_epoch_seq': 1, 'kernel_source': '/ipfs/bafyeffective/boot/kernel.wasm',
                             'kernel_cid': 'override' if current['mode'] in ('local-target', 'immutable-target') else 'default'}
                return json.dumps(value).encode()

            def ipfs_output(command, **kwargs):
                if 'add' in command:
                    current['override'] = Path(command[-1]).read_bytes()
                    return 'bafysource\n'
                return current['override'] if current['mode'] in ('local-target', 'immutable-target') else b'kernel'

            @contextmanager
            def kubo():
                yield {'IPFS_API': 'http://127.0.0.1:14321', 'IPFS_ROUTING_API': 'http://127.0.0.1:14322'}

            options = [str(binary), str(fhs), '--guarded', '--package-tool', str(binary),
                       '--producer-provenance', str(binary), '--admission-timeout-secs', '300',
                       '--evidence', str(evidence)]
            with patch.object(boot, 'install_guarded', side_effect=install), \
                    patch.object(boot.tempfile, 'tempdir', '/tmp'), \
                    patch.object(boot, 'admit_guarded', side_effect=admit), \
                    patch.object(boot, 'isolated_kubo', kubo), \
                    patch.object(boot.shutil, 'which', return_value='/bin/ipfs'), \
                    patch.object(boot, 'BoundedProcess', side_effect=launch), \
                    patch.object(boot.subprocess, 'check_output', side_effect=ipfs_output), \
                    patch.object(boot.subprocess, 'run', return_value=SimpleNamespace(returncode=1)), \
                    patch.object(boot, 'wait_http', side_effect=http), \
                    redirect_stdout(io.StringIO()) as output:
                boot.main(options)
            self.assertEqual([call[3] for call in admission_calls],
                             ['configured', 'bare', 'local-target', 'immutable-target', 'explicit-root'])
            self.assertEqual(len(launches), 5)
            self.assertEqual(output.getvalue().count('PASS: installed '), 5)
            self.assertNotIn('--namespace-root', launches[1])
            self.assertTrue(launches[2][-1].endswith('override.wasm:/boot/kernel.wasm'))
            self.assertEqual(launches[3][-1], '/ipfs/bafysource:/boot/kernel.wasm')
            self.assertEqual(launches[4][-1], str(fhs.resolve()))
            report = json.loads((evidence / 'report.json').read_text())
            self.assertEqual(report['status'], 'completed')
            self.assertEqual(len(report['modes']), 5)
            for mode in report['modes'].values():
                self.assertEqual(mode['status'], 'completed')
                self.assertTrue(mode['ready']['ready'])

    def test_guarded_fixture_refuses_socket_path_overflow_before_install(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            long_root = root / ('long-fixture-path-' * 8)
            long_root.mkdir()
            binary, fhs = root / 'ww', root / 'fhs'
            binary.write_bytes(b'Host fixture')
            fhs.mkdir()
            evidence = root / 'evidence'
            options = [str(binary), str(fhs), '--guarded', '--package-tool', str(binary),
                       '--producer-provenance', str(binary), '--admission-timeout-secs', '120',
                       '--evidence', str(evidence)]
            with patch.object(boot.tempfile, 'tempdir', str(long_root)), \
                    patch.object(boot.shutil, 'which', return_value='/bin/ipfs'), \
                    patch.object(boot, 'install_guarded') as install:
                with self.assertRaisesRegex(AssertionError, 'socket path budget'):
                    boot.main(options)
                install.assert_not_called()
            self.assertEqual(json.loads((evidence / 'report.json').read_text())['status'], 'failed')

    def test_admission_recovery_is_not_authorization_for_a_new_launch(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = boot.GuardedEvidence(Path(directory) / 'evidence')
            response = '{"status":"completed","activation":false,"result":{"kind":"recovered"}}'
            with patch.object(evidence, 'run', return_value=response):
                with self.assertRaises(AssertionError), evidence:
                    boot.admit_guarded(['/owned/ww', 'run'], Path(directory), {}, 120, evidence, 'bare')
            report = json.loads((evidence.path / 'report.json').read_text())
            self.assertEqual(report['status'], 'failed')
            self.assertEqual(report['modes']['bare']['admission']['result']['kind'], 'recovered')

    def test_default_invocation_retains_unmanaged_bootstrap_and_propagates_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            binary, fhs = root / 'ww', root / 'fhs'
            binary.write_bytes(b'Host bytes')
            fhs.mkdir()
            installed_commands = []

            def install(command, **kwargs):
                installed_commands.append(command)
                home = Path(kwargs['env']['HOME'])
                self.assertNotIn('WW_COMPILE_WORKERS', kwargs['env'])
                (home / '.ww/bin').mkdir(parents=True)
                (home / '.ww/bin/ww').write_bytes(binary.read_bytes())
                (home / '.ww/identity').write_bytes(b'identity')
                (home / '.ww/fhs/boot').mkdir(parents=True)
                (home / '.ww/fhs/boot/kernel.wasm').write_bytes(b'kernel')
                return SimpleNamespace(returncode=0)

            def launch(command, **kwargs):
                self.assertNotIn('stdin', kwargs)
                return SimpleNamespace(returncode=1)

            @contextmanager
            def kubo():
                yield {'IPFS_API': 'http://127.0.0.1:14321', 'IPFS_ROUTING_API': 'http://127.0.0.1:14322'}

            with patch.object(boot, 'bootstrap_arguments', return_value=['--payload', str(fhs)]), \
                    patch.object(boot, 'install_guarded', side_effect=AssertionError('unexpected guarded install')), \
                    patch.object(boot, 'admit_guarded', side_effect=AssertionError('unexpected admission')), \
                    patch.object(boot, 'isolated_kubo', kubo), \
                    patch.object(boot.shutil, 'which', return_value='/bin/ipfs'), \
                    patch.object(boot.subprocess, 'run', side_effect=install), \
                    patch.object(boot.subprocess, 'Popen', side_effect=launch), \
                    patch.object(boot.subprocess, 'check_output', return_value='bafysource'), \
                    patch.object(boot, 'wait_http', side_effect=RuntimeError('missing startup lease')), \
                    patch.object(boot, 'stop'):
                with self.assertRaisesRegex(RuntimeError, 'missing startup lease'):
                    boot.main([str(binary), str(fhs)])
            self.assertEqual(installed_commands,
                             [[str(binary), 'install', '--payload', str(fhs)]])

    def test_existing_evidence_and_symlinks_are_not_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = root / 'evidence'
            evidence.mkdir(mode=0o700)
            receipt = evidence / 'previous-receipt'
            receipt.write_text('retained')
            with self.assertRaises(AssertionError):
                boot.GuardedEvidence(evidence)
            alias = root / 'alias'
            alias.symlink_to(evidence)
            with self.assertRaises(AssertionError):
                boot.GuardedEvidence(alias)
            self.assertEqual(receipt.read_text(), 'retained')


if __name__ == '__main__':
    unittest.main()
