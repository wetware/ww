#!/usr/bin/env python3
"""Exercise real shell authentication with ephemeral keys and isolated transport."""
import base64
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from release_auth_fixture import generate_key, sign_manifest

REPO = Path(__file__).resolve().parents[1]
INSTALLER = REPO / 'scripts/install.sh'
SIGNER = REPO / 'scripts/sign_release.sh'
PLATFORMS = [('Linux', 'x86_64', 'linux/x86_64'),
             ('Linux', 'aarch64', 'linux/aarch64'),
             ('Darwin', 'x86_64', 'macos/x86_64'),
             ('Darwin', 'arm64', 'macos/aarch64')]


class ReleaseAuthentication(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='ww-release-auth-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.private, self.keys = generate_key(self.root / 'keys')
        self.other_private, self.other_keys = generate_key(self.root / 'other-keys')
        self.release = self.root / 'release'
        self.release.mkdir()
        self.manifest = self.release / 'CHECKSUMS.txt'
        self.signature = self.release / 'CHECKSUMS.txt.sig'
        self.home = self.root / 'home'
        self.home.mkdir()
        (self.home / 'sentinel').write_text('existing installation state')
        self.calls = self.root / 'executions'
        self.transport = self.root / 'transport-calls'
        self.stubs = self.root / 'bin'
        self.stubs.mkdir()
        self.script('uname', '''case "$1" in
-s) echo "$TEST_OS" ;;
-m) echo "$TEST_ARCH" ;;
*) exit 90 ;;
esac
''')
        self.script('ipfs', '''printf '%s\n' "$*" >> "$TEST_TRANSPORT"
case "$1" in
id) exit 0 ;;
cat)
  case "$2" in /ipfs/test-release/*) ;; *) exit 91 ;; esac
  path=${2#/ipfs/test-release/}
  if [ "${FAIL_FETCH:-}" = "$path" ]; then printf truncated; exit 1; fi
  cat "$TEST_RELEASE/$path" ;;
*) exit 92 ;;
esac
''')
        binary = b'''#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$TEST_EXECUTIONS"
case "$*" in
*--check*) exit 0 ;;
'perform install '*) mkdir -p "$HOME/.ww/bin"; cp "$0" "$HOME/.ww/bin/ww" ;;
esac
'''
        for _, _, path in PLATFORMS:
            target = self.release / 'bin/ww' / path / 'ww'
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(binary)
        for name in ('fhs/boot/kernel.wasm', 'fhs/bin/status.wasm'):
            target = self.release / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(b'fixture WASM')
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(('WW_', 'IPFS_', 'GITHUB_'))}
        self.env.update(HOME=str(self.home), PATH=str(self.stubs) + os.pathsep + os.environ['PATH'],
                        TEST_OS='Linux', TEST_ARCH='x86_64', TEST_RELEASE=str(self.release),
                        TEST_EXECUTIONS=str(self.calls), TEST_TRANSPORT=str(self.transport))
        self.write_manifest()

    def script(self, name, body):
        path = self.stubs / name
        path.write_text('#!/bin/sh\nset -eu\n' + body)
        path.chmod(0o755)

    def write_manifest(self):
        records = []
        for path in sorted(self.release.rglob('*')):
            if path.is_file() and path.name not in ('CHECKSUMS.txt', 'CHECKSUMS.txt.sig'):
                records.append(hashlib.sha256(path.read_bytes()).hexdigest() + '  '
                               + path.relative_to(self.release).as_posix() + '\n')
        self.manifest.write_text('# sha256\n' + ''.join(records))
        self.sign()

    def sign(self, private=None):
        sign_manifest(private or self.private, self.manifest)

    def run_installer(self, verify=False, success=False, keys=None, legacy=False):
        installer = REPO / 'tests/fixtures/release-auth/legacy-install.sh' if legacy else INSTALLER
        args = ['sh', str(installer)]
        if not legacy:
            args += ['--trusted-release-keys', str(keys or self.keys)]
        if verify:
            args += ['--verify-manifest', str(self.manifest), '--signature', str(self.signature)]
        else:
            args += ['--version', 'test-release']
        before = {str(path.relative_to(self.home)): path.read_bytes()
                  for path in self.home.rglob('*') if path.is_file()}
        result = subprocess.run(args, env=self.env, capture_output=True, text=True, timeout=30)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse(self.calls.exists(), 'unauthenticated candidate executed')
            after = {str(path.relative_to(self.home)): path.read_bytes()
                     for path in self.home.rglob('*') if path.is_file()}
            self.assertEqual(after, before)
        if verify:
            self.assertFalse(self.transport.exists(), 'verify-only mode contacted IPFS')
        return result

    def test_valid_release_all_platforms(self):
        for os_name, arch, _ in PLATFORMS:
            with self.subTest(os=os_name, arch=arch):
                self.env.update(TEST_OS=os_name, TEST_ARCH=arch)
                self.run_installer(success=True)
                self.assertEqual(len(self.calls.read_text().splitlines()), 3)
                self.calls.unlink()

    def test_legacy_consumer_accepts_signed_tree(self):
        # Frozen pre-authentication installer ignores the new detached signature.
        # This proves producer-first compatibility, not legacy consumer security.
        self.run_installer(success=True, legacy=True)
        self.assertEqual(len(self.calls.read_text().splitlines()), 3)

    def test_small_order_keys_and_signature_points_fail(self):
        ys = [
            '00' * 32,
            '01' + '00' * 31,
            '26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05',
            'c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a',
            'ec' + 'ff' * 30 + '7f',
            'ed' + 'ff' * 30 + '7f',
            'ee' + 'ff' * 30 + '7f',
        ]
        original_key = self.keys.read_bytes()
        original_signature = self.signature.read_bytes()
        # R=identity, S=0 is a universal forgery for an identity public key
        # under OpenSSL alone. The installer must reject that provisioned key.
        forgery = bytes.fromhex('01' + '00' * 63)
        for y in ys:
            for sign_bit in (0, 128):
                point = bytes.fromhex(y)
                point = point[:-1] + bytes([point[-1] | sign_bit])
                with self.subTest(point=point.hex()):
                    self.keys.write_bytes(base64.b64encode(
                        bytes.fromhex('302a300506032b6570032100') + point) + b'\n')
                    # A valid R isolates rejection of the keyring, including a
                    # weak second key after an otherwise successful signature.
                    self.signature.write_bytes(original_signature)
                    result = self.run_installer(verify=True)
                    self.assertIn('Weak Ed25519 release point', result.stderr)
                    self.keys.write_bytes(original_key + self.keys.read_bytes())
                    result = self.run_installer(verify=True)
                    self.assertIn('Weak Ed25519 release point', result.stderr)
                    self.keys.write_bytes(original_key)
                    self.signature.write_bytes(point + original_signature[32:])
                    result = self.run_installer(verify=True)
                    self.assertIn('Weak Ed25519 release point', result.stderr)
        self.keys.write_bytes(base64.b64encode(
            bytes.fromhex('302a300506032b6570032100') + bytes.fromhex('01' + '00' * 31)) + b'\n')
        self.signature.write_bytes(forgery)
        self.run_installer(verify=True)

    def test_verify_only_authenticates_without_transport(self):
        self.run_installer(verify=True, success=True)
        self.assertFalse(self.calls.exists())

    def test_manifest_modification(self):
        self.manifest.write_bytes(self.manifest.read_bytes().replace(b'fhs', b'FHS'))
        self.run_installer()

    def test_modified_artifact(self):
        (self.release / 'bin/ww/linux/x86_64/ww').write_bytes(b'evil')
        self.run_installer()

    def test_attacker_generated_checksums(self):
        (self.release / 'bin/ww/linux/x86_64/ww').write_bytes(b'evil')
        self.write_manifest()
        self.sign(self.other_private)
        self.run_installer()

    def test_wrong_trusted_key(self):
        self.run_installer(keys=self.other_keys)

    def test_fetched_public_key_is_not_a_trust_anchor(self):
        (self.release / 'release-keys').write_bytes(self.other_keys.read_bytes())
        self.sign(self.other_private)
        self.run_installer()
        calls = self.transport.read_text() if self.transport.exists() else ''
        self.assertNotIn('/release-keys', calls)

    def test_missing_manifest_and_signature(self):
        for path in (self.manifest, self.signature):
            with self.subTest(path=path.name):
                data = path.read_bytes()
                path.unlink()
                self.run_installer()
                path.write_bytes(data)

    def test_malformed_signature(self):
        original = self.signature.read_bytes()
        for data in (b'', original[:-1], original + b'\n', b'X' * 64,
                     base64.b64encode(original), b'\0' * 64):
            with self.subTest(length=len(data)):
                self.signature.write_bytes(data)
                self.run_installer()
        self.signature.write_bytes(original)

    def test_missing_platform_entry_and_payload(self):
        original = self.manifest.read_bytes()
        self.manifest.write_bytes(b''.join(line for line in original.splitlines(keepends=True)
                                          if b'bin/ww/linux/x86_64/ww' not in line))
        self.sign()
        self.run_installer()
        self.manifest.write_bytes(original)
        self.sign()
        (self.release / 'bin/ww/linux/x86_64/ww').unlink()
        self.run_installer()

    def test_signed_digest_mismatch_and_malformed_manifests(self):
        original = self.manifest.read_bytes()
        record = original.splitlines(keepends=True)[1]
        digest = b'0' * 64
        cases = [original.replace(b'# sha256', b'# blake3'),
                 original.replace(b'# sha256', b'# SHA256'),
                 original[9:], original + record, original[:-1],
                 original.replace(b'\n', b'\r\n'), b'# sha256\n',
                 b'# sha256\n' + digest + b'  ../escape\n',
                 b'# sha256\n' + digest + b'  /absolute\n',
                 b'# sha256\n' + digest + b'  fhs//name\n',
                 b'# sha256\n' + digest + b'  fhs/./name\n',
                 b'# sha256\n' + digest + b'  fhs/../name\n',
                 b'# sha256\n' + digest + b'  fhs/name with space\n',
                 b'# sha256\n' + digest + b'  fhs/name\x00\n',
                 b'# sha256\n' + b'A' * 64 + b'  fhs/name\n',
                 b'# sha256\n' + digest + b' *fhs/name\n',
                 b'# sha256\n' + b'x' * (1024 * 1024)]
        for index, data in enumerate(cases):
            with self.subTest(case=index):
                self.manifest.write_bytes(data)
                self.sign()
                self.run_installer(verify=True)

    def test_rotation_overlap_and_retirement(self):
        overlap = self.root / 'overlap'
        overlap.write_bytes(self.keys.read_bytes() + self.other_keys.read_bytes())
        self.run_installer(verify=True, success=True, keys=overlap)
        self.sign(self.other_private)
        self.run_installer(verify=True, success=True, keys=overlap)
        self.run_installer(verify=True, success=True, keys=self.other_keys)
        self.run_installer(verify=True, keys=self.keys)
        self.sign()
        self.run_installer(verify=True, keys=self.other_keys)

    def test_malformed_keyrings(self):
        original = self.keys.read_bytes()
        wrong_der = base64.b64encode(b'X' * 44) + b'\n'
        for data in (b'', original[:-1], original + original,
                     original + self.other_keys.read_bytes() + original,
                     b'not base64\n', wrong_der, original + b'\n',
                     original.replace(b'\n', b'\r\n'), b' ' + original):
            with self.subTest(keyring=data):
                self.keys.write_bytes(data)
                self.run_installer(verify=True)
        self.keys.unlink()
        self.run_installer(verify=True)

    def test_failed_transport_never_executes(self):
        for path in ('CHECKSUMS.txt', 'CHECKSUMS.txt.sig', 'fhs/bin/status.wasm'):
            with self.subTest(path=path):
                self.env['FAIL_FETCH'] = path
                self.run_installer()

    def run_signer(self, success=False):
        output = self.root / 'output.sig'
        output.unlink(missing_ok=True)
        result = subprocess.run(['bash', str(SIGNER), str(self.manifest), str(output),
                                 str(self.private), str(self.keys)], env=self.env,
                                capture_output=True, text=True, timeout=30)
        private_body = self.private.read_text().splitlines()[1]
        self.assertNotIn(private_body, result.stdout + result.stderr)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(output.read_bytes(), self.signature.read_bytes())
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse(output.exists())

    def test_signer_exact_bytes(self):
        self.run_signer(success=True)

    def test_signer_rejects_wrong_key(self):
        self.keys.write_bytes(self.other_keys.read_bytes())
        self.run_signer()

    def test_signer_private_key_cannot_enter_release_tree(self):
        private = self.release / 'private.pem'
        private.write_bytes(self.private.read_bytes())
        private.chmod(0o600)
        self.private = private
        self.run_signer()

    def test_signer_rejects_malformed_manifest(self):
        self.manifest.write_bytes(self.manifest.read_bytes().replace(b'# sha256', b'# blake3'))
        self.run_signer()

    def test_signer_ci_admission(self):
        self.env.update(GITHUB_ACTIONS='true', GITHUB_REPOSITORY='wetware/ww',
                        GITHUB_REF='refs/heads/master', GITHUB_EVENT_NAME='push')
        self.run_signer(success=True)
        self.env['GITHUB_EVENT_NAME'] = 'workflow_dispatch'
        self.run_signer(success=True)
        for name, value in [('GITHUB_EVENT_NAME', 'pull_request'),
                            ('GITHUB_EVENT_NAME', 'pull_request_target'),
                            ('GITHUB_EVENT_NAME', 'workflow_run'),
                            ('GITHUB_REF', 'refs/pull/412/merge'),
                            ('GITHUB_REF', 'refs/heads/staging'),
                            ('GITHUB_REPOSITORY', 'attacker/ww')]:
            with self.subTest(name=name, value=value):
                saved = self.env[name]
                self.env[name] = value
                self.run_signer()
                self.env[name] = saved


if __name__ == '__main__':
    unittest.main(verbosity=2)
