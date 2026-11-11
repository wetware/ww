#!/usr/bin/env python3
"""Retained acquisition must stay offline and reject changed capture evidence."""
import shutil
import sys
from pathlib import Path
from unittest.mock import patch

from test_verify_native_release_inputs import Fixture

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import fetch_native_release_inputs as fetch


class RetainedAcquisition(Fixture):
    def test_copy_retained_never_fetches(self):
        # Use the real immutable capture so its pins and byte bindings are tested.
        root = Path(__file__).resolve().parents[1]
        inputs = root / 'tests/fixtures/native-release-originals/inputs'
        pins = fetch.read_json(root / '.github/native-release-integration.json')
        output = self.root / 'copied'
        with patch.object(fetch.subprocess, 'run', side_effect=AssertionError('network request')):
            fetch.copy_retained(inputs, output, pins)
        for source in inputs.iterdir():
            self.assertEqual(source.read_bytes(), (output / source.name).read_bytes())
        with self.assertRaisesRegex(RuntimeError, 'must be absent'):
            fetch.copy_retained(inputs, output, pins)

    def test_expired_or_failed_transport_does_not_claim_acquisition(self):
        # A failed original download must not fall back to another producer or emit completion receipts.
        root = Path(__file__).resolve().parents[1]
        pins = root / '.github/native-release-integration.json'
        for timeout in (False, True):
            output = self.root / ('timeout' if timeout else 'expired')
            def failed_request(argv, **kwargs):
                self.assertIn('--method', argv)
                self.assertEqual(argv[argv.index('--method') + 1], 'GET')
                kwargs['stdout'].write(b'incomplete download')
                if timeout:
                    raise fetch.subprocess.TimeoutExpired(argv, kwargs['timeout'])
                raise fetch.subprocess.CalledProcessError(1, argv, stderr=b'HTTP 410 Gone')
            with self.subTest(timeout=timeout), \
                    patch.object(sys, 'argv', ['fetch', '--pins', str(pins), '--output', str(output)]), \
                    patch.object(fetch.subprocess, 'check_output', return_value='gh api help'), \
                    patch.object(fetch.subprocess, 'run', side_effect=failed_request):
                with self.assertRaises(fetch.subprocess.TimeoutExpired if timeout else fetch.subprocess.CalledProcessError):
                    fetch.main()
            self.assertFalse((output / 'fetch-receipts.json').exists())
            self.assertFalse((output / 'verified-inputs.json').exists())

    def test_mutated_capture_has_no_output(self):
        root = Path(__file__).resolve().parents[1]
        inputs = self.root / 'retained'
        shutil.copytree(root / 'tests/fixtures/native-release-originals/inputs', inputs)
        pins = fetch.read_json(root / '.github/native-release-integration.json')
        (inputs / 'a-run.json').write_text('{}')
        output = self.root / 'copied'
        with self.assertRaisesRegex(RuntimeError, 'digest differs'):
            fetch.copy_retained(inputs, output, pins)
        self.assertFalse(output.exists())


if __name__ == '__main__':
    import unittest
    unittest.main()
