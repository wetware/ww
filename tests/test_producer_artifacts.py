#!/usr/bin/env python3
"""The public producer emits data receipts, never an assembled release."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('producer', Path(__file__).parents[1] / 'scripts/producer_artifacts.py')
producer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(producer)


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
