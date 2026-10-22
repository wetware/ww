#!/usr/bin/env python3
"""Real Host installer tests. Only IPFS transport uses a local fixture."""
import argparse
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import unittest
from kubo_fixture import isolated_kubo

REPO = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--fhs', type=Path, required=True)
args, remaining = parser.parse_known_args()
BINARY = args.binary.resolve(strict=True)
FHS = args.fhs.resolve(strict=True)
HOST_PATH = 'bin/ww/{}/{}/ww'.format(
    'macos' if platform.system() == 'Darwin' else 'linux',
    'aarch64' if platform.machine() in ('arm64', 'aarch64') else 'x86_64')
PAYLOADS = [HOST_PATH, 'fhs/boot/kernel.wasm', 'fhs/bin/status.wasm']


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Installation(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='ww-install-test-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.home = self.root / 'home'
        self.home.mkdir()
        self.release = self.root / 'release'
        (self.release / HOST_PATH).parent.mkdir(parents=True)
        shutil.copy2(BINARY, self.release / HOST_PATH)
        shutil.copytree(FHS, self.release / 'fhs')
        self.checksums()
        self.stubs = self.root / 'transport'
        self.stubs.mkdir()
        ipfs = self.stubs / 'ipfs'
        ipfs.write_text('''#!/bin/sh
set -eu
case "$1" in
  id) exit 0 ;;
  cat)
    case "$2" in /ipfs/test-release/*) ;; *) exit 91 ;; esac
    p=${2#/ipfs/test-release/}
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
        self.env.update(HOME=str(self.home), TEST_RELEASE=str(self.release),
                        PATH=str(self.stubs) + os.pathsep + os.environ['PATH'],
                        NO_COLOR='1', WW_CWASM_DIR=str(self.root / 'cwasm'), IPFS_API=os.environ['IPFS_API'],
                        IPFS_ROUTING_API=os.environ['IPFS_ROUTING_API'])

    def checksums(self):
        (self.release / 'CHECKSUMS.txt').write_text('# sha256\n' + ''.join(
            f'{digest(self.release / name)}  {name}\n' for name in PAYLOADS))

    def install(self, success=True):
        result = subprocess.run(['sh', str(REPO / 'scripts/install.sh'),
                                 '--version', 'test-release'], env=self.env,
                                text=True, capture_output=True, timeout=120)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertTrue((self.home / '.ww/identity').is_file())
            installed = self.home / '.ww/bin/ww'
            self.assertFalse(installed.is_symlink())
            self.assertEqual(digest(installed), digest(BINARY))
            for name in ('boot/kernel.wasm', 'bin/status.wasm'):
                self.assertEqual((self.home / '.ww/fhs' / name).read_bytes(),
                                 (self.release / 'fhs' / name).read_bytes())
            subprocess.run([str(installed), '--version'], env=self.env,
                           capture_output=True, check=True, timeout=15)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def snapshot(self):
        return {str(p.relative_to(self.home)): p.read_bytes()
                for p in self.home.rglob('*') if p.is_file() and not p.is_symlink()}

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
        self.assertEqual(len(services), 1)
        service = services[0].read_text()
        self.assertIn(str(self.home / '.ww/bin/ww'), service)
        self.assertIn(str(self.home / '.ww/fhs'), service)
        self.assertNotIn('ww-install.', service)

    def test_missing_payloads_and_checksums_do_not_replace(self):
        for name in PAYLOADS + ['CHECKSUMS.txt']:
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
                    self.checksums()
                    before = self.snapshot()
                    self.install(False)
                    self.assertEqual(self.snapshot(), before)
                    (self.release / name).write_bytes(original)
                    self.checksums()

    def test_checksum_mismatch_missing_and_duplicate_entries_refused(self):
        original = (self.release / 'CHECKSUMS.txt').read_text()
        for content in (original.replace(digest(self.release / PAYLOADS[1]), '0' * 64),
                        '\n'.join(original.splitlines()[:-1]) + '\n',
                        original + original.splitlines()[1] + '\n'):
            (self.release / 'CHECKSUMS.txt').write_text(content)
            self.install(False)
            self.assertEqual(self.snapshot(), {})

    def test_malformed_identity_preserved(self):
        (self.home / '.ww').mkdir()
        (self.home / '.ww/identity').write_text('malformed identity')
        before = self.snapshot()
        self.install(False)
        self.assertEqual(self.snapshot(), before)

    def test_unknown_kernel_collision_preserved(self):
        destination = self.home / '.ww/fhs/boot/kernel.wasm'
        destination.parent.mkdir(parents=True)
        destination.write_text('operator kernel')
        before = self.snapshot()
        self.install(False)
        self.assertEqual(self.snapshot(), before)

    def test_managed_payload_can_change(self):
        self.install()
        identity = (self.home / '.ww/identity').read_bytes()
        # A legal custom section changes bytes without changing behavior.
        with (self.release / 'fhs/bin/status.wasm').open('ab') as output:
            output.write(b'\x00\x05\x01xabc')
        self.checksums()
        self.install()
        self.assertEqual((self.home / '.ww/identity').read_bytes(), identity)

    def test_direct_executable_remains_regular(self):
        destination = self.home / '.ww/bin/ww'
        destination.parent.mkdir(parents=True)
        shutil.copy2(BINARY, destination)
        result = subprocess.run([str(destination), 'perform', 'install', '--payload',
                                 str(FHS)], env=self.env, text=True,
                                capture_output=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(destination.is_symlink())
        subprocess.run([str(destination), '--version'], env=self.env,
                       check=True, capture_output=True)

    def test_embedded_install_seeds_match_package(self):
        result = subprocess.run([str(BINARY), 'perform', 'install'], env=self.env,
                                text=True, capture_output=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for name in ('boot/kernel.wasm', 'bin/status.wasm'):
            self.assertEqual((self.home / '.ww/fhs' / name).read_bytes(),
                             (FHS / name).read_bytes())

    def test_native_upgrade_refuses_without_writes(self):
        self.install()
        before = self.snapshot()
        result = subprocess.run([str(self.home / '.ww/bin/ww'), 'perform', 'upgrade'],
                                env=self.env, capture_output=True, text=True, timeout=15)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('installer', result.stderr.lower())
        self.assertEqual(self.snapshot(), before)


if __name__ == '__main__':
    with isolated_kubo() as kubo:
        os.environ.update(kubo)
        unittest.main(argv=[__file__] + remaining, verbosity=2)
