#!/usr/bin/env python3
"""Compare package utilities using real CLI calls, not native Host execution.

Build receipts must establish the supplied development and release profiles.
Synthetic Host files cross the UnixFS chunk boundary at all five release paths.
These checks establish package-byte equivalence and refusal behavior only.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

from release_auth_fixture import generate_key, package_release


TARGETS = {
    'x86_64-unknown-linux-gnu': 'bin/ww/linux/x86_64/ww',
    'x86_64-unknown-linux-musl': 'bin/ww/linux/musl-x86_64/ww',
    'aarch64-unknown-linux-gnu': 'bin/ww/linux/aarch64/ww',
    'x86_64-apple-darwin': 'bin/ww/macos/x86_64/ww',
    'aarch64-apple-darwin': 'bin/ww/macos/aarch64/ww',
}


def files(directory):
    """Read exact regular-file bytes; no hard links or symbolic links are allowed."""
    result = {}
    for path in sorted(directory.rglob('*')):
        if path.is_symlink():
            raise AssertionError(f'fixture contains a symbolic link: {path}')
        if path.is_file():
            if path.stat().st_nlink != 1:
                raise AssertionError(f'fixture contains a hard link: {path}')
            result[str(path.relative_to(directory))] = path.read_bytes()
    return result


class PackageProfiles(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix='ww-package-profiles-')
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        seed = cls.root / 'seed'
        generate_key(seed / 'keys')
        tree = seed / 'tree'
        shutil.copytree(cls.arguments.fhs, tree / 'fhs', copy_function=shutil.copyfile)
        payload = bytes(range(256)) * 4096 + b'after-one-MiB-chunk'
        for relative in TARGETS.values():
            host = tree / relative
            host.parent.mkdir(parents=True)
            host.write_bytes(payload)
            host.chmod(0o600)
        cls.tools, cls.packages, cls.receipts = {}, {}, {}
        for profile in ('development', 'release'):
            tool = cls.root / f'{profile}-package-tool'
            shutil.copyfile(getattr(cls.arguments, profile + '_tool'), tool)
            tool.chmod(0o700)
            fixture = cls.root / profile
            shutil.copytree(seed, fixture, copy_function=shutil.copyfile)
            # Independent files retain the same key and exact inputs. Ed25519
            # signs the same manifest deterministically in both fixtures.
            receipt = package_release(tool, fixture / 'tree', fixture / 'spool',
                                      fixture / 'keys/private.pem',
                                      fixture / 'keys/trusted-keys', list(TARGETS))
            cls.tools[profile] = tool
            cls.packages[profile] = fixture
            cls.receipts[profile] = receipt

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='case-', dir=self.root)
        self.addCleanup(temporary.cleanup)
        self.case = Path(temporary.name)

    def copy_package(self, profile):
        destination = self.case / profile
        shutil.copytree(self.packages[profile], destination, copy_function=shutil.copyfile)
        for path in destination.rglob('*'):
            self.assertFalse(path.is_symlink(), str(path))
            if path.is_file():
                self.assertEqual(path.stat().st_nlink, 1, str(path))
        return destination

    def invoke(self, profile, command, fixture, trusted_keys=None):
        arguments = [str(self.tools[profile]), command,
                     '--spool', str(fixture / 'spool'),
                     '--trusted-keys', str(trusted_keys or fixture / 'keys/trusted-keys')]
        if command == 'finalize':
            arguments += ['--tree', str(fixture / 'tree')]
        return subprocess.run(arguments, capture_output=True, text=True, timeout=90)

    def assert_refused(self, result, reason):
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual(result.stdout, '')
        self.assertIn(reason, result.stderr)

    def test_canonical_bytes_and_authenticated_receipts_match(self):
        self.assertEqual(self.receipts['development'], self.receipts['release'])
        for directory in ('tree', 'spool'):
            with self.subTest(directory=directory):
                self.assertEqual(files(self.packages['development'] / directory),
                                 files(self.packages['release'] / directory))
        for profile in self.tools:
            with self.subTest(profile=profile):
                fixture = self.copy_package(profile)
                manifest = (fixture / 'tree/CHECKSUMS.txt').read_text()
                rows = dict(line.split('  ', 1)[::-1]
                            for line in manifest.splitlines()[1:])
                self.assertTrue(set(TARGETS.values()).issubset(rows))
                for relative, expected in rows.items():
                    self.assertEqual(hashlib.sha256((fixture / 'tree' / relative).read_bytes())
                                     .hexdigest(), expected)
                self.assertEqual((fixture / 'tree/CHECKSUMS.txt.sig').stat().st_size, 64)
                result = self.invoke(profile, 'verify', fixture)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(json.loads(result.stdout), self.receipts[profile])

    def test_changed_host_is_refused(self):
        for profile in self.tools:
            with self.subTest(profile=profile):
                fixture = self.copy_package(profile)
                host = fixture / 'tree' / TARGETS['aarch64-unknown-linux-gnu']
                with host.open('r+b') as output:
                    output.write(b'changed')
                self.assert_refused(self.invoke(profile, 'finalize', fixture),
                                    'artifact root differs from descriptor')

    def test_independent_untrusted_key_is_refused(self):
        for profile in self.tools:
            with self.subTest(profile=profile):
                fixture = self.copy_package(profile)
                _, unknown = generate_key(fixture / 'untrusted-keys')
                self.assert_refused(self.invoke(profile, 'finalize', fixture, unknown),
                                    'release manifest signature is not authorized')

    def test_tampered_signature_is_refused(self):
        for profile in self.tools:
            with self.subTest(profile=profile):
                fixture = self.copy_package(profile)
                (fixture / 'tree/CHECKSUMS.txt.sig').write_bytes(bytes(64))
                self.assert_refused(self.invoke(profile, 'finalize', fixture),
                                    'release manifest signature is not authorized')

    def test_changed_raw_block_is_refused_by_cid(self):
        for profile in self.tools:
            with self.subTest(profile=profile):
                fixture = self.copy_package(profile)
                # The full 1 MiB raw block is shared by all five Host paths.
                blocks = [path for path in (fixture / 'spool/blocks').iterdir()
                          if path.read_bytes() == bytes(range(256)) * 4096]
                self.assertEqual(len(blocks), 1)
                with blocks[0].open('r+b') as output:
                    output.write(b'changed')
                self.assert_refused(self.invoke(profile, 'verify', fixture),
                                    'release block CID mismatch')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--development-tool', type=Path, required=True)
    parser.add_argument('--release-tool', type=Path, required=True)
    parser.add_argument('--fhs', type=Path, required=True)
    arguments, remaining = parser.parse_known_args()
    for name in ('development_tool', 'release_tool', 'fhs'):
        setattr(arguments, name, getattr(arguments, name).resolve(strict=True))
    utilities = {}
    for profile in ('development', 'release'):
        path = getattr(arguments, profile + '_tool')
        with path.open('rb') as stream:
            digest = hashlib.file_digest(stream, 'sha256').hexdigest()
        utilities[profile] = {'path': str(path), 'size': path.stat().st_size, 'sha256': digest}
    print(json.dumps({'package_utilities': utilities}, sort_keys=True), flush=True)
    if utilities['development']['sha256'] == utilities['release']['sha256']:
        parser.error('development and release utilities must have distinct executable bytes')
    PackageProfiles.arguments = arguments
    unittest.main(argv=[__file__] + remaining, verbosity=2)
