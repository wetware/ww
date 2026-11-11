#!/usr/bin/env python3
"""Real Host installer tests. Only IPFS transport uses a local fixture."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import tempfile
import unittest
from release_auth_fixture import (bootstrap_arguments, cid_text, generate_key,
                                  manifest_root, package_release,
                                  ReleaseMetadataServer, sign_manifest)

REPO = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--fhs', type=Path, required=True)
parser.add_argument('--package-tool', type=Path, required=True)
args, remaining = parser.parse_known_args()
BINARY = args.binary.resolve(strict=True)
FHS = args.fhs.resolve(strict=True)
PACKAGE_TOOL = args.package_tool.resolve(strict=True)
HOST_PATH = 'bin/ww/{}/{}/ww'.format(
    'macos' if platform.system() == 'Darwin' else 'linux',
    'aarch64' if platform.machine() in ('arm64', 'aarch64') else 'x86_64')
PAYLOADS = [HOST_PATH, 'fhs/boot/kernel.wasm', 'fhs/bin/status.wasm']


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Installation(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tools = tempfile.TemporaryDirectory(prefix='ww-install-tools-')
        cls.addClassCleanup(cls.tools.cleanup)
        cls.package_tool = Path(cls.tools.name) / 'ww-release-package'
        # Artifact download does not retain executable mode. Change only a
        # private fixture copy, preserving the supplied artifact bytes.
        shutil.copyfile(PACKAGE_TOOL, cls.package_tool)
        cls.package_tool.chmod(0o700)

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='ww-install-test-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.home = self.root / 'home'
        self.home.mkdir()
        self.private, self.trusted_keys = generate_key(self.root / 'release-keys')
        self.release = self.root / 'release'
        (self.release / HOST_PATH).parent.mkdir(parents=True)
        shutil.copy2(BINARY, self.release / HOST_PATH)
        target = {'bin/ww/linux/aarch64/ww': 'aarch64-unknown-linux-gnu',
                  'bin/ww/linux/x86_64/ww': 'x86_64-unknown-linux-gnu',
                  'bin/ww/macos/aarch64/ww': 'aarch64-apple-darwin',
                  'bin/ww/macos/x86_64/ww': 'x86_64-apple-darwin'}[HOST_PATH]
        self.targets = [target]
        if target == 'x86_64-unknown-linux-gnu':
            alias = self.release / 'bin/ww/linux/musl-x86_64/ww'
            alias.parent.mkdir(parents=True)
            shutil.copy2(BINARY, alias)
            self.targets.append('x86_64-unknown-linux-musl')
        shutil.copytree(FHS, self.release / 'fhs')
        self.metadata = ReleaseMetadataServer()
        self.addCleanup(self.metadata.close)
        self.checksums()
        self.stubs = self.root / 'transport'
        self.stubs.mkdir()
        ipfs = self.stubs / 'ipfs'
        ipfs.write_text('''#!/bin/sh
set -eu
case "$1" in
  id) exit 0 ;;
  block)
    [ "$2" = get ] && [ "$3" = "$TEST_RELEASE_CID" ] || exit 91
    cat "$TEST_ROOT_BLOCK" ;;
  cat)
    prefix=/ipfs/$TEST_RELEASE_CID/
    case "$2" in "$prefix"*) ;; *) exit 91 ;; esac
    p=${2#"$prefix"}
    if [ "${FAIL_FETCH:-}" = "$p" ]; then printf truncated; exit 1; fi
    cat "$TEST_RELEASE/$p" ;;
  *) echo "unexpected IPFS operation: $*" >&2; exit 92 ;;
esac
''')
        ipfs.chmod(0o755)
        for command in ('systemctl', 'launchctl', 'curl'):
            stub = self.stubs / command
            stub.write_text('#!/bin/sh\necho "unexpected external command" >&2\nexit 93\n')
            stub.chmod(0o755)
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith(('WW_', 'IPFS_'))}
        self.env.update(HOME=str(self.home), TEST_RELEASE=str(self.release), TEST_RELEASE_CID=self.release_cid,
                        TEST_ROOT_BLOCK=str(self.root / 'root.block'),
                        PATH=str(self.stubs) + os.pathsep + os.environ['PATH'],
                        NO_COLOR='1', WW_CWASM_DIR=str(self.root / 'cwasm'),
                        IPFS_API=self.metadata.endpoint, IPFS_ROUTING_API='http://127.0.0.1:1')

    def checksums(self, valid_payload=True):
        if valid_payload:
            package = package_release(self.package_tool, self.release, self.root / 'spool',
                                      self.private, self.trusted_keys, self.targets)
            self.release_cid = package['root']
            (self.root / 'root.block').write_bytes(
                (self.root / 'spool/blocks' / self.release_cid).read_bytes())
            self.publish_metadata()
        else:
            # Preserve the valid descriptor while signing the corrupted bytes.
            # The shell must pass its hash checks; native metadata must refuse
            # the disagreement. Separate direct tests below reach FHS validation.
            manifest = self.release / 'CHECKSUMS.txt'
            records = [line.split('  ', 1)[1] for line in manifest.read_text().splitlines()[1:]]
            manifest.write_text('# sha256\n' + ''.join(
                f'{digest(self.release / name)}  {name}\n' for name in records))
            sign_manifest(self.private, manifest)
            self.bind_manifest()

    def bind_manifest(self, descriptor_link=True):
        descriptor = (self.release / 'RELEASE-DESCRIPTOR.cbor').read_bytes()
        block = manifest_root((self.release / 'CHECKSUMS.txt').read_bytes(),
                              descriptor if descriptor_link else None)
        (self.root / 'root.block').write_bytes(block)
        self.release_cid = cid_text(block)
        self.publish_metadata()

    def publish_metadata(self):
        self.descriptor_cid = (self.release / 'RELEASE-DESCRIPTOR').read_text().strip()
        self.metadata.routes = {
            ('/api/v0/cat', f'/ipfs/{self.release_cid}/{name}'):
                (self.release / name).read_bytes()
            for name in ('CHECKSUMS.txt', 'CHECKSUMS.txt.sig', 'RELEASE-DESCRIPTOR')}
        self.metadata.routes.update({
            ('/api/v0/block/get', self.descriptor_cid):
                (self.release / 'RELEASE-DESCRIPTOR.cbor').read_bytes(),
            ('/api/v0/block/get', self.release_cid): (self.root / 'root.block').read_bytes()})
        if hasattr(self, 'env'):
            self.env['TEST_RELEASE_CID'] = self.release_cid

    def install(self, success=True):
        before_requests = len(self.metadata.requests)
        result = subprocess.run(['sh', str(REPO / 'scripts/install.sh'),
                                 '--release', self.release_cid,
                                 '--trusted-release-keys', str(self.trusted_keys)], env=self.env,
                                text=True, capture_output=True, timeout=120)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            expected = [('/api/v0/cat', f'/ipfs/{self.release_cid}/{name}')
                        for name in ('CHECKSUMS.txt', 'CHECKSUMS.txt.sig', 'RELEASE-DESCRIPTOR')]
            expected += [('/api/v0/block/get', self.descriptor_cid),
                         ('/api/v0/block/get', self.release_cid)]
            # Both --check and actual installation authenticate independently.
            self.assertEqual(self.metadata.requests[before_requests:], expected * 2)
            startup = json.loads((self.home / '.ww/bin/.ww-host-upgrade/startup-state.json').read_text())
            self.assertEqual(startup['trusted_release_keys'], str(self.trusted_keys.resolve()))
            self.assertEqual(startup['disposition'], {'kind': 'required-unbound'})
            self.assertEqual(startup['native']['transport_root'], self.release_cid)
            self.assertEqual(startup['native']['descriptor'], self.descriptor_cid)
            proof = self.home / '.ww/bin/.ww-host-upgrade/proofs' / self.release_cid
            self.assertEqual(digest(proof), startup['native']['proof']['sha256'])
            self.assertEqual(proof.stat().st_size, startup['native']['proof']['size'])
            self.assertTrue((self.home / '.ww/identity').is_file())
            installed = self.home / '.ww/bin/ww'
            self.assertFalse(installed.is_symlink())
            self.assertEqual(digest(installed), digest(BINARY))
            for name in ('boot/kernel.wasm', 'bin/status.wasm'):
                self.assertEqual((self.home / '.ww/fhs' / name).read_bytes(),
                                 (self.release / 'fhs' / name).read_bytes())
            subprocess.run([str(installed), '--version'], env=self.env,
                           capture_output=True, check=True, timeout=15)
            locator = json.loads((self.home / '.ww/installed-release.json').read_text())
            self.assertEqual(locator['version'], 1)
            self.assertEqual(locator['release'], self.release_cid)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def snapshot(self):
        return {str(p.relative_to(self.home)): p.read_bytes()
                for p in self.home.rglob('*') if p.is_file() and not p.is_symlink()}

    def assert_native_refusal_preserved(self, before):
        # Native preflight creates the persistent coordination inode before it
        # inspects installation state. Account for only that exact new file;
        # retain the full snapshot assertion for every other HOME file.
        after = self.snapshot()
        lock_name = '.ww-installation.lock'
        expected = dict(before)
        if lock_name not in before and os.path.lexists(self.home / lock_name):
            metadata = (self.home / lock_name).lstat()
            self.assertTrue(stat.S_ISREG(metadata.st_mode))
            self.assertEqual(stat.S_IMODE(metadata.st_mode), 0o600)
            self.assertEqual(metadata.st_uid, os.geteuid())
            self.assertEqual(metadata.st_nlink, 1)
            expected[lock_name] = b''
        self.assertEqual(after, expected)

    def test_fresh_precreated_bin_and_rerun(self):
        (self.home / '.ww/bin').mkdir(parents=True)
        self.install()
        identity = (self.home / '.ww/identity').read_bytes()
        for name in ('ipns-watermark', 'signed-ipns-record', 'namespace-credential'):
            (self.home / '.ww' / name).write_text('private state')
        (self.home / '.ww/fhs/custom').write_text('custom content')
        before = self.snapshot()
        self.install()
        self.assertEqual((self.home / '.ww/identity').read_bytes(), identity)
        self.assertEqual(self.snapshot(), before)
        services = list(self.home.rglob('*.plist')) + list(self.home.rglob('*.service'))
        self.assertEqual(services, [])
        self.assertFalse((self.home / '.ww/etc/ns/ww').exists())

    def test_missing_payloads_and_checksums_do_not_replace(self):
        for name in PAYLOADS + ['CHECKSUMS.txt', 'CHECKSUMS.txt.sig']:
            with self.subTest(name=name):
                original = (self.release / name).read_bytes()
                mode = (self.release / name).stat().st_mode
                (self.release / name).unlink()
                before = self.snapshot()
                self.install(False)
                self.assertEqual(self.snapshot(), before)
                (self.release / name).write_bytes(original)
                (self.release / name).chmod(mode)

    def test_truncated_download_does_not_replace(self):
        self.install()
        before = self.snapshot()
        self.env['FAIL_FETCH'] = 'fhs/bin/status.wasm'
        self.install(False)
        self.assertEqual(self.snapshot(), before)

    def test_corrupt_and_empty_payloads_refused(self):
        for name in PAYLOADS[1:]:
            for content in (b'', b'corrupt component'):
                with self.subTest(name=name, content=content):
                    original = (self.release / name).read_bytes()
                    (self.release / name).write_bytes(content)
                    self.checksums(valid_payload=False)
                    before = self.snapshot()
                    result = self.install(False)
                    expected = ('Mandatory payload is empty: ' + name if not content else
                                'descriptor and signed manifest disagree: ' + name)
                    self.assertIn(expected, result.stderr)
                    self.assert_native_refusal_preserved(before)
                    bootstrap = bootstrap_arguments(self.root / 'corrupt-bootstrap', BINARY,
                                                    self.release / 'fhs')
                    result = subprocess.run([str(BINARY), 'install', *bootstrap], env=self.env,
                                            text=True, capture_output=True, timeout=120)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    expected = ('payload is empty' if not content else
                                'invalid packaged ' + name.removeprefix('fhs/'))
                    self.assertIn(expected, result.stderr)
                    self.assert_native_refusal_preserved(before)
                    (self.release / name).write_bytes(original)
                    self.checksums()

    def test_checksum_mismatch_missing_and_duplicate_entries_refused(self):
        original = (self.release / 'CHECKSUMS.txt').read_text()
        host_row = next(line for line in original.splitlines()
                        if line.endswith('  ' + HOST_PATH))
        cases = [(original.replace(digest(self.release / PAYLOADS[1]), '0' * 64),
                  'Checksum mismatch: fhs/boot/kernel.wasm'),
                 (''.join(line + '\n' for line in original.splitlines()
                          if not line.endswith('  fhs/bin/status.wasm')),
                  'Missing or duplicate SHA-256 entry: fhs/bin/status.wasm'),
                 (original + host_row + '\n', 'Malformed SHA-256 release manifest')]
        for content, expected in cases:
            (self.release / 'CHECKSUMS.txt').write_text(content)
            sign_manifest(self.private, self.release / 'CHECKSUMS.txt')
            self.bind_manifest()
            result = self.install(False)
            self.assertIn(expected, result.stderr)
            self.assertEqual(self.snapshot(), {})

    def test_malformed_identity_preserved(self):
        (self.home / '.ww').mkdir()
        (self.home / '.ww/identity').write_text('malformed identity')
        before = self.snapshot()
        result = self.install(False)
        self.assertIn('existing identity is malformed', result.stderr)
        self.assert_native_refusal_preserved(before)

    def test_unknown_kernel_collision_preserved(self):
        destination = self.home / '.ww/fhs/boot/kernel.wasm'
        destination.parent.mkdir(parents=True)
        destination.write_text('operator kernel')
        before = self.snapshot()
        result = self.install(False)
        self.assertIn('unrecognized or altered installation file', result.stderr)
        self.assertIn(str(destination), result.stderr)
        self.assert_native_refusal_preserved(before)

    def test_installer_cannot_update_the_managed_payload(self):
        self.install()
        before = self.snapshot()
        # A legal custom section changes bytes without changing behavior.
        with (self.release / 'fhs/bin/status.wasm').open('ab') as output:
            output.write(b'\x00\x05\x01xabc')
        self.checksums()
        result = self.install(False)
        self.assertIn('ww image update', result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_direct_executable_remains_regular(self):
        destination = self.home / '.ww/bin/ww'
        destination.parent.mkdir(parents=True)
        shutil.copy2(BINARY, destination)
        bootstrap = bootstrap_arguments(self.root / 'bootstrap', destination, FHS)
        result = subprocess.run([str(destination), 'install', *bootstrap], env=self.env, text=True,
                                capture_output=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(destination.is_symlink())
        subprocess.run([str(destination), '--version'], env=self.env,
                       check=True, capture_output=True)

    def test_unsigned_fresh_install_refuses_without_writes(self):
        before = self.snapshot()
        result = subprocess.run([str(BINARY), 'install'], env=self.env,
                                text=True, capture_output=True, timeout=120)
        self.assertNotEqual(result.returncode, 0)
        self.assert_native_refusal_preserved(before)

    def test_legacy_upgrade_is_a_usage_error_without_writes(self):
        self.install()
        before = self.snapshot()
        result = subprocess.run([str(self.home / '.ww/bin/ww'), 'perform', 'upgrade'],
                                env=self.env, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_native_metadata_transport_failure_preserves_home(self):
        self.env['IPFS_API'] = 'http://127.0.0.1:1'
        before = self.snapshot()
        result = self.install(False)
        self.assertIn('fetch release content', result.stderr)
        self.assertIn('/CHECKSUMS.txt', result.stderr)
        self.assertEqual(self.metadata.requests, [])
        self.assert_native_refusal_preserved(before)

    def test_native_descriptor_binding_failure_preserves_home(self):
        # The shell's manifest membership proof remains valid. Only native
        # descriptor verification can reject this missing descriptor link.
        self.bind_manifest(descriptor_link=False)
        before = self.snapshot()
        result = self.install(False)
        self.assertIn('transport root has no descriptor link', result.stderr)
        self.assertEqual(self.metadata.requests[-2:], [
            ('/api/v0/block/get', self.descriptor_cid),
            ('/api/v0/block/get', self.release_cid)])
        self.assert_native_refusal_preserved(before)

    def test_native_metadata_signature_failure_preserves_home(self):
        # Shell transport still serves the valid signature. The native HTTP
        # reread must authenticate independently before reading the descriptor.
        signature = ('/api/v0/cat', f'/ipfs/{self.release_cid}/CHECKSUMS.txt.sig')
        self.metadata.routes[signature] = b'\x00' * 64
        before = self.snapshot()
        result = self.install(False)
        self.assertIn('release manifest signature is not authorized', result.stderr)
        self.assertEqual(self.metadata.requests, [
            ('/api/v0/cat', f'/ipfs/{self.release_cid}/CHECKSUMS.txt'), signature])
        self.assert_native_refusal_preserved(before)


if __name__ == '__main__':
    unittest.main(argv=[__file__] + remaining, verbosity=2)
