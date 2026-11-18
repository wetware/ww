#!/usr/bin/env python3
"""Live qualification requires explicit inputs; CI preserves required native lanes."""
from pathlib import Path
import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


class Boundary(unittest.TestCase):
    def test_live_entrypoints_require_explicit_inputs(self):
        for name in ('scripts/qualify_native_upgrade.py',
                     'scripts/run_native_release_integration.py',
                     'scripts/run_native_development_boot.py',
                     'tests/native_release_integration.py'):
            with self.subTest(name=name):
                result = subprocess.run([sys.executable, str(ROOT / name)],
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn('required', result.stderr)

    def test_workflow_has_no_live_lane_or_branch_exception(self):
        workflow = (ROOT / '.github/workflows/rust.yml').read_text()
        self.assertNotIn('lthibault/pasted-text-v38', workflow)
        for name in ('run_native_release_integration.py', 'run_native_development_boot.py',
                     'prepare_current_native_release.py --', 'qualify_native_upgrade.py --'):
            self.assertNotIn(name, workflow)
        self.assertNotIn('Verify retained historical release inputs', workflow)
        self.assertNotIn('verify_native_release_inputs.py', workflow)
        self.assertNotIn('fetch_native_release_inputs.py', workflow)
        self.assertIn('python3 tests/test_native_release_inputs.py', workflow)
        self.assertIn('python3 scripts/check_repository_artifacts.py', workflow)
        self.assertIn('python3 tests/test_repository_artifacts.py', workflow)

    def test_historical_pair_command_is_removed(self):
        result = subprocess.run([sys.executable, str(ROOT / 'scripts/prepare_current_native_release.py'),
                                 'verify'], capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid choice: 'verify'", result.stderr)
        for name in ('scripts/verify_native_release_inputs.py', 'scripts/fetch_native_release_inputs.py',
                     '.github/native-release-integration.json'):
            self.assertFalse((ROOT / name).exists(), name)
        self.assertFalse(any((ROOT / 'tests/fixtures/native-release-originals').rglob('*.*')))


    def test_unresolved_or_noncanonical_baseline_pin_refuses(self):
        import json
        import tempfile
        sys.path.insert(0, str(ROOT / 'scripts'))
        from prepare_current_native_release import baseline_pin
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.github').mkdir()
            path = root / '.github/native-upgrade-baseline.json'
            for revision, accepted in [('UNRESOLVED_REDUCED_BASELINE', False), ('A' * 40, False),
                                        ('a' * 39, False), ('a' * 40, True)]:
                path.write_text(json.dumps({'schema': 1, 'repository': 'wetware/ww',
                                           'revision': revision, 'contract': 'managed-systemd-v3'}))
                if accepted:
                    self.assertEqual(baseline_pin(root)['revision'], revision)
                else:
                    with self.assertRaisesRegex(RuntimeError, 'unresolved or invalid'):
                        baseline_pin(root)


class InstallerFixtureBoundary(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        # Import definitions only: never run native setup or a fixture binary.
        source = ROOT / 'tests/test_install.py'
        spec = importlib.util.spec_from_file_location('installer_fixture_offline', source)
        self.module = importlib.util.module_from_spec(spec)
        with patch.object(sys, 'argv', [str(source), '--binary', str(source),
                '--fhs', str(self.root), '--package-tool', str(source)]):
            spec.loader.exec_module(self.module)
        self.case = self.module.Installation('test_corrupt_and_empty_payloads_refused')
        self.case.home = self.root / 'home'
        self.case.home.mkdir()

    def test_only_exact_new_empty_custodied_locks_are_allowed(self):
        existing = self.case.home / 'operator-state'
        existing.write_bytes(b'preserve')
        before = self.case.snapshot()
        for name in ('.ww-installation.lock', '.ww-fhs.lock'):
            path = self.case.home / name
            path.touch(mode=0o600)
            self.case.assert_native_refusal_preserved(before)
        before = self.case.snapshot()
        self.case.assert_native_refusal_preserved(before)
        existing.write_bytes(b'changed')
        with self.assertRaises(AssertionError):
            self.case.assert_native_refusal_preserved(before)

    def test_new_lock_custody_and_existing_state_changes_are_refused(self):
        for name in ('.ww-installation.lock', '.ww-fhs.lock'):
            for invalid in ('mode', 'contents', 'symlink', 'dangling', 'hardlink', 'directory', 'owner'):
                with self.subTest(lock=name, invalid=invalid):
                    path = self.case.home / name
                    target = self.root / 'outside'
                    target.write_bytes(b'')
                    if invalid in ('symlink', 'dangling'):
                        path.symlink_to(target if invalid == 'symlink' else self.root / 'absent')
                    elif invalid == 'hardlink':
                        os.link(target, path)
                    elif invalid == 'directory':
                        path.mkdir(mode=0o600)
                    else:
                        path.touch(mode=0o600)
                        if invalid == 'mode':
                            path.chmod(0o644)
                        if invalid == 'contents':
                            path.write_bytes(b'not empty')
                    try:
                        with patch.object(self.module.os, 'geteuid', return_value=os.geteuid() + (invalid == 'owner')):
                            with self.assertRaises(AssertionError):
                                self.case.assert_native_refusal_preserved({})
                    finally:
                        path.rmdir() if path.is_dir() and not path.is_symlink() else path.unlink()
                        target.unlink()
            path.write_bytes(b'existing lock bytes')
            path.chmod(0o600)
            before = self.case.snapshot()
            self.case.assert_native_refusal_preserved(before)
            path.write_bytes(b'')
            with self.assertRaises(AssertionError):
                self.case.assert_native_refusal_preserved(before)
            path.unlink()
        (self.case.home / '.ww-fhs.lock.extra').write_bytes(b'')
        with self.assertRaises(AssertionError):
            self.case.assert_native_refusal_preserved({})

    def test_corrupt_payload_subtests_restore_bytes_and_signing_state_after_failure(self):
        case = self.case
        case.root, case.release = self.root, self.root / 'release'
        originals = {name: ('valid ' + name).encode() for name in self.module.PAYLOADS[1:]}
        for name, raw in originals.items():
            path = case.release / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(raw)
        signed = case.release / 'CHECKSUMS.txt.sig'
        signed.write_bytes(b'valid signature')
        observations, restored = [], []
        def checksums(valid_payload=True):
            if valid_payload:
                self.assertEqual({name: (case.release / name).read_bytes() for name in originals}, originals)
                signed.write_bytes(b'valid signature')
                restored.append(True)
            else:
                changed = [(name, (case.release / name).read_bytes()) for name in originals
                           if (case.release / name).read_bytes() != originals[name]]
                self.assertEqual(len(changed), 1)
                self.assertEqual(signed.read_bytes(), b'valid signature')
                observations.extend(changed)
                signed.write_bytes(b'corrupt fixture signature')
        def install(success):
            name, content = observations[-1]
            message = ('Mandatory payload is empty: ' if not content else
                       'descriptor and signed manifest disagree: ') + name
            return subprocess.CompletedProcess([], 1, '', message)
        case.setUp = case.tearDown = lambda: None
        case.checksums, case.install = checksums, install
        case.restore_checksums = checksums
        case.assert_native_refusal_preserved = lambda before: case.fail('injected refusal assertion')
        result = unittest.TestResult()
        with patch.object(self.module.subprocess, 'run', side_effect=AssertionError('native execution forbidden')):
            case.run(result)
        self.assertEqual(len(result.failures), 4)
        self.assertEqual(result.errors, [])
        self.assertEqual(observations, [(name, content) for name in originals for content in (b'', b'corrupt component')])
        self.assertEqual(len(restored), 4)
        self.assertEqual(signed.read_bytes(), b'valid signature')
        self.assertEqual({name: (case.release / name).read_bytes() for name in originals}, originals)


if __name__ == '__main__':
    unittest.main()
