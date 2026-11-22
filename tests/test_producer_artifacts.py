#!/usr/bin/env python3
"""The public producer emits data receipts, never an assembled release."""
import importlib.util
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import textwrap
import unittest

spec = importlib.util.spec_from_file_location('producer', Path(__file__).parents[1] / 'scripts/producer_artifacts.py')
producer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(producer)


class WorkflowPackaging(unittest.TestCase):
    """Exercise the real package step across the cache/download boundary."""

    def setUp(self):
        self.repository = Path(__file__).resolve().parents[1]
        workflow = (self.repository / '.github/workflows/rust.yml').read_text()
        job = re.search(r'(?ms)^  build-binaries:\n.*?(?=^  [\w-]+:|\Z)', workflow)
        self.assertIsNotNone(job, 'build-binaries job missing')
        steps = re.split(r'(?m)^    - ', job.group())[1:]

        def step(prefix):
            matches = [(i, value) for i, value in enumerate(steps) if value.startswith(prefix)]
            self.assertEqual(len(matches), 1, prefix)
            return matches[0]

        self.cache_index, _ = step('uses: Swatinem/rust-cache@')
        self.download_index, download = step('uses: actions/download-artifact@')
        self.assertIn('        name: wasm-artifacts\n', download)
        _, package = step('name: Package binary\n')
        self.package_script = textwrap.dedent(package.split('      run: |\n', 1)[1])
        for name, value in {
            'matrix.bin_path': 'bin/linux/musl-x86_64',
            'matrix.target': 'x86_64-unknown-linux-musl',
            'github.sha': 'a' * 40, 'github.run_id': '123', 'github.run_attempt': '2',
        }.items():
            self.package_script = self.package_script.replace('${{ ' + name + ' }}', value)
        self.assertNotIn('${{', self.package_script)

    def package(self, cache, invalid=None):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        sources = {'boot/kernel.wasm': 'std/kernel/bin/main.wasm',
                   'bin/status.wasm': 'std/status/bin/status.wasm'}
        payloads = {'boot/kernel.wasm': b'kernel', 'bin/status.wasm': b'status',
                    'etc/config': b'preserve the complete FHS'}

        def write(name, data):
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)

        for _, operation in sorted(((self.cache_index, 'cache'), (self.download_index, 'download'))):
            if operation == 'cache':
                # rust-cache d1527c8f, src/{restore,cleanup}.ts: a partial
                # match recursively unlinks ordinary files in non-Cargo target
                # directories. Model only this external effect, not the action.
                if cache == 'partial':
                    for path in (root / 'target/fhs').rglob('*'):
                        if path.is_file():
                            path.unlink()
            else:
                for name, data in payloads.items():
                    write('target/fhs/' + name, data)
                    if name in sources:
                        write(sources[name], data)
                (root / 'target/fhs/var/empty').mkdir(parents=True)
                if invalid:
                    name, damage = invalid
                    path = root / 'target/fhs' / name
                    if damage == 'missing':
                        path.unlink()
                    elif damage == 'empty':
                        # Both copies empty: cmp alone must not admit them.
                        path.write_bytes(b'')
                        (root / sources[name]).write_bytes(b'')
                    else:
                        path.write_bytes(b'different payload')

        write('target/x86_64-unknown-linux-musl/release/ww', b'fixture host')
        (root / 'scripts').mkdir()
        shutil.copyfile(self.repository / 'scripts/producer_artifacts.py',
                        root / 'scripts/producer_artifacts.py')
        result = subprocess.run(['bash', '--noprofile', '--norc', '-eo', 'pipefail',
                                 '-c', self.package_script], cwd=root, capture_output=True, text=True)
        return result, root / 'bin/linux/musl-x86_64'

    def test_packaging_survives_cache_cleanup(self):
        for cache in ('miss', 'full', 'partial'):
            with self.subTest(cache=cache):
                result, package = self.package(cache)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                for name, data in {'ww': b'fixture host', 'fhs/boot/kernel.wasm': b'kernel',
                                   'fhs/bin/status.wasm': b'status',
                                   'fhs/etc/config': b'preserve the complete FHS'}.items():
                    self.assertEqual((package / name).read_bytes(), data)
                self.assertTrue((package / 'fhs/var/empty').is_dir())
                self.assertEqual(json.loads((package / 'PROVENANCE.json').read_text()), {
                    'repository': 'https://github.com/wetware/ww', 'revision': 'a' * 40,
                    'builder': 'github-actions:123:2:x86_64-unknown-linux-musl'})

    def test_invalid_wasm_refuses_packaging(self):
        for name in ('boot/kernel.wasm', 'bin/status.wasm'):
            for damage in ('missing', 'empty', 'mismatch'):
                with self.subTest(name=name, damage=damage):
                    result, package = self.package('partial', (name, damage))
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse((package / 'ww').exists())
                    self.assertFalse((package / 'PROVENANCE.json').exists())


class ProducerArtifacts(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.revision = 'a' * 40
        for name, target in producer.TARGETS.items():
            root = self.root / name
            (root / 'fhs/boot').mkdir(parents=True)
            (root / 'fhs/bin').mkdir()
            (root / 'ww').write_bytes(name.encode())
            (root / 'fhs/boot/kernel.wasm').write_bytes(b'kernel')
            (root / 'fhs/bin/status.wasm').write_bytes(b'status')
            producer.record(root, self.revision, '123', '2', target)

    def validate(self):
        producer.validate_hosts(self.root, self.revision, '123', '2')

    def test_complete_current_producer_set(self):
        self.validate()

    def test_substitution_extra_file_missing_and_stale_receipts(self):
        root = self.root / 'ww-linux-x86_64'
        for mutate in ('substitute', 'extra', 'missing', 'stale'):
            with self.subTest(mutate=mutate):
                original = (root / 'ww').read_bytes()
                receipt = (root / 'PROVENANCE.json').read_bytes()
                if mutate == 'substitute':
                    (root / 'ww').write_bytes(b'changed')
                elif mutate == 'extra':
                    (root / 'unexpected').write_bytes(b'extra')
                elif mutate == 'missing':
                    (root / 'ww').unlink()
                else:
                    value = json.loads(receipt)
                    value['builder'] = value['builder'].replace(':2:', ':1:')
                    (root / 'PROVENANCE.json').write_text(json.dumps(value))
                    producer.checksums(root)
                with self.assertRaises(ValueError):
                    self.validate()
                (root / 'unexpected').unlink(missing_ok=True)
                (root / 'ww').write_bytes(original)
                (root / 'PROVENANCE.json').write_bytes(receipt)
                producer.checksums(root)

    def test_different_valid_fhs_rejected(self):
        root = self.root / 'ww-linux-x86_64'
        (root / 'fhs/bin/status.wasm').write_bytes(b'other-valid-payload')
        producer.checksums(root)
        with self.assertRaisesRegex(ValueError, 'FHS'):
            self.validate()

    def test_checksums_use_relative_posix_path_order(self):
        root = self.root / 'ww-linux-x86_64'
        (root / 'fhs/bin-a').write_bytes(b'prefix sibling')
        producer.checksums(root)
        paths = [line.split('  ', 1)[1] for line in (root / 'SHA256SUMS').read_text().splitlines()]
        self.assertEqual(paths, sorted(paths))

    def test_symlink_rejected(self):
        root = self.root / 'ww-linux-x86_64'
        (root / 'link').symlink_to(root / 'ww')
        with self.assertRaises(ValueError):
            producer.checksums(root)

    def test_checksum_order_and_duplicates_are_rejected(self):
        root = self.root / 'ww-linux-x86_64'
        receipt = root / 'SHA256SUMS'
        original = receipt.read_bytes()
        for invalid in (b''.join(reversed(original.splitlines(keepends=True))), original + original):
            receipt.write_bytes(invalid)
            with self.assertRaises(ValueError):
                self.validate()


if __name__ == '__main__':
    unittest.main()
