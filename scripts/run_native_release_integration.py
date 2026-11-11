#!/usr/bin/env python3
"""Offline disposable-account helpers; live orchestration is unavailable in PR 2.

Package fixtures use ephemeral keys, never publication or production credentials.
Account creation and live qualification require the reviewed PR 3 implementation.
"""

import json
import os
from pathlib import Path
import pwd
import shutil
import signal
import stat
import subprocess
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
from release_auth_fixture import generate_key, sign_manifest
from qualify_native_upgrade import Runner, custody, require, sha256, unavailable

MAX_EVIDENCE = 256 * 1024 * 1024


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")
    path.chmod(0o644)


def create_marker(path, value):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
    try:
        with os.fdopen(descriptor, "w") as stream:
            stream.write(json.dumps(value, indent=2) + "\n")
            stream.flush()
            os.fsync(stream.fileno())
            info = os.fstat(stream.fileno())
        return (info.st_dev, info.st_ino, sha256(path))
    except BaseException:
        # The newly created file belongs to this invocation, even if writing failed.
        path.unlink()
        raise


def regular_tree(source, destination):
    """Copy only regular files/directories, with fresh inodes and root custody."""
    require(source.is_dir() and not source.is_symlink(), "invalid source directory")
    destination.mkdir(mode=0o755)
    for path in sorted(source.iterdir()):
        if path.name == "__pycache__":
            continue
        info = path.lstat()
        target = destination / path.name
        if stat.S_ISDIR(info.st_mode):
            regular_tree(path, target)
        else:
            require(stat.S_ISREG(info.st_mode), f"nonregular input: {path}")
            shutil.copyfile(path, target)
            target.chmod(0o755 if info.st_mode & 0o111 else 0o644)


def copy_evidence(source, destination):
    """Refuse symlinks and bound the artifact retained from the fixture user."""
    require(stat.S_ISDIR(source.lstat().st_mode) and source == source.resolve(strict=True),
            "evidence root must be a canonical ordinary directory")
    files, total = [], 0
    for path in sorted(source.rglob("*")):
        info = path.lstat()
        require(stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode),
                f"nonregular evidence: {path}")
        if stat.S_ISREG(info.st_mode):
            total += info.st_size
            files.append(path)
    require(len(files) <= 4096 and total <= MAX_EVIDENCE, "evidence exceeds bounds")
    destination.mkdir(mode=0o755)
    receipts = {}
    copied = 0
    for path in files:
        target = destination / path.relative_to(source)
        target.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(descriptor, "rb") as stream, target.open("xb") as sink:
            require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), "evidence file type changed")
            while chunk := stream.read(1024 * 1024):
                copied += len(chunk)
                require(copied <= MAX_EVIDENCE, "opened evidence exceeds bound")
                sink.write(chunk)
        target.chmod(0o644)
        receipts[str(target.relative_to(destination))] = sha256(target)
    return receipts


def verify_fhs(inputs, record):
    for label in ("a", "b"):
        expected = record["producer_evidence"][label]["fhs"]
        require(set(expected) == {"fhs/boot/kernel.wasm", "fhs/bin/status.wasm"},
                "original FHS member set differs")
        original = inputs / label
        actual = {str(p.relative_to(original)) for p in (original / "fhs").rglob("*") if p.is_file()}
        require(actual == set(expected), "copied FHS member set differs")
        for name, receipt in expected.items():
            path = original / name
            info = custody(path, 0)
            require(info.st_size == receipt["size"] and sha256(path) == receipt["sha256"],
                    "copied original FHS differs: " + name)


def quiesce_account(account):
    """Reap detached fixture sessions before copying evidence or releasing its UID."""
    killed = []
    deadline = time.monotonic() + 15
    while True:
        require(pwd.getpwnam(account.pw_name) == account, "fixture account identity changed")
        found = False
        for path in Path("/proc").iterdir():
            if not path.name.isdigit():
                continue
            try:
                uids = next(line for line in (path / "status").read_text().splitlines()
                            if line.startswith("Uid:")).split()[1:]
                if not any(int(uid) == account.pw_uid for uid in uids):
                    continue
                found = True
                descriptor = os.pidfd_open(int(path.name))
                try:
                    # Verify identity again after obtaining the stable process handle.
                    current = next(line for line in (path / "status").read_text().splitlines()
                                   if line.startswith("Uid:")).split()[1:]
                    require(uids == current, "fixture process UID changed during cleanup")
                    signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                    killed.append(int(path.name))
                finally:
                    os.close(descriptor)
            except (FileNotFoundError, ProcessLookupError):
                continue
        if not found:
            return killed
        require(time.monotonic() < deadline, "fixture account still has processes after cleanup")
        time.sleep(0.1)


class Provisioner:
    def __init__(self, output):
        self.output = output
        self.runner = Runner(output, output, 90)
        self.runner.env = {"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "LANG": "C.UTF-8"}
        self.deadline = time.monotonic() + 55 * 60

    def run(self, name, argv, timeout=90, check=True):
        # Reuse the existing live byte limit, deadline and owned-group reaper.
        self.runner.timeout = timeout
        try:
            self.runner.run(name, argv, expected=0 if check else tuple(range(-64, 256)),
                            deadline=min(self.deadline, time.monotonic() + timeout))
        finally:
            write_json(self.output / "provision-commands.json", self.runner.commands)
        receipt = self.runner.commands[-1]
        return subprocess.CompletedProcess(argv, receipt["exit"],
            (self.output / receipt["stdout"]).read_bytes(),
            (self.output / receipt["stderr"]).read_bytes())

    def packages(self, inputs, output, tool):
        output.mkdir(mode=0o755)
        private, trust = generate_key(output / "signer")
        public = output / "trusted-keys"
        shutil.copyfile(trust, public)
        public.chmod(0o644)
        # Valid fixture channel, unrelated to any published release name.
        value = int.from_bytes(bytes.fromhex("0172002408011220") + os.urandom(32), "big")
        channel = ""
        while value:
            value, digit = divmod(value, 36)
            channel = "0123456789abcdefghijklmnopqrstuvwxyz"[digit] + channel
        receipt = {"channel": "k" + channel, "publication": False,
                   "signing_authority": "ephemeral disposable fixture only",
                   "package_tool_sha256": sha256(tool), "trust_sha256": sha256(public)}
        try:
            for sequence, label in enumerate(("a", "b"), 1):
                folder = output / label
                folder.mkdir(mode=0o755)
                tree = folder / "tree"
                tree.mkdir(mode=0o755)
                original = inputs / label
                regular_tree(original / "fhs", tree / "fhs")
                binary = tree / "bin/ww/linux/aarch64/ww"
                binary.parent.mkdir(parents=True)
                shutil.copyfile(original / "ww", binary)
                binary.chmod(0o755)
                provenance = json.loads((original / "PROVENANCE.json").read_text())
                (tree / "VERSION").write_text("0.1.1\n")
                (tree / "REVISION").write_text(provenance["revision"] + "\n")
                metadata = {"channel": receipt["channel"], "sequence": sequence,
                            "version": "0.1.1", "source": provenance,
                            "hosts": {"aarch64-unknown-linux-gnu": provenance}}
                write_json(folder / "metadata.json", metadata)
                self.run(label + "-prepare", [tool, "prepare", "--tree", tree,
                         "--spool", folder / "spool", "--metadata", folder / "metadata.json"])
                sign_manifest(private, tree / "CHECKSUMS.txt")
                identity = json.loads(self.run(label + "-finalize", [tool, "finalize",
                    "--tree", tree, "--spool", folder / "spool", "--trusted-keys", public]).stdout)
                self.run(label + "-export", [tool, "export-car", "--spool", folder / "spool",
                                            "--car", folder / "release.car"])
                self.run(label + "-import", [tool, "import-car", "--car", folder / "release.car",
                                            "--spool", folder / "verification"])
                verified = json.loads(self.run(label + "-verify", [tool, "verify", "--spool",
                    folder / "verification", "--trusted-keys", public]).stdout)
                require(identity == verified, "package roundtrip identity differs")
                require(sha256(binary) == sha256(original / "ww"), "packaged Host changed")
                receipt["release_" + label] = identity["root"]
                receipt[label] = {"identity": identity, "host_sha256": sha256(binary),
                                  "car_sha256": sha256(folder / "release.car"),
                                  "provenance": provenance}
        finally:
            private.unlink(missing_ok=True)
        receipt["private_signer_removed"] = not private.exists()
        write_json(output / "packages.json", receipt)
        # Finalized files are public fixture inputs; private signer is already gone.
        for path in output.rglob("*"):
            path.chmod(0o755 if path.is_dir() else 0o644)
        return receipt


def main():
    # PR 3 must restore orchestration with the reviewed runtime and CI policy.
    return unavailable()


if __name__ == "__main__":
    raise SystemExit(main())
