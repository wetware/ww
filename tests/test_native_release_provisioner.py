#!/usr/bin/env python3
"""Filesystem and mocked cleanup controls; never provision an account or service."""
import hashlib
import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import run_native_release_integration as provision
from qualify_native_upgrade import custody


class ExclusiveMarker(unittest.TestCase):
    def test_new_marker_retains_its_file_identity_and_written_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / '1001.json'
            value = {'uid': 1001, 'purpose': 'disposable-native-upgrade'}
            device, inode, digest = provision.create_marker(path, value)
            info = path.lstat()
            self.assertEqual((device, inode), (info.st_dev, info.st_ino))
            self.assertEqual(json.loads(path.read_text()), value)
            self.assertEqual(digest, hashlib.sha256(path.read_bytes()).hexdigest())

    def test_existing_regular_marker_is_refused_without_changing_bytes_or_inode(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / '1001.json'
            path.write_bytes(b'previous marker\n')
            before = path.lstat()
            with self.assertRaises(FileExistsError):
                provision.create_marker(path, {'uid': 1001})
            self.assertEqual(path.read_bytes(), b'previous marker\n')
            self.assertEqual((path.lstat().st_dev, path.lstat().st_ino),
                             (before.st_dev, before.st_ino))

    def test_dangling_marker_symlink_is_refused_without_touching_link_or_target(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / '1001.json'
            target = Path(directory) / 'absent'
            path.symlink_to(target)
            before = path.lstat()
            with self.assertRaises(FileExistsError):
                provision.create_marker(path, {'uid': 1001})
            self.assertTrue(path.is_symlink())
            self.assertEqual(path.readlink(), target)
            self.assertEqual(path.lstat().st_ino, before.st_ino)
            self.assertFalse(target.exists())


class CopiedFhs(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        expected = {'fhs/boot/kernel.wasm': b'KERNEL_PAYLOAD',
                    'fhs/bin/status.wasm': b'STATUS_PAYLOAD'}
        self.record = {'producer_evidence': {}}
        for label in ('a', 'b'):
            receipts = {}
            for name, value in expected.items():
                path = self.root / label / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(value)
                receipts[name] = {'size': len(value), 'sha256': hashlib.sha256(value).hexdigest()}
            self.record['producer_evidence'][label] = {'fhs': receipts}

    def verify(self):
        def local_owner(path, owner, directory=False):
            # Exercise real type/mode/hardlink custody while these local fixtures
            # belong to the test caller rather than requiring root privileges.
            self.assertEqual(owner, 0)
            return custody(path, os.geteuid(), directory)

        with patch.object(provision, 'custody', side_effect=local_owner):
            provision.verify_fhs(self.root, self.record)

    def test_exact_copied_fhs_is_accepted(self):
        self.verify()

    def test_same_size_tampering_is_refused_by_digest(self):
        (self.root / 'b/fhs/boot/kernel.wasm').write_bytes(b'TAMPER_PAYLOAD')
        with self.assertRaisesRegex(RuntimeError, 'copied original FHS differs'):
            self.verify()

    def test_missing_or_extra_copied_member_is_refused(self):
        kernel = self.root / 'b/fhs/boot/kernel.wasm'
        kernel.unlink()
        with self.assertRaisesRegex(RuntimeError, 'member set differs'):
            self.verify()
        kernel.write_bytes(b'KERNEL_PAYLOAD')
        (self.root / 'a/fhs/unexpected').write_bytes(b'extra')
        with self.assertRaisesRegex(RuntimeError, 'member set differs'):
            self.verify()

    def test_receipt_cannot_authorize_an_extra_fhs_member(self):
        self.record['producer_evidence']['a']['fhs']['fhs/extra'] = {'size': 0, 'sha256': '0' * 64}
        with self.assertRaisesRegex(RuntimeError, 'original FHS member set differs'):
            self.verify()

    def test_symlinked_member_is_refused_even_when_its_bytes_match(self):
        target = self.root / 'external-kernel'
        target.write_bytes(b'KERNEL_PAYLOAD')
        kernel = self.root / 'a/fhs/boot/kernel.wasm'
        kernel.unlink()
        kernel.symlink_to(target)
        # Linux symlink mode 0777 can fail custody before the type check.
        with self.assertRaises(RuntimeError) as refused:
            self.verify()
        self.assertIn(str(refused.exception), (
            f'path has another owner or permits other-user writes: {kernel}',
            f'path has the wrong type or is a symlink: {kernel}',
        ))
        self.assertTrue(kernel.is_symlink())
        self.assertEqual(kernel.readlink(), target)
        self.assertEqual(target.read_bytes(), b'KERNEL_PAYLOAD')


class EvidenceCopy(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.source = self.root / 'source'
        self.source.mkdir()
        self.destination = self.root / 'retained'

    def test_receipt_hashes_destination_even_if_source_changes_after_copy(self):
        source_file = self.source / 'nested/report.json'
        source_file.parent.mkdir()
        source_file.write_bytes(b'original evidence')

        def change_source_then_hash(path):
            source_file.write_bytes(b'later source bytes')
            return hashlib.sha256(path.read_bytes()).hexdigest()

        with patch.object(provision, 'sha256', side_effect=change_source_then_hash):
            receipts = provision.copy_evidence(self.source, self.destination)
        self.assertEqual(receipts, {'nested/report.json': hashlib.sha256(b'original evidence').hexdigest()})
        self.assertEqual((self.destination / 'nested/report.json').read_bytes(), b'original evidence')
        self.assertEqual(source_file.read_bytes(), b'later source bytes')

    def test_child_symlinks_are_refused_before_destination_creation(self):
        target = self.root / 'outside'
        target.write_bytes(b'not evidence')
        alias = self.source / 'alias'
        for destination in (target, self.root / 'missing'):
            with self.subTest(destination=destination):
                alias.symlink_to(destination)
                try:
                    with self.assertRaisesRegex(RuntimeError, 'nonregular evidence'):
                        provision.copy_evidence(self.source, self.destination)
                    self.assertFalse(self.destination.exists())
                finally:
                    alias.unlink()
        self.assertEqual(target.read_bytes(), b'not evidence')

    def test_symlinked_evidence_root_is_refused(self):
        (self.source / 'private').write_bytes(b'outside the intended evidence directory')
        alias = self.root / 'alias'
        alias.symlink_to(self.source, target_is_directory=True)
        with self.assertRaises(RuntimeError):
            provision.copy_evidence(alias, self.destination)
        self.assertFalse(self.destination.exists())

    def test_total_size_limit_refuses_before_copy(self):
        (self.source / 'report').write_bytes(b'12345')
        with patch.object(provision, 'MAX_EVIDENCE', 4):
            with self.assertRaisesRegex(RuntimeError, 'evidence exceeds bounds'):
                provision.copy_evidence(self.source, self.destination)
        self.assertFalse(self.destination.exists())

    def test_file_count_limit_refuses_before_copy(self):
        for number in range(4097):
            (self.source / str(number)).touch()
        with self.assertRaisesRegex(RuntimeError, 'evidence exceeds bounds'):
            provision.copy_evidence(self.source, self.destination)
        self.assertFalse(self.destination.exists())

    def test_growth_after_size_scan_is_still_bounded(self):
        source_file = self.source / 'report'
        source_file.write_bytes(b'123')
        original_open = os.open

        def grow_then_open(path, flags, *args, **kwargs):
            source_file.write_bytes(b'12345')
            return original_open(path, flags, *args, **kwargs)

        with patch.object(provision, 'MAX_EVIDENCE', 4), \
                patch.object(provision.os, 'open', side_effect=grow_then_open):
            with self.assertRaisesRegex(RuntimeError, 'opened evidence exceeds bound'):
                provision.copy_evidence(self.source, self.destination)
        self.assertLessEqual((self.destination / 'report').stat().st_size, 4)


class ProvisionerSubprocess(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.output = Path(self.temporary.name).resolve()
        self.pidfile = self.output / 'child.pid'
        self.worker = provision.Provisioner(self.output)
        self.addCleanup(self.cleanup_child)

    def cleanup_child(self):
        """Only signal a still-owned child if a regression left it unreaped."""
        if not self.pidfile.exists():
            return
        pid = int(self.pidfile.read_text())
        try:
            waited, _ = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return
        if waited == 0:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)

    def command(self, action):
        program = (
            'import os, sys, time\n'
            'from pathlib import Path\n'
            'Path(sys.argv[1]).write_text(str(os.getpid()))\n'
            'os.write(2, b"stderr evidence\\n")\n'
            'os.write(1, b"stdout evidence\\n")\n'
            + action
        )
        return [sys.executable, '-I', '-S', '-c', program, str(self.pidfile)]

    def retained_receipt(self, name, command):
        ledger = json.loads((self.output / 'provision-commands.json').read_text())
        self.assertEqual(len(ledger), 1)
        receipt = ledger[0]
        self.assertEqual(receipt['name'], name)
        self.assertEqual(receipt['argv'], command)
        self.assertEqual(receipt['exit'], -signal.SIGKILL)
        self.assertEqual(json.loads((self.output / 'commands.json').read_text()), ledger)
        self.assertEqual((self.output / receipt['stderr']).read_bytes(), b'stderr evidence\n')
        pid = int(self.pidfile.read_text())
        with self.assertRaises(ChildProcessError, msg='supervisor must reap the terminated child'):
            os.waitpid(pid, os.WNOHANG)
        return receipt

    def test_output_overflow_retains_bounded_evidence_and_reaps_child(self):
        command = self.command('while True:\n    os.write(1, b"x" * 65536)\n')
        with self.assertRaisesRegex(RuntimeError, 'command output exceeds bound: overflow'):
            self.worker.run('overflow', command, timeout=5)
        receipt = self.retained_receipt('overflow', command)
        output = self.output / receipt['stdout']
        self.assertGreater(output.stat().st_size, 4 * 1024 * 1024)
        # The existing reader retains at most the first overflowing read chunk.
        self.assertLessEqual(output.stat().st_size, 4 * 1024 * 1024 + 65536)
        with output.open('rb') as stream:
            self.assertEqual(stream.read(16), b'stdout evidence\n')

    def test_deadline_retains_both_streams_and_reaps_child(self):
        command = self.command('time.sleep(60)\n')
        started = time.monotonic()
        with self.assertRaisesRegex(RuntimeError, 'command deadline: deadline'):
            self.worker.run('deadline', command, timeout=2)
        self.assertLess(time.monotonic() - started, 8, 'timeout must include bounded cleanup')
        receipt = self.retained_receipt('deadline', command)
        self.assertEqual((self.output / receipt['stdout']).read_bytes(), b'stdout evidence\n')


class AccountCleanup(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.proc = Path(self.temporary.name)
        self.account = SimpleNamespace(pw_name='ww739-fixture', pw_uid=1001, pw_dir='/owned/home')
        # Replacing only the module's Path factory prevents any real /proc scan.
        self.addCleanup(patch.stopall)
        patch.object(provision, 'Path', side_effect=lambda value: self.proc if value == '/proc' else Path(value)).start()
        self.lookup = patch.object(provision.pwd, 'getpwnam', return_value=self.account).start()
        self.open_pidfd = patch.object(provision.os, 'pidfd_open', return_value=9001, create=True).start()
        self.close_pidfd = patch.object(provision.os, 'close').start()
        self.send_signal = patch.object(provision.signal, 'pidfd_send_signal', create=True).start()
        patch.object(provision.time, 'sleep').start()

    def process(self, pid, uids):
        directory = self.proc / str(pid)
        directory.mkdir(exist_ok=True)
        (directory / 'status').write_text('Name:\tfixture\nUid:\t' + '\t'.join(map(str, uids)) + '\n')
        return directory

    @staticmethod
    def remove_process(path):
        (path / 'status').unlink()
        path.rmdir()

    def test_only_fixture_owned_processes_are_signaled_through_pidfd(self):
        owned = self.process(411, [1001, 1001, 1001, 1001])
        foreign = self.process(422, [1002, 1002, 1002, 1002])
        (self.proc / 'self').mkdir()
        self.send_signal.side_effect = lambda *_: self.remove_process(owned)
        self.assertEqual(provision.quiesce_account(self.account), [411])
        self.open_pidfd.assert_called_once_with(411)
        self.send_signal.assert_called_once_with(9001, signal.SIGKILL)
        self.close_pidfd.assert_called_once_with(9001)
        self.assertTrue((foreign / 'status').is_file())

    def test_changed_account_identity_refuses_before_opening_process_handles(self):
        self.process(411, [1001] * 4)
        self.lookup.return_value = SimpleNamespace(pw_name='ww739-fixture', pw_uid=1002, pw_dir='/owned/home')
        with self.assertRaisesRegex(RuntimeError, 'account identity changed'):
            provision.quiesce_account(self.account)
        self.open_pidfd.assert_not_called()
        self.send_signal.assert_not_called()

    def test_uid_change_after_pidfd_open_refuses_signal_and_closes_handle(self):
        self.process(411, [1001] * 4)

        def change_owner(pid):
            self.process(pid, [1002] * 4)
            return 9001

        self.open_pidfd.side_effect = change_owner
        with self.assertRaisesRegex(RuntimeError, 'UID changed'):
            provision.quiesce_account(self.account)
        self.send_signal.assert_not_called()
        self.close_pidfd.assert_called_once_with(9001)

    def test_process_disappearing_before_pidfd_open_is_tolerated(self):
        owned = self.process(411, [1001] * 4)

        def disappear(_):
            self.remove_process(owned)
            raise ProcessLookupError('already exited')

        self.open_pidfd.side_effect = disappear
        self.assertEqual(provision.quiesce_account(self.account), [])
        self.send_signal.assert_not_called()
        self.close_pidfd.assert_not_called()

    def test_process_that_survives_signaling_fails_the_cleanup_deadline(self):
        self.process(411, [1001] * 4)
        with patch.object(provision.time, 'monotonic', side_effect=[10, 26]):
            with self.assertRaisesRegex(RuntimeError, 'still has processes'):
                provision.quiesce_account(self.account)
        self.send_signal.assert_called_once_with(9001, signal.SIGKILL)
        self.close_pidfd.assert_called_once_with(9001)


if __name__ == '__main__':
    unittest.main()
