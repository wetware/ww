#!/usr/bin/env python3
"""Offline executable installer contracts. Requires WW_TEST_BINARY and WW_TEST_FHS."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from release_auth_fixture import bootstrap_arguments


class HostInstall(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name) / "home"
        self.home.mkdir()
        self.binary = Path(os.environ["WW_TEST_BINARY"]).resolve()
        self.payload = Path(os.environ["WW_TEST_FHS"]).resolve()
        self.destination = self.home / ".ww/bin/ww"
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("WW_", "IPFS_"))}
        self.env.update(HOME=str(self.home), IPFS_API="http://127.0.0.1:1",
                        IPFS_ROUTING_API="http://127.0.0.1:1",
                        WW_CWASM_DIR=str(Path(self.tmp.name) / "cwasm"))

    def install(self, *flags, binary=None):
        selected = binary or self.binary
        bootstrap = bootstrap_arguments(Path(self.tmp.name) / 'bootstrap', selected, self.payload)
        return subprocess.run([str(selected), "install", *bootstrap, *flags], env=self.env,
                              capture_output=True, text=True, timeout=120)

    def snapshot(self):
        return {str(path.relative_to(self.home)): path.read_bytes()
                for path in self.home.rglob('*') if path.is_file() and not path.is_symlink()}

    def assert_installed(self):
        self.assertTrue((self.home / ".ww/identity").is_file())
        self.assertTrue(self.destination.is_file())
        self.assertFalse(self.destination.is_symlink())
        self.assertEqual(hashlib.sha256(self.destination.read_bytes()).digest(),
                         hashlib.sha256(self.binary.read_bytes()).digest())
        for name in ("boot/kernel.wasm", "bin/status.wasm"):
            self.assertEqual((self.home / ".ww/fhs" / name).read_bytes(),
                             (self.payload / name).read_bytes())
        result = subprocess.run([str(self.destination), "--version"], env=self.env,
                                capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_fresh_preflight_and_repeat_install_are_offline_and_do_not_register(self):
        self.destination.parent.mkdir(parents=True)
        check = self.install("--check", "--json")
        self.assertEqual(check.returncode, 0, check.stderr)
        report = json.loads(check.stdout)
        self.assertEqual(report['version'], 1)
        self.assertEqual(report['operation'], 'install')
        self.assertTrue(report['check'])
        self.assertFalse(report['activation'])
        self.assertFalse((self.home / ".ww/identity").exists())
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()
        self.assertFalse((self.home / '.ww/etc/ns/ww').exists())
        self.assertEqual(list(self.home.rglob('*.plist')), [])
        self.assertEqual(list(self.home.rglob('*.service')), [])
        for name in ('ipns-watermark', 'signed-ipns-record', 'namespace-credential'):
            (self.home / '.ww' / name).write_bytes(b'private authority state')
        before = self.snapshot()
        result = self.install('--json')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)['changed_paths'], [])
        self.assertEqual(self.snapshot(), before)

    def test_same_path_executable_stays_regular(self):
        self.destination.parent.mkdir(parents=True)
        shutil.copy2(self.binary, self.destination)
        result = self.install(binary=self.destination)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()

    def test_native_bootstrap_rejects_invalid_signature_without_mutation(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        before = self.snapshot()
        bootstrap = bootstrap_arguments(Path(self.tmp.name) / 'bootstrap', self.binary, self.payload)
        signature = Path(bootstrap[bootstrap.index('--signature') + 1])
        signature.write_bytes(b'0' * 64)
        result = subprocess.run([str(self.binary), 'install', *bootstrap], env=self.env,
                                capture_output=True, text=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.snapshot(), before)

    def test_unsigned_payload_handoff_is_a_usage_error(self):
        result = subprocess.run([str(self.binary), 'install', '--payload', str(self.payload)],
                                env=self.env, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(list(self.home.iterdir()), [])

    def test_exact_recorded_install_needs_no_new_release_selection(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        before = self.snapshot()
        result = subprocess.run([str(self.destination), 'install', '--json'], env=self.env,
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)['changed_paths'], [])
        self.assertEqual(self.snapshot(), before)

    def test_malformed_identity_is_not_replaced(self):
        identity = self.home / '.ww/identity'
        identity.parent.mkdir()
        identity.write_bytes(b'malformed identity')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('identity', result.stderr)
        self.assertEqual(identity.read_bytes(), b'malformed identity')
        self.assertFalse(self.destination.exists())

    def test_unknown_payload_collision_refuses_before_executable(self):
        kernel = self.home / '.ww/fhs/boot/kernel.wasm'
        kernel.parent.mkdir(parents=True)
        kernel.write_bytes(b'custom kernel')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(kernel.read_bytes(), b'custom kernel')
        self.assertFalse(self.destination.exists())
        self.assertFalse((self.home / '.ww/identity').exists())

    def test_valid_existing_identity_is_preserved(self):
        identity = self.home / '.ww/identity'
        identity.parent.mkdir()
        generated = subprocess.run([str(self.binary), 'keygen', '--output', str(identity)],
                                   env=self.env, capture_output=True, timeout=15)
        self.assertEqual(generated.returncode, 0, generated.stderr)
        original = identity.read_bytes()
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(identity.read_bytes(), original)

    def test_unknown_binary_is_not_replaced(self):
        self.destination.parent.mkdir(parents=True)
        self.destination.write_bytes(b'custom executable')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.destination.read_bytes(), b'custom executable')
        self.assertFalse((self.home / '.ww/identity').exists())

    def test_symlinked_destination_directory_is_not_followed(self):
        outside = Path(self.tmp.name) / 'outside'
        outside.mkdir()
        (self.home / '.ww').symlink_to(outside, target_is_directory=True)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(outside.iterdir()), [])

    def test_dangling_identity_is_not_regenerated(self):
        identity = self.home / '.ww/identity'
        identity.parent.mkdir()
        identity.symlink_to('missing-private-key')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(identity.is_symlink())
        self.assertFalse((identity.parent / 'missing-private-key').exists())

    def test_custom_service_dropin_and_namespace_survive_fresh_and_repeat_install(self):
        private = {
            'Library/LaunchAgents/io.wetware.ww.plist': b'custom launchd identity/listen/image/routing',
            '.config/systemd/user/ww.service': b'custom systemd identity/listen/image/routing',
            '.config/systemd/user/ww.service.d/kernel.conf': b'Environment=WW_KERNEL=custom',
            '.ww/etc/ns/ww': b'custom signed namespace configuration',
        }
        for name, content in private.items():
            path = self.home / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        for _ in range(2):
            result = self.install()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assert_installed()
            for name, content in private.items():
                self.assertEqual((self.home / name).read_bytes(), content)

    def test_incomplete_managed_install_recovers_exact_recorded_payload(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = self.home / '.ww/installation.json'
        state = json.loads(metadata.read_text())
        state['complete'] = False
        metadata.write_text(json.dumps(state))
        (self.home / '.ww/fhs/bin/status.wasm').unlink()
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()
        self.assertTrue(json.loads(metadata.read_text())['complete'])

    def test_install_refuses_different_fhs_without_changing_state(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        before = self.snapshot()
        updated = Path(self.tmp.name) / 'updated-fhs'
        shutil.copytree(self.payload, updated)
        status = updated / 'bin/status.wasm'
        status.write_bytes(status.read_bytes() + b'\x00\x05\x04newx')
        self.payload = updated
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('ww image update', result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_install_does_not_repair_altered_native_bytes(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.destination.write_bytes(b'externally replaced Host')
        before = self.snapshot()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.snapshot(), before)

    def test_invalid_payload_does_not_replace_installed_files(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        before = self.snapshot()
        bad = Path(self.tmp.name) / 'bad-fhs'
        shutil.copytree(self.payload, bad)
        (bad / 'boot/kernel.wasm').write_bytes(b'corrupt wasm')
        self.payload = bad
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.snapshot(), before)

    def test_missing_owned_identity_refuses_regeneration(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        identity = self.home / '.ww/identity'
        identity.unlink()
        before = self.snapshot()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.snapshot(), before)
        self.assertFalse(identity.exists())

    def test_home_spaces_and_xml_characters_do_not_create_service_files(self):
        home = self.home.with_name('home space & chars')
        self.home.rename(home)
        self.home = home
        self.destination = home / '.ww/bin/ww'
        self.env['HOME'] = str(home)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()
        self.assertEqual(list(home.rglob('*.plist')), [])
        self.assertEqual(list(home.rglob('*.service')), [])

    def test_legacy_perform_operations_are_usage_errors_without_mutation(self):
        for action in ('install', 'update', 'upgrade', 'uninstall'):
            with self.subTest(action=action):
                result = subprocess.run([str(self.binary), 'perform', action], env=self.env,
                                        capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(list(self.home.iterdir()), [])


if __name__ == '__main__':
    unittest.main()
