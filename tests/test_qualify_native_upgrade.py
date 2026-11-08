#!/usr/bin/env python3
"""Refusal and bounded-command controls; these are not native qualification."""

import importlib.util
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


SOURCE = Path(__file__).resolve().parents[1] / "scripts/qualify_native_upgrade.py"
SPEC = importlib.util.spec_from_file_location("qualify_native_upgrade", SOURCE)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class Inputs(unittest.TestCase):
    def receipt(self):
        return {"path": "/fixture/original/ww", "sha256": "a" * 64,
                "revision": "b" * 40, "run_id": 123, "attempt": 2,
                "archive_sha256": "c" * 64,
                "builder": "github-actions:123:2:aarch64-unknown-linux-gnu"}

    def arguments(self):
        return SimpleNamespace(timeout_secs=60, release_a="bafya", release_b="bafyb",
                               ipfs_url="http://127.0.0.1:5001", platform="linux-aarch64",
                               service_manager="systemd", home="/home/ww739-fixture",
                               trusted_release_keys="/fixture/public-keys",
                               output="/home/ww739-fixture/evidence")

    def test_producer_identity_requires_exact_run_attempt_target_and_original_digests(self):
        receipt = self.receipt()
        self.assertEqual(MODULE.validate_producer(receipt, "aarch64-unknown-linux-gnu"), Path(receipt["path"]))
        for field, value in [("builder", "local-development-fixture"), ("run_id", 0),
                             ("attempt", True), ("revision", "b" * 39),
                             ("sha256", "A" * 64), ("archive_sha256", ""),
                             ("path", "relative/ww")]:
            with self.subTest(field=field):
                invalid = dict(receipt, **{field: value})
                with self.assertRaises(RuntimeError):
                    MODULE.validate_producer(invalid, "aarch64-unknown-linux-gnu")
        with self.assertRaises(RuntimeError):
            MODULE.validate_producer(dict(receipt, synthetic=True), "aarch64-unknown-linux-gnu")
        with self.assertRaises(RuntimeError):
            MODULE.validate_producer(receipt, "x86_64-unknown-linux-gnu")

    def test_nonlocal_transport_and_missing_budget_refuse_before_fixture_access(self):
        for field, value in [("timeout_secs", 0), ("timeout_secs", -1),
                             ("timeout_secs", 3601), ("release_b", "bafya"),
                             ("release_a", "/ipns/moving-name"),
                             ("ipfs_url", "https://127.0.0.1:5001"),
                             ("ipfs_url", "http://example.org:5001"),
                             ("ipfs_url", "http://secret@127.0.0.1:5001"),
                             ("ipfs_url", "http://127.0.0.1:5001/path")]:
            with self.subTest(field=field, value=value):
                args = self.arguments()
                setattr(args, field, value)
                with patch.object(MODULE.pwd, "getpwuid", side_effect=AssertionError("must refuse before account lookup")):
                    with self.assertRaises(RuntimeError):
                        MODULE.validate_arguments(args)

    def test_native_platform_and_disposable_account_are_required(self):
        args = self.arguments()
        with patch.object(MODULE.platform, "system", return_value="Darwin"):
            with self.assertRaisesRegex(RuntimeError, "native platform"):
                MODULE.validate_arguments(args)
        with patch.object(MODULE.platform, "system", return_value="Linux"), \
             patch.object(MODULE.platform, "machine", return_value="aarch64"), \
             patch.object(MODULE.os, "geteuid", return_value=1001), \
             patch.object(MODULE.pwd, "getpwuid", return_value=SimpleNamespace(pw_name="operator", pw_dir=args.home)):
            with self.assertRaisesRegex(RuntimeError, "dedicated ww739"):
                MODULE.validate_arguments(args)

    def test_paths_refuse_aliases_hardlinks_foreign_write_and_oversized_records(self):
        for value in ("relative", "/home/fixture/../operator", "/home/fixture\nExecStart=bad"):
            with self.assertRaises(RuntimeError):
                MODULE.absolute_path(value)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            original = root / "record"
            original.write_text("{}")
            alias = root / "alias"
            alias.symlink_to(original)
            with self.assertRaises(RuntimeError):
                MODULE.record(alias, os.geteuid())
            alias.unlink()
            os.link(original, alias)
            with self.assertRaises(RuntimeError):
                MODULE.record(original, os.geteuid())
            alias.unlink()
            original.chmod(0o666)
            with self.assertRaises(RuntimeError):
                MODULE.record(original, os.geteuid())
            original.chmod(0o600)
            original.write_bytes(b" " * (MODULE.MAX_RECORD + 1))
            with self.assertRaisesRegex(RuntimeError, "exceeds limit"):
                MODULE.record(original, os.geteuid())


class Commands(unittest.TestCase):
    def test_command_failure_and_output_overflow_preserve_non_success_ledger(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runner = MODULE.Runner(root, root, 2)
            with self.assertRaisesRegex(RuntimeError, "unexpected exit"):
                runner.run("failed", ["/bin/sh", "-c", "printf error >&2; exit 7"])
            self.assertEqual(json.loads((root / "commands.json").read_text())[0]["exit"], 7)
            self.assertEqual((root / "0000-failed.stderr").read_bytes(), b"error")
            with patch.object(MODULE, "MAX_COMMAND_OUTPUT", 16):
                with self.assertRaisesRegex(RuntimeError, "output exceeds bound"):
                    runner.run("overflow", ["/bin/sh", "-c", "printf 123456789012345678901234567890"])
            self.assertEqual(len(json.loads((root / "commands.json").read_text())), 2)


if __name__ == "__main__":
    unittest.main()
