#!/usr/bin/env python3
"""Consumer refusal controls, not native recovery qualification."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[1] / "scripts/qualify_release_consumer.py"
SPEC = importlib.util.spec_from_file_location("qualify_release_consumer", SOURCE)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class AlternateRefusal(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.home = Path(temporary.name)
        for name, data in {"bin/ww": b"installed Host", "identity": b"private identity",
                           "installation.json": b"installation receipt",
                           "installed-release.json": b"release locator"}.items():
            path = self.home / ".ww" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("WW_", "IPFS_"))}
        self.env["HOME"] = str(self.home)
        self.before = MODULE.snapshot(self.home)
        self.shell_calls = 0

    def consume(self, name, action, root, selected=None, failure=None):
        # Model the current unmanaged-upgrade CLI boundary. Genuine release
        # authentication is exercised by the full rehearsal, not this unit test.
        self.assertEqual(action[0], "upgrade")
        self.assertIn("--timeout-secs", action)
        self.assertGreater(int(action[action.index("--timeout-secs") + 1]), 0)
        self.assertEqual(root, "alternate-release")
        self.assertEqual(selected, self.home / ".ww/bin/ww")
        self.assertEqual(failure, "unmanaged automatic native replacement is unsupported")

    def run_shell(self, name, command, env=None, failure=None):
        self.shell_calls += 1
        result = subprocess.run([str(part) for part in command], env=env,
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("Recovery requires a pending Host upgrade", result.stderr)
        self.assertIn(failure, result.stderr)
        self.assertNotIn("--timeout-secs", result.stderr)

    def exercise(self, consume=None, run=None):
        MODULE.check_alternate_refusal(
            consume or self.consume, run or self.run_shell, self.home, self.env,
            "original-release", "alternate-release", self.home / "unused-trust",
            "http://127.0.0.1:1")

    def test_refusal_then_real_shell_missing_transaction_preserve_state(self):
        self.exercise()
        self.assertEqual(self.shell_calls, 1)
        self.assertEqual(MODULE.snapshot(self.home), self.before)
        self.assertFalse((self.home / ".ww/host-upgrade.json").exists())

    def test_upgrade_mutation_is_rejected_before_shell(self):
        for name in ("bin/ww", "identity", "installation.json", "unexpected-state"):
            with self.subTest(name=name):
                path = self.home / ".ww" / name
                previous = path.read_bytes() if path.exists() else None
                def mutate(*args, **kwargs):
                    self.consume(*args, **kwargs)
                    path.write_bytes(b"changed")
                with self.assertRaisesRegex(RuntimeError, "refused upgrade changed"):
                    self.exercise(consume=mutate)
                if previous is None:
                    path.unlink()
                else:
                    path.write_bytes(previous)
        self.assertEqual(self.shell_calls, 0)

    def test_unexpected_journal_is_rejected_before_shell(self):
        def mutate(*args, **kwargs):
            self.consume(*args, **kwargs)
            # An empty sentinel tests detection only; it is not a transaction fixture.
            (self.home / ".ww/host-upgrade.json").touch()
        with self.assertRaisesRegex(RuntimeError, "journal"):
            self.exercise(consume=mutate)
        self.assertEqual(self.shell_calls, 0)

    def test_shell_mutation_is_rejected(self):
        def mutate(*args, **kwargs):
            self.run_shell(*args, **kwargs)
            (self.home / ".ww/installation.json").write_bytes(b"changed")
        with self.assertRaisesRegex(RuntimeError, "shell recovery refusal changed"):
            self.exercise(run=mutate)


if __name__ == "__main__":
    unittest.main(verbosity=2)
