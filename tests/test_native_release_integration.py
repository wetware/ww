#!/usr/bin/env python3
"""Pure contract tests; these never start a Host, Kubo or service manager."""
import copy
from contextlib import ExitStack
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import native_release_integration as lane


CHECKS = [
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
COMMANDS = '''initial-service install-a daemon-register daemon-reload fault-stop-policy
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


class NativeContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.output = Path(self.tmp.name)
        self.marker = {'a': {'sha256': 'a' * 64, 'archive_sha256': 'c' * 64,
                             'revision': '1' * 40},
                       'b': {'sha256': 'b' * 64, 'archive_sha256': 'd' * 64,
                             'revision': '2' * 40}}
        self.report = {'version': 1, 'platform': 'linux-aarch64', 'status': 'PARTIAL',
                       'checks': list(CHECKS), 'executed_checks': 12,
                       'producer_receipts': copy.deepcopy(self.marker),
                       'trust_sha256': 'e' * 64, 'production_activation': False,
                       'remaining': ['wrong executable, PID reuse and same-PID exec refusal'],
                       'cleanup': 'fixture service stopped and removed; installation evidence retained'}
        self.commands = []
        readiness_commands = [stage + suffix
                              for stage in ('a', 'b', 'restored-a', 'readiness-timeout-restored-a',
                                            'failed-start-restored-a')
                              for suffix in ('-process', '-process-after')]
        for index, name in enumerate(COMMANDS + readiness_commands):
            code = {'missing-upgrade-budget': 1, 'zero-upgrade-budget': 2,
                    'readiness-timeout': 1, 'failed-start': 1}.get(name, 0)
            receipt = {'name': name, 'argv': ['/fixture/ww', name], 'exit': code,
                       'elapsed_seconds': 0.1,
                       'stdout': f'{index:04d}-{name}.stdout',
                       'stderr': f'{index:04d}-{name}.stderr'}
            self.commands.append(receipt)
            for stream in ('stdout', 'stderr'):
                (self.output / receipt[stream]).write_text('')
        self.write_json('original-producer-verification.json', {
            key: {'sha256': value['sha256'], 'archive_sha256': value['archive_sha256'],
                  'embedded_revision_bytes': value['revision']}
            for key, value in self.marker.items()})
        for name, producer in [('a', 'a'), ('b', 'b'), ('restored-a', 'a'),
                               ('readiness-timeout-restored-a', 'a'),
                               ('failed-start-restored-a', 'a')]:
            self.write_json(name + '-readiness.json', {
                'pid': 123, 'invocation': 'f' * 32, 'process_start': ['boot', 1],
                'response': {'serving_ready': True,
                             'artifact': {'digest': self.marker[producer]['sha256']}}})
        for fault in ('readiness-timeout', 'failed-start'):
            watcher = self.output / (fault + '-watcher')
            watcher.mkdir()
            self.write_json(fault + '-watcher/result.json', {'injected': True})
            self.write_json(fault + '-state.json', {'state': {'disposition': {'kind': 'blocked'}},
                'journal': {'phase': 'containing', 'disposition': {'stop_intent': True}}})

    def write_json(self, name, value):
        (self.output / name).write_text(json.dumps(value))

    def validate(self, code=2):
        self.write_json('report.json', self.report)
        self.write_json('commands.json', self.commands)
        return lane.validate_native_result(self.output, code, self.marker, 'e' * 64)

    def test_complete_partial_is_accepted_without_rewriting_native_status(self):
        self.assertEqual(self.validate(), 12)
        self.assertEqual(json.loads((self.output / 'report.json').read_text())['status'], 'PARTIAL')

    def test_generic_exit_two_or_failed_report_is_refused(self):
        for status in ('RUNNING', 'FAILED', 'PASS'):
            with self.subTest(status=status):
                self.report['status'] = status
                with self.assertRaisesRegex(RuntimeError, 'PARTIAL'):
                    self.validate()

    def test_unexpected_exit_is_refused_even_with_complete_report(self):
        for code in (0, 1, -9, None):
            with self.subTest(code=code), self.assertRaisesRegex(RuntimeError, 'exit 2'):
                self.validate(code)

    def test_missing_or_reordered_check_is_refused(self):
        for checks in (CHECKS[:-1], list(reversed(CHECKS)), CHECKS + [CHECKS[0]]):
            with self.subTest(checks=checks), self.assertRaisesRegex(RuntimeError, 'checks'):
                self.report['checks'] = checks
                self.validate()

    def test_error_and_cleanup_error_are_refused_even_when_empty(self):
        for field in ('error', 'cleanup_error'):
            with self.subTest(field=field):
                self.report[field] = ''
                with self.assertRaisesRegex(RuntimeError, 'error'):
                    self.validate()
                del self.report[field]

    def test_missing_cleanup_is_refused(self):
        self.report['cleanup'] = 'fixture cleanup failed; inspect retained service and command evidence'
        with self.assertRaisesRegex(RuntimeError, 'cleanup'):
            self.validate()

    def test_missing_recovery_or_stopped_command_is_refused(self):
        original = self.commands
        for name in ('upgrade-a-b', 'rollback-b-a', 'exact-noop', 'readiness-timeout-recover',
                     'failed-start-recover', 'stopped-upgrade', 'stopped-rollback', 'cleanup-reload'):
            with self.subTest(name=name), self.assertRaisesRegex(RuntimeError, 'command ledger'):
                self.commands = [item for item in original if item['name'] != name]
                self.validate()

    def test_wrong_command_exit_is_refused(self):
        next(item for item in self.commands if item['name'] == 'failed-start-recover')['exit'] = 1
        with self.assertRaisesRegex(RuntimeError, 'command exit'):
            self.validate()

    def test_missing_command_output_is_refused(self):
        (self.output / self.commands[0]['stderr']).unlink()
        with self.assertRaises((RuntimeError, FileNotFoundError)):
            self.validate()

    def test_changed_producer_or_public_trust_is_refused(self):
        self.report['producer_receipts']['b']['sha256'] = '9' * 64
        with self.assertRaisesRegex(RuntimeError, 'producer'):
            self.validate()
        self.report['producer_receipts'] = copy.deepcopy(self.marker)
        self.report['trust_sha256'] = '9' * 64
        with self.assertRaisesRegex(RuntimeError, 'trust'):
            self.validate()

    def test_missing_readiness_or_injection_evidence_is_refused(self):
        self.write_json('failed-start-watcher/result.json', {'injected': False})
        with self.assertRaisesRegex(RuntimeError, 'injection'):
            self.validate()

    def test_unknown_extra_command_is_refused(self):
        extra = dict(self.commands[0], name='wrong-executable')
        self.commands.insert(1, extra)
        with self.assertRaisesRegex(RuntimeError, 'unexpected native command'):
            self.validate()

    def test_nonexistent_or_symlinked_output_receipt_is_refused(self):
        path = self.output / self.commands[0]['stdout']
        path.unlink()
        path.symlink_to(self.output / self.commands[1]['stdout'])
        with self.assertRaisesRegex(RuntimeError, 'symlink'):
            self.validate()


class SuiteContract(unittest.TestCase):
    def test_unittest_success_requires_every_case_ok_and_exact_count(self):
        output = ''.join(f'test_{n} (__main__.HostInstall.test_{n}) ... ok\n' for n in range(21))
        output += '\nRan 21 tests in 1.2s\n\nOK\n'
        self.assertEqual(lane.validate_unittest_output(output, 21), 21)
        for broken in (output.replace('... ok', '... skipped reason', 1),
                       output.replace('Ran 21', 'Ran 20'), output.replace('\nOK\n', '\nFAILED\n')):
            with self.subTest(output=broken), self.assertRaises(RuntimeError):
                lane.validate_unittest_output(broken, 21)

    def test_boot_requires_each_original_mode_once(self):
        output = '\n'.join('PASS: installed ' + mode + ', /readyz, /status, root=bfixture'
                           for mode in ('configured', 'bare', 'local-target', 'immutable-target', 'explicit-root'))
        self.assertEqual(lane.validate_boot_output(output), 5)
        with self.assertRaises(RuntimeError):
            lane.validate_boot_output(output.replace('explicit-root', 'configured'))

    def test_boot_receipt_requires_admission_readiness_and_private_cleanup(self):
        modes = ('configured', 'bare', 'local-target', 'immutable-target', 'explicit-root')
        record = {'status': 'completed', 'modes': {name: {
                    'status': 'completed', 'private_paths_absent': True, 'service_files': [],
                    'host_returncode': 0,
                    'admission': {'status': 'completed', 'activation': False,
                                  'result': {'kind': 'admitted'}},
                    'ready': {'ready': True, 'phase': 'ready'}, 'http_status': {'status': 'ok'}}
                  for name in modes}, 'commands': [{'name': name, 'status': 'completed', 'returncode': 0}
                  for name in ('install', *(f'admit-{mode}' for mode in modes))]}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'report.json'
            path.write_text(json.dumps(record))
            self.assertEqual(lane.validate_boot_report(Path(directory)), 5)
            for field, value in (('status', 'running'), ('private_paths_absent', False),
                                 ('host_returncode', None)):
                broken = copy.deepcopy(record)
                broken['modes']['explicit-root'][field] = value
                path.write_text(json.dumps(broken))
                with self.subTest(field=field), self.assertRaises(RuntimeError):
                    lane.validate_boot_report(Path(directory))


class TemporaryRootBoundary(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.home, self.private = self.root / 'h', self.root / 't'
        self.home.mkdir(mode=0o700)
        self.private.mkdir(mode=0o700)
        self.inputs, self.packages = self.root / 'inputs', self.root / 'packages'
        self.inputs.mkdir()
        self.packages.mkdir()
        self.tool = self.root / 'package-tool'
        self.tool.write_bytes(b'never executed')
        self.args = SimpleNamespace(inputs=str(self.inputs), packages=str(self.packages),
                    package_tool=str(self.tool), output=str(self.home / 'evidence'),
                    temporary_root=str(self.private), timeout_secs=120)
        self.actual_uid = os.geteuid()
        self.account = SimpleNamespace(pw_name='ww739-fixture', pw_uid=1001, pw_dir=str(self.home))

    def validate(self, *, temporary_bytes=b'/tmp/w12345678/t', foreign_temp_owner=False):
        original_custody, original_fsencode = lane.native.custody, os.fsencode
        root_inputs = {self.inputs, self.packages, self.tool}

        def fixture_custody(path, owner, directory=False):
            self.assertEqual(owner, 0 if path in root_inputs else 1001)
            # Adapt only simulated account ownership. All file type, link and
            # permission checks still run against the real local filesystem.
            local_owner = self.actual_uid + (path == self.private and foreign_temp_owner)
            return original_custody(path, local_owner, directory)

        def fixture_encoding(path):
            # macOS temp paths are longer than the native /tmp fixture. Keep
            # the byte-budget boundary independently controllable in this test.
            return temporary_bytes if Path(path) == self.private else original_fsencode(path)

        with ExitStack() as context:
            context.enter_context(patch.object(lane.platform, 'system', return_value='Linux'))
            context.enter_context(patch.object(lane.platform, 'machine', return_value='aarch64'))
            context.enter_context(patch.object(lane.os, 'geteuid', return_value=1001))
            context.enter_context(patch.object(lane.os, 'getuid', return_value=1001))
            context.enter_context(patch.object(lane.pwd, 'getpwuid', return_value=self.account))
            context.enter_context(patch.object(lane.Path, 'cwd', return_value=self.home))
            context.enter_context(patch.dict(lane.os.environ, HOME=str(self.home)))
            context.enter_context(patch.object(lane.native, 'custody', side_effect=fixture_custody))
            context.enter_context(patch.object(lane.os, 'fsencode', side_effect=fixture_encoding))
            return lane.validate_arguments(self.args)

    def test_exact_owned_empty_temporary_root_is_accepted(self):
        home, paths = self.validate()
        self.assertEqual(home, self.home)
        self.assertEqual(paths['temporary_root'], self.private)
        self.assertTrue((self.home / 'evidence').is_dir())
        self.assertEqual(list(self.private.iterdir()), [])

    def test_arbitrary_sibling_or_home_cannot_be_used_as_temporary_root(self):
        other = self.root / 'other'
        other.mkdir()
        for path in (other, self.home):
            with self.subTest(path=path), self.assertRaisesRegex(RuntimeError, 'explicitly provisioned'):
                self.args.temporary_root = str(path)
                self.validate()
        self.assertFalse((self.home / 'evidence').exists())

    def test_nonempty_temporary_root_is_refused_without_removing_existing_files(self):
        sentinel = self.private / 'existing'
        sentinel.write_bytes(b'preserve me')
        with self.assertRaisesRegex(RuntimeError, 'must be empty'):
            self.validate()
        self.assertEqual(sentinel.read_bytes(), b'preserve me')
        self.assertFalse((self.home / 'evidence').exists())

    def test_symlinked_temporary_root_is_refused(self):
        target = self.root / 'other'
        target.mkdir()
        self.private.rmdir()
        self.private.symlink_to(target, target_is_directory=True)
        with self.assertRaisesRegex(RuntimeError, 'explicitly provisioned'):
            self.validate()
        self.assertTrue(self.private.is_symlink())
        self.assertTrue(target.is_dir())

    def test_foreign_owned_temporary_root_is_refused(self):
        with self.assertRaisesRegex(RuntimeError, 'another owner'):
            self.validate(foreign_temp_owner=True)

    def test_other_user_writable_temporary_root_is_refused(self):
        self.private.chmod(0o770)
        with self.assertRaisesRegex(RuntimeError, 'other-user writes'):
            self.validate()

    def test_temporary_root_exceeding_socket_path_budget_is_refused(self):
        with self.assertRaisesRegex(RuntimeError, 'socket path budget'):
            self.validate(temporary_bytes=b'/tmp/w123456789/t')
        self.assertFalse((self.home / 'evidence').exists())


class TemporaryRootCleanup(unittest.TestCase):
    def test_contents_are_removed_without_removing_directory_or_following_links(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            private, outside = root / 't', root / 'outside'
            private.mkdir(mode=0o700)
            outside.mkdir()
            sentinel = outside / 'preserve'
            sentinel.write_bytes(b'external contents')
            (private / 'nested').mkdir()
            (private / 'nested/file').write_bytes(b'owned fixture')
            (private / 'link').symlink_to(outside, target_is_directory=True)
            (private / 'dangling').symlink_to(root / 'missing')
            identity = private.lstat().st_ino
            root.chmod(0o555)
            try:
                lane.clear_temporary_root(private)
                self.assertTrue(private.is_dir())
                self.assertEqual(private.lstat().st_ino, identity)
                self.assertEqual(list(private.iterdir()), [])
                self.assertEqual(sentinel.read_bytes(), b'external contents')
            finally:
                root.chmod(0o700)

    def test_cleanup_refuses_a_symlinked_root_without_touching_target(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            target = root / 'target'
            target.mkdir(mode=0o700)
            sentinel = target / 'preserve'
            sentinel.write_bytes(b'outside fixture')
            private = root / 't'
            private.symlink_to(target, target_is_directory=True)
            with self.assertRaisesRegex(RuntimeError, 'symlink'):
                lane.clear_temporary_root(private)
            self.assertTrue(private.is_symlink())
            self.assertEqual(sentinel.read_bytes(), b'outside fixture')

    def test_cleanup_refuses_changed_permissions_without_deleting_contents(self):
        with tempfile.TemporaryDirectory() as directory:
            private = Path(directory) / 't'
            private.mkdir(mode=0o700)
            sentinel = private / 'preserve'
            sentinel.write_bytes(b'changed custody')
            private.chmod(0o770)
            with self.assertRaisesRegex(RuntimeError, 'other-user writes'):
                lane.clear_temporary_root(private)
            self.assertEqual(sentinel.read_bytes(), b'changed custody')


if __name__ == '__main__':
    unittest.main()
