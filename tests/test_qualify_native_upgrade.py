#!/usr/bin/env python3
"""Refusal and bounded-command controls; these are not native qualification."""

import importlib.util
import hashlib
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
import zipfile
from unittest.mock import patch


SOURCE = Path(__file__).resolve().parents[1] / "scripts/qualify_native_upgrade.py"
SPEC = importlib.util.spec_from_file_location("qualify_native_upgrade", SOURCE)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class Inputs(unittest.TestCase):
    def test_fault_stop_policy_requires_terminal_failure_configuration(self):
        observed = []
        def run(name, argv):
            observed.append((name, argv))
            return "TimeoutStopUSec=5s\nKillMode=control-group\nSendSIGKILL=yes\nRestart=no\n"
        MODULE.require_fault_stop_policy(SimpleNamespace(run=run))
        self.assertEqual(observed[0][0], "fault-stop-policy")
        self.assertIn("--property=Restart", observed[0][1])

    def test_fault_stop_policy_refuses_automatic_restart_or_missing_evidence(self):
        for value in ("Restart=on-failure\n", "Restart=always\n", ""):
            with self.subTest(value=value):
                runner = SimpleNamespace(run=lambda *_: "TimeoutStopUSec=5s\nKillMode=control-group\nSendSIGKILL=yes\n" + value)
                with self.assertRaisesRegex(RuntimeError, "no automatic restart"):
                    MODULE.require_fault_stop_policy(runner)

    def receipt(self):
        return {"path": "/fixture/original/ww", "sha256": "a" * 64,
                "archive_path": "/fixture/original.zip",
                "revision": "b" * 40, "run_id": 123, "attempt": 2,
                "archive_sha256": "c" * 64,
                "builder": "github-actions:123:2:aarch64-unknown-linux-gnu"}

    def test_original_archive_and_receipts_must_match_the_executable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "ww"
            revision = "b" * 40
            header = bytearray(64)
            header[:6] = b"\x7fELF\x02\x01"
            header[18:20] = (183).to_bytes(2, "little")
            binary.write_bytes(bytes(header) + revision.encode())
            receipt = dict(self.receipt(), path=str(binary), archive_path=str(root / "original.zip"),
                           sha256=MODULE.sha256(binary))
            files = {"ww": binary.read_bytes(), "REVISION": (revision + "\n").encode(),
                     "SHA256SUMS": (receipt["sha256"] + "  ww\n").encode(),
                     "PROVENANCE.json": json.dumps({"repository": "https://github.com/wetware/ww",
                         "revision": revision, "builder": receipt["builder"]}).encode()}
            for name, value in files.items():
                (root / name).write_bytes(value)

            def archive(values):
                with zipfile.ZipFile(receipt["archive_path"], "w") as output:
                    for name, value in values.items():
                        output.writestr(name, value)
                receipt["archive_sha256"] = MODULE.sha256(Path(receipt["archive_path"]))

            archive(files)
            evidence = MODULE.verify_producer_files(receipt, "aarch64-unknown-linux-gnu", os.geteuid())
            self.assertEqual(evidence["size"], 104)
            self.assertEqual(evidence["embedded_revision_bytes"], revision)
            for name, changed in (("ww", b"another binary"), ("REVISION", b"c" * 40 + b"\n"),
                                  ("SHA256SUMS", b"0" * 64 + b"  ww\n")):
                with self.subTest(name=name):
                    archive(dict(files, **{name: changed}))
                    with self.assertRaises(RuntimeError):
                        MODULE.verify_producer_files(receipt, "aarch64-unknown-linux-gnu", os.geteuid())
            archive(dict(files, **{"../escaped": b"unsafe"}))
            with self.assertRaisesRegex(RuntimeError, "archive member"):
                MODULE.verify_producer_files(receipt, "aarch64-unknown-linux-gnu", os.geteuid())
            archive(files)
            (root / "PROVENANCE.json").write_text(json.dumps({"repository": "https://github.com/other/repo",
                "revision": revision, "builder": receipt["builder"]}))
            with self.assertRaisesRegex(RuntimeError, "producer provenance"):
                MODULE.verify_producer_files(receipt, "aarch64-unknown-linux-gnu", os.geteuid())

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

    def test_durable_snapshot_detects_same_bytes_rewrite_and_new_journal(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            root = home / ".ww"
            root.mkdir()
            state = root / "installed-release.json"
            state.write_text('{"version":1,"release":"bafya"}')
            before = MODULE.durable_snapshot(home)
            self.assertEqual(before, MODULE.durable_snapshot(home))
            replacement = root / "replacement"
            replacement.write_bytes(state.read_bytes())
            replacement.replace(state)
            self.assertNotEqual(before, MODULE.durable_snapshot(home))
            before = MODULE.durable_snapshot(home)
            (root / "host-upgrade.json").write_text('{}')
            self.assertNotEqual(before, MODULE.durable_snapshot(home))


class RollbackScratch(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.home = Path(temporary.name)
        self.store = self.home / '.ww/bin/.ww-host-upgrade'
        (self.store / 'proofs').mkdir(parents=True)
        self.source = b'corrupt retained executable'
        self.retained = self.store / 'retained.ww'
        self.retained.write_bytes(self.source)
        (self.home / '.ww/bin/ww').write_bytes(b'installed current executable')
        (self.home / '.ww/host-upgrade-retained.json').write_bytes(b'original tuple')
        (self.store / 'proofs/proof').write_bytes(b'original signed proof')
        self.stage = self.store / 'stage'
        self.before = MODULE.durable_snapshot(self.home)

    def validate(self):
        return MODULE.rollback_refusal_evidence(self.home, self.before,
            hashlib.sha256(self.source).hexdigest(), len(self.source))

    def test_only_exact_bounded_scratch_is_allowed_after_corrupt_copy_refusal(self):
        self.assertIsNone(self.validate())
        self.stage.write_bytes(self.source)
        observed = self.validate()
        self.assertEqual(observed['size'], len(self.source))
        self.assertEqual(observed['sha256'], hashlib.sha256(self.source).hexdigest())
        self.stage.write_bytes(self.source + b'extra')
        with self.assertRaisesRegex(RuntimeError, 'bounded corrupt source'):
            self.validate()
        self.stage.write_bytes(b'x' * len(self.source))
        with self.assertRaisesRegex(RuntimeError, 'bounded corrupt source'):
            self.validate()

    def test_scratch_permission_never_allows_retained_proof_journal_or_extra_changes(self):
        self.stage.write_bytes(self.source)
        for relative in ('bin/ww', 'host-upgrade-retained.json', 'host-upgrade.json',
                         'bin/.ww-host-upgrade/retained.ww', 'bin/.ww-host-upgrade/proofs/proof',
                         'bin/.ww-host-upgrade/unexpected'):
            with self.subTest(path=relative):
                path = self.home / '.ww' / relative
                saved = path.read_bytes() if path.exists() else None
                path.write_bytes(b'changed evidence')
                with self.assertRaisesRegex(RuntimeError, 'installed state or discarded evidence'):
                    self.validate()
                if saved is None:
                    path.unlink()
                else:
                    path.write_bytes(saved)
                # Restoring test input bytes changes its inode metadata. Start a
                # fresh before snapshot for the next independent corruption.
                self.stage.unlink()
                self.before = MODULE.durable_snapshot(self.home)
                self.stage.write_bytes(self.source)

    def test_stage_alias_hardlink_or_preexisting_scratch_is_refused(self):
        self.stage.symlink_to(self.retained)
        with self.assertRaises(RuntimeError): self.validate()
        self.stage.unlink()
        self.stage.symlink_to(self.store / 'proofs', target_is_directory=True)
        with self.assertRaises(RuntimeError): self.validate()
        self.stage.unlink()
        os.link(self.retained, self.stage)
        with self.assertRaises(RuntimeError): self.validate()
        self.stage.unlink()
        self.stage.write_bytes(self.source)
        self.before = MODULE.durable_snapshot(self.home)
        with self.assertRaisesRegex(RuntimeError, 'unexpected scratch'):
            self.validate()


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

    def test_candidate_fault_allows_only_scoped_stop_and_exit_signals(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runner = MODULE.Runner(root, root, 0.1)
            with self.assertRaisesRegex(RuntimeError, "candidate fault kind"):
                MODULE.CandidatePauseWatcher(runner, "a" * 64, (123, "b" * 32), fault="SIGTERM")
            watcher = MODULE.CandidatePauseWatcher(runner, "a" * 64, (123, "b" * 32), fault="failed-start")
            self.assertEqual(watcher.fault_signal, signal.SIGKILL)
            self.assertTrue((root / "failed-start-watcher").is_dir())

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

    @unittest.skipUnless(sys.platform == "linux" and hasattr(os, "pidfd_open")
                         and hasattr(signal, "pidfd_send_signal"), "Linux pidfd signal support required")
    def test_candidate_exit_signal_kills_only_the_verified_process(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        try:
            expected = MODULE.sha256(Path(f"/proc/{child.pid}/exe"))
            receipt = MODULE.pause_verified_process(child.pid, expected, lambda *_: None,
                time.monotonic() + 5, fault_signal=signal.SIGKILL)
            self.assertEqual(child.wait(timeout=2), -signal.SIGKILL)
            self.assertEqual(receipt["signal"], "SIGKILL")
        finally:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=2)

    def test_readiness_requires_http_success_ready_body_and_bounded_output(self):
        class Stream:
            def __init__(self, raw): self.raw = raw
            def __enter__(self): return self
            def __exit__(self, *_): pass
            def settimeout(self, value): pass
            def sendall(self, data):
                self.request = data
            def recv(self, count):
                part, self.raw = self.raw[:count], self.raw[count:]
                return part
        for status, body, accepted in [(200, b'{"ready":true,"phase":"ready"}', True),
                                      (503, b'{"ready":true,"phase":"ready"}', False),
                                      (200, b'{"ready":false,"phase":"starting"}', False),
                                      (200, b'x' * (MODULE.MAX_RECORD + 1), False)]:
            with self.subTest(status=status, body=body[:60]):
                stream = Stream(f'HTTP/1.1 {status} Test\r\nContent-Length: {len(body)}\r\n\r\n'.encode() + body)
                with patch.object(MODULE.socket, 'create_connection', return_value=stream):
                    if accepted:
                        value = MODULE.probe_ready('http://127.0.0.1:1234', time.monotonic() + 1)
                        self.assertTrue(value['body']['ready'])
                        self.assertIn(b'GET /readyz HTTP/1.1', stream.request)
                    else:
                        with self.assertRaises(RuntimeError):
                            MODULE.probe_ready('http://127.0.0.1:1234', time.monotonic() + 1)
        class Drip(Stream):
            def recv(self, count):
                time.sleep(0.02)
                return b'x'
        start = time.monotonic()
        with patch.object(MODULE.socket, 'create_connection', return_value=Drip(b'')):
            with self.assertRaisesRegex(RuntimeError, 'readiness deadline'):
                MODULE.probe_ready('http://127.0.0.1:1234', start + 0.05)
        self.assertLess(time.monotonic() - start, 0.15)

    def test_v3_journal_requires_explicit_native_discriminator(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            (home / '.ww').mkdir()
            path = home / '.ww/host-upgrade.json'
            transaction = {'phase': 'Starting', 'operation': 'Upgrade'}
            for value, accepted in [({'version': 3, 'work': {'kind': 'native', 'transaction': transaction}}, True),
                                    ({'version': 2, 'work': {'kind': 'native', 'transaction': transaction}}, False),
                                    ({'version': 3, 'work': {'kind': 'baseline', 'transaction': transaction}}, False)]:
                path.write_text(json.dumps(value))
                if accepted:
                    self.assertEqual(MODULE.pending_native(home), transaction)
                else:
                    with self.assertRaises(RuntimeError):
                        MODULE.pending_native(home)

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
