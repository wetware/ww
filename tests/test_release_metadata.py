"""Producer provenance survives packaging by a different run."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = {"linux-x86_64": "linux/x86_64", "linux-aarch64": "linux/aarch64",
    "macos-aarch64": "macos/aarch64", "macos-x86_64": "macos/x86_64",
    "linux-musl-x86_64": "linux/musl-x86_64"}


class Metadata(unittest.TestCase):
    def test_actual_producer_survives_and_missing_receipt_fails(self):
        with tempfile.TemporaryDirectory() as work:
            work = Path(work)
            tree = work / "tree"
            tree.mkdir()
            (tree / "VERSION").write_text("1.0\n")
            (tree / "REVISION").write_text("b" * 40 + "\n")
            for platform, path in PLATFORMS.items():
                artifact = work / "artifacts" / ("ww-" + platform)
                artifact.mkdir(parents=True)
                binary = tree / "bin/ww" / path / "ww"
                binary.parent.mkdir(parents=True)
                binary.write_bytes(b"native Host")
                (artifact / "REVISION").write_text("a" * 40 + "\n")
                (artifact / "SHA256SUMS").write_text(hashlib.sha256(binary.read_bytes()).hexdigest() + "  ww\n")
                (artifact / "PROVENANCE.json").write_text(json.dumps(dict(repository="https://github.com/wetware/ww", revision="a" * 40, builder="original-build")))
            command = [sys.executable, str(ROOT / "scripts/release_metadata.py"), "--artifacts", str(work / "artifacts"), "--tree", str(tree), "--channel", "fixture", "--sequence", "1", "--builder", "packaging-build"]
            result = subprocess.run(command, check=True, capture_output=True, text=True)
            metadata = json.loads(result.stdout)
            self.assertEqual(metadata["source"]["builder"], "packaging-build")
            self.assertTrue(all(host["builder"] == "original-build" and host["revision"] == "a" * 40 for host in metadata["hosts"].values()))
            (artifact / "PROVENANCE.json").unlink()
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("invalid producer receipt", result.stderr)


if __name__ == "__main__":
    unittest.main()
