#!/usr/bin/env python3
"""Refusal and bounded-command controls; these are not native qualification."""

import importlib.util
import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time
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
    def test_timeout_cleans_descendants_after_the_command_leader_exits(self):
        import fcntl
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            held = root / "descendant.lock"
            ready = root / "descendant.json"
            program = (
                "import fcntl,json,os,pathlib,sys,time\n"
                "child=os.fork()\n"
                "if child: os._exit(0)\n"
                "lock=open(sys.argv[1],'w')\n"
                "fcntl.flock(lock,fcntl.LOCK_EX)\n"
                "pathlib.Path(sys.argv[2]).write_text(json.dumps({'group':os.getpgrp()}))\n"
                "time.sleep(60)\n"
            )
            runner = MODULE.Runner(root, root, 1)
            try:
                with self.assertRaisesRegex(RuntimeError, "command deadline"):
                    runner.run("orphan-output", [sys.executable, "-c", program, held, ready],
                               deadline=time.monotonic() + 0.5)
                self.assertTrue(ready.exists(), "descendant probe did not execute")
                with held.open() as lock:
                    until = time.monotonic() + 2
                    while True:
                        try:
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                            break
                        except BlockingIOError:
                            self.assertLess(time.monotonic(), until, "descendant survived cleanup")
                            time.sleep(0.01)
            finally:
                if ready.exists():
                    with held.open() as lock:
                        try:
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        except BlockingIOError:
                            # The held lock proves this probe's original group
                            # still exists; do not signal a released/reused ID.
                            os.killpg(json.loads(ready.read_text())["group"], signal.SIGKILL)

    def test_readiness_manager_command_obeys_the_readiness_deadline(self):
        class SlowManager(MODULE.Runner):
            def run(self, name, argv, **kwargs):
                return super().run(name, [sys.executable, "-c", "import time; time.sleep(1)"], **kwargs)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runner = SlowManager(root, root, 0.1)
            started = time.monotonic()
            with self.assertRaisesRegex(RuntimeError, "readiness deadline"):
                MODULE.readiness(runner, {}, "slow-manager")
            self.assertLess(time.monotonic() - started, 0.75)

    def test_cancellation_reaps_child_after_output_descriptors_close(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runner = MODULE.Runner(root, root, 60)
            cancelled = threading.Event()
            timer = threading.Timer(0.1, cancelled.set)
            timer.start()
            try:
                started = time.monotonic()
                with self.assertRaisesRegex(RuntimeError, "command cancelled"):
                    runner.run("cancelled", [sys.executable, "-c",
                               "import os,time; os.close(1); os.close(2); time.sleep(60)"],
                               cancelled=cancelled.is_set)
                self.assertLess(time.monotonic() - started, 5)
                self.assertEqual(runner.commands[-1]["exit"], -signal.SIGKILL)
            finally:
                timer.cancel()
                timer.join()

    def test_command_deadline_kills_and_reaps_owned_child(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runner = MODULE.Runner(root, root, 60)
            started = time.monotonic()
            with self.assertRaisesRegex(RuntimeError, "command deadline"):
                runner.run("deadline", [sys.executable, "-c", "import time; time.sleep(60)"],
                           deadline=started + 0.1)
            self.assertLess(time.monotonic() - started, 5)
            self.assertEqual(runner.commands[-1]["exit"], -signal.SIGKILL)

    def test_candidate_watcher_timeout_does_not_claim_injection(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runner = MODULE.Runner(root, root, 0.1)
            with MODULE.CandidatePauseWatcher(runner, "a" * 64, (123, "b" * 32)) as watcher:
                watcher.thread.join(timeout=2)
            with self.assertRaisesRegex(RuntimeError, "did not inject"):
                watcher.require_injected()
            self.assertFalse(json.loads((root / "readiness-timeout-watcher/result.json").read_text())["injected"])

    @unittest.skipUnless(sys.platform == "linux" and hasattr(os, "pidfd_open")
                         and hasattr(signal, "pidfd_send_signal"), "Linux pidfd signal support required")
    def test_pidfd_fault_refuses_wrong_bytes_changed_context_and_exited_child(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        descriptor = os.pidfd_open(child.pid)
        try:
            deadline = time.monotonic() + 5
            expected = MODULE.sha256(Path(f"/proc/{child.pid}/exe"))
            with self.assertRaisesRegex(RuntimeError, "candidate executable"):
                MODULE.pause_verified_process(child.pid, "0" * 64, lambda *_: None, deadline)
            with self.assertRaisesRegex(RuntimeError, "manager changed"):
                MODULE.pause_verified_process(child.pid, expected,
                    lambda *_: MODULE.require(False, "manager changed"), deadline)
            self.assertNotIn(Path(f"/proc/{child.pid}/stat").read_text().rsplit(")", 1)[1].split()[0], ("T", "t"))
            observed = []
            result = MODULE.pause_verified_process(child.pid, expected,
                                                   lambda pid, token: observed.append((pid, token)), deadline)
            self.assertEqual(result["pid"], child.pid)
            self.assertEqual(result["sha256"], expected)
            self.assertTrue(observed)
            stop_deadline = time.monotonic() + 1
            while Path(f"/proc/{child.pid}/stat").read_text().rsplit(")", 1)[1].split()[0] not in ("T", "t"):
                self.assertLess(time.monotonic(), stop_deadline)
                time.sleep(0.01)
            signal.pidfd_send_signal(descriptor, signal.SIGKILL)
            child.wait(timeout=2)
            with self.assertRaises((OSError, RuntimeError)):
                MODULE.pause_verified_process(child.pid, expected, lambda *_: None, deadline)
        finally:
            if child.poll() is None:
                signal.pidfd_send_signal(descriptor, signal.SIGKILL)
            child.wait(timeout=2)
            os.close(descriptor)

    def test_reply_acknowledgement_requires_live_peer_without_extra_bytes(self):
        for state in ("live", "closed", "extra"):
            with self.subTest(state=state):
                # Create and close both descriptors explicitly for each case.
                left, right = socket.socketpair()
                try:
                    if state == "closed":
                        right.close()
                    elif state == "extra":
                        right.sendall(b"unexpected")
                    if state == "live":
                        MODULE.acknowledge_live_reply(left)
                        self.assertEqual(right.recv(1), b"\x06")
                    else:
                        with self.assertRaisesRegex(RuntimeError, "closed or sent extra"):
                            MODULE.acknowledge_live_reply(left)
                finally:
                    left.close()
                    right.close()

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
