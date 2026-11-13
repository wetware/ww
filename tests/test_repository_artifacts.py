#!/usr/bin/env python3
"""Exercise the committed-artifact boundary in disposable Git repositories."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/check_repository_artifacts.py"


class RepositoryArtifacts(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.git("init", "-q")

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args], stderr=subprocess.PIPE)

    def commit(self, files):
        for name, raw in files.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(raw)
        self.git("add", ".")
        self.git("-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
                 "commit", "-qm", "generated fixture")

    def check(self):
        return subprocess.run([sys.executable, str(SCRIPT)], cwd=self.root,
                              capture_output=True, text=True, timeout=30)

    def test_source_and_small_intentional_formats_pass(self):
        self.commit({"script": b"#!/bin/sh\ntrue\n", "sample.wasm": b"\0asm\x01\0\0\0",
                     "sample.car": b"generated CAR framing", "sample.zip": b"PK\x03\x04fixture",
                     "measurement.json": b" " * (1024 * 1024)})
        self.assertEqual(self.check().returncode, 0)

    def test_native_headers_refuse_regardless_of_name_or_executable_mode(self):
        headers = [b"\x7fELF", b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe",
                   b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe",
                   b"\xbe\xba\xfe\xca", b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"]
        self.commit({f"fixtures/disguised-{i}.txt": header + b"generated"
                     for i, header in enumerate(headers)})
        result = self.check()
        self.assertEqual(result.returncode, 1)
        for i in range(len(headers)):
            self.assertIn(f"disguised-{i}.txt", result.stderr)

    def test_pe_signature_refuses_but_mz_text_does_not(self):
        pe = bytearray(132)
        pe[:2] = b"MZ"
        pe[60:64] = (128).to_bytes(4, "little")
        pe[128:] = b"PE\0\0"
        self.commit({"windows.bin": pe, "notes.txt": b"MZ starts this ordinary text"})
        result = self.check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("windows.bin", result.stderr)
        self.assertNotIn("notes.txt", result.stderr)

    def test_large_blob_and_small_retired_input_refuse(self):
        self.commit({"release.zip": b"PK\x03\x04" + b"x" * 1024 * 1024,
                     "tests/fixtures/native-release-originals/inputs/renamed.json": b"{}"})
        result = self.check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("release.zip", result.stderr)
        self.assertIn("retired historical", result.stderr)

    def test_committed_bytes_cannot_be_hidden_by_checkout_edits(self):
        self.commit({"hidden": b"\x7fELFgenerated"})
        (self.root / "hidden").write_text("ordinary checkout")
        self.assertEqual(self.check().returncode, 1)

    def test_untracked_build_output_does_not_fail(self):
        self.commit({"source.rs": b"fn main() {}"})
        (self.root / "untracked").write_bytes(b"\x7fELFgenerated")
        self.assertEqual(self.check().returncode, 0)


if __name__ == "__main__":
    unittest.main()
