#!/usr/bin/env python3
"""Executable installer regressions. Requires WW_TEST_BINARY and WW_TEST_FHS."""
import os
import json
import plistlib
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class HostInstall(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name) / "home"
        self.home.mkdir()
        self.binary = Path(os.environ["WW_TEST_BINARY"]).resolve()
        self.payload = Path(os.environ["WW_TEST_FHS"]).resolve()
        self.destination = self.home / ".ww/bin/ww"
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("WW_")}
        self.env.update(HOME=str(self.home), IPFS_API=os.environ["IPFS_API"],
                        WW_CWASM_DIR=str(Path(self.tmp.name) / "cwasm"))

    def install(self, *flags, binary=None):
        return subprocess.run([str(binary or self.binary), "perform", "install", "--payload",
            str(self.payload), "--executable", str(self.destination), *flags],
            env=self.env, capture_output=True, text=True, timeout=120)

    def assert_installed(self):
        self.assertTrue((self.home / ".ww/identity").is_file())
        self.assertTrue(self.destination.is_file())
        self.assertFalse(self.destination.is_symlink())
        for name in ("boot/kernel.wasm", "bin/status.wasm"):
            self.assertEqual((self.home / ".ww/fhs" / name).read_bytes(),
                             (self.payload / name).read_bytes())
        service = self.home / ("Library/LaunchAgents/io.wetware.ww.plist" if os.uname().sysname == "Darwin"
                               else ".config/systemd/user/ww.service")
        text = service.read_text()
        self.assertIn(str(self.destination), text)
        self.assertIn(str(self.home / ".ww/fhs"), text)
        self.assertIn("--namespace-root", text)
        self.assertNotIn(str(self.binary), text) if self.binary != self.destination else None
        result = subprocess.run([str(self.destination), "--version"], env=self.env, capture_output=True)
        self.assertEqual(result.returncode, 0)

    def test_fresh_precreated_bin_initializes_and_reruns(self):
        self.destination.parent.mkdir(parents=True)
        check = self.install("--check")
        self.assertEqual(check.returncode, 0, check.stderr)
        self.assertFalse((self.home / ".ww/identity").exists())
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()
        identity = (self.home / ".ww/identity").read_bytes()
        state = self.home / ".ww/ipns-watermark"
        state.write_bytes(b"durable authority")
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.home / ".ww/identity").read_bytes(), identity)
        self.assertEqual(state.read_bytes(), b"durable authority")

    def test_same_path_executable_stays_regular(self):
        self.destination.parent.mkdir(parents=True)
        shutil.copy2(self.binary, self.destination)
        result = self.install(binary=self.destination)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()

    def test_malformed_identity_is_not_replaced(self):
        identity = self.home / ".ww/identity"
        identity.parent.mkdir()
        identity.write_bytes(b"malformed identity")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("identity", result.stderr)
        self.assertEqual(identity.read_bytes(), b"malformed identity")
        self.assertFalse(self.destination.exists())

    def test_unknown_payload_collision_refuses_before_executable(self):
        kernel = self.home / ".ww/fhs/boot/kernel.wasm"
        kernel.parent.mkdir(parents=True)
        kernel.write_bytes(b"custom kernel")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("custom", result.stderr.lower())
        self.assertEqual(kernel.read_bytes(), b"custom kernel")
        self.assertFalse(self.destination.exists())
        self.assertFalse((self.home / ".ww/identity").exists())

    def test_valid_existing_identity_is_preserved(self):
        identity = self.home / ".ww/identity"
        identity.parent.mkdir()
        generated = subprocess.run([str(self.binary), "keygen", "--output", str(identity)],
                                   env=self.env, capture_output=True)
        self.assertEqual(generated.returncode, 0, generated.stderr)
        original = identity.read_bytes()
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(identity.read_bytes(), original)

    def test_unknown_binary_is_not_replaced(self):
        self.destination.parent.mkdir(parents=True)
        self.destination.write_bytes(b"custom executable")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.destination.read_bytes(), b"custom executable")
        self.assertFalse((self.home / ".ww/identity").exists())

    def test_symlinked_destination_directory_is_not_followed(self):
        outside = Path(self.tmp.name) / "outside"
        outside.mkdir()
        (self.home / ".ww").symlink_to(outside, target_is_directory=True)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(outside.iterdir()), [])

    def test_dangling_identity_is_not_regenerated(self):
        identity = self.home / ".ww/identity"
        identity.parent.mkdir()
        identity.symlink_to("missing-private-key")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(identity.is_symlink())
        self.assertFalse((identity.parent / "missing-private-key").exists())

    def test_custom_service_refuses_and_preserves_private_state(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        identity = (self.home / ".ww/identity").read_bytes()
        service = self.home / ("Library/LaunchAgents/io.wetware.ww.plist" if os.uname().sysname == "Darwin"
                               else ".config/systemd/user/ww.service")
        service.write_text("custom --kernel /opt/kernel.wasm WW_KERNEL=custom")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("custom", result.stderr.lower())
        self.assertIn("--kernel", service.read_text())
        self.assertEqual((self.home / ".ww/identity").read_bytes(), identity)

    def test_incomplete_managed_install_recovers_by_rerun(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = self.home / ".ww/installation.json"
        state = json.loads(metadata.read_text())
        state["complete"] = False
        metadata.write_text(json.dumps(state))
        (self.home / ".ww/fhs/bin/status.wasm").unlink()
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()
        self.assertTrue(json.loads(metadata.read_text())["complete"])

    def test_pending_payload_update_reindexes_surviving_old_namespace(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = self.home / ".ww/installation.json"
        namespace = self.home / ".ww/etc/ns/ww"
        old_state = json.loads(metadata.read_text())
        old_namespace = namespace.read_bytes()
        updated = Path(self.tmp.name) / "updated-fhs"
        shutil.copytree(self.payload, updated)
        status = updated / "bin/status.wasm"
        status.write_bytes(status.read_bytes() + b"\x00\x05\x04newx")
        self.payload = updated
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        expected_namespace = namespace.read_bytes()
        self.assertNotEqual(expected_namespace, old_namespace)
        pending = json.loads(metadata.read_text())
        pending["complete"] = False
        pending["previous"] = old_state["hashes"]
        metadata.write_text(json.dumps(pending))
        namespace.write_bytes(old_namespace)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(namespace.read_bytes(), expected_namespace)
        # An omitted payload must retain this explicit package over embedded seeds.
        result = subprocess.run([str(self.binary), "perform", "install"], env=self.env,
                                capture_output=True, text=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.home / ".ww/fhs/bin/status.wasm").read_bytes(), status.read_bytes())

    def test_changed_routing_endpoint_refuses_before_service_replacement(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        service = self.home / ("Library/LaunchAgents/io.wetware.ww.plist" if os.uname().sysname == "Darwin"
                               else ".config/systemd/user/ww.service")
        original = service.read_bytes()
        self.env["IPFS_ROUTING_API"] = "http://127.0.0.1:1"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Gateway.ExposeRoutingAPI", result.stderr)
        self.assertEqual(service.read_bytes(), original)

    def test_unreachable_routing_refuses_before_private_or_managed_writes(self):
        self.env["IPFS_ROUTING_API"] = "http://127.0.0.1:1"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Gateway.ExposeRoutingAPI", result.stderr)
        self.assertFalse((self.home / ".ww").exists())

    def test_unreachable_kubo_refuses_before_private_or_managed_writes(self):
        self.env["IPFS_API"] = "http://127.0.0.1:1"
        check = self.install("--check")
        self.assertEqual(check.returncode, 0, check.stderr)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Kubo", result.stderr)
        self.assertFalse((self.home / ".ww").exists())

    def test_invalid_payload_does_not_replace_installed_files(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        before = self.destination.read_bytes()
        bad = Path(self.tmp.name) / "bad-fhs"
        shutil.copytree(self.payload, bad)
        (bad / "boot/kernel.wasm").write_bytes(b"corrupt wasm")
        self.payload = bad
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.destination.read_bytes(), before)

    def test_missing_owned_identity_refuses_regeneration(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        identity = self.home / ".ww/identity"
        identity.unlink()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(identity.exists())

    def test_home_spaces_and_xml_characters_preserve_service_argv(self):
        home = self.home.with_name("home space & chars")
        self.home.rename(home)
        self.home = home
        self.destination = self.home / ".ww/bin/ww"
        self.env["HOME"] = str(home)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        if os.uname().sysname == "Darwin":
            self.assertIn('launchctl load "$HOME/Library/LaunchAgents/io.wetware.ww.plist"', result.stdout)
            service = self.home / "Library/LaunchAgents/io.wetware.ww.plist"
            argv = plistlib.loads(service.read_bytes())["ProgramArguments"]
            self.assertEqual(argv[0], str(self.destination))
            self.assertIn(str(self.home / ".ww/fhs"), argv)
            self.assertIn(str(self.home / ".ww/identity"), argv)

    def test_custom_systemd_dropin_refuses_before_writes(self):
        custom = self.home / ".config/systemd/user/ww.service.d/kernel.conf"
        custom.parent.mkdir(parents=True)
        custom.write_text("Environment=WW_KERNEL=custom")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.destination.exists())
        self.assertEqual(custom.read_text(), "Environment=WW_KERNEL=custom")

    def test_upgrade_disabled_without_network_or_mutation(self):
        result = subprocess.run([str(self.binary), "perform", "upgrade", "--ipfs-url",
                                 "http://127.0.0.1:1"], env=self.env, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("disabled", result.stderr)
        self.assertIn("installer", result.stderr)
        self.assertEqual(list(self.home.iterdir()), [])


if __name__ == "__main__":
    from kubo_fixture import isolated_kubo
    with isolated_kubo() as endpoints:
        os.environ.update(endpoints)
        unittest.main()
