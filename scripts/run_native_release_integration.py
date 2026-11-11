#!/usr/bin/env python3
"""Provision one disposable GNU ARM64 integration account on a systemd VM.

Run only on a disposable runner. This script creates its own account and user
manager. Original producer inputs must first pass verify_native_release_inputs.
It uses ephemeral envelope keys, never publication or production credentials.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import pwd
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import uuid

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
from release_auth_fixture import generate_key, sign_manifest
from qualify_native_upgrade import Runner, custody, require, sha256, verify_producer_files
from prepare_current_native_release import POLICY as CURRENT_POLICY, verify_build_files

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
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--package-tool", type=Path, required=True)
    parser.add_argument("--ipfs", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout-secs", type=int, required=True)
    parser.add_argument("--disposable-runner", action="store_true", required=True)
    args = parser.parse_args()
    inherited_umask = os.umask(0o022)
    require(os.geteuid() == 0 and platform.system() == "Linux"
            and platform.machine() == "aarch64", "requires root on disposable native GNU ARM64")
    require(Path("/proc/1/comm").read_text().strip() == "systemd", "requires actual systemd PID 1")
    require(0 < args.timeout_secs <= 120, "explicit native operation budget must be 1..120 seconds")
    for path in (args.inputs, args.package_tool, args.ipfs):
        require(path.is_absolute() and path.resolve(strict=True) == path, "inputs must be canonical absolute paths")
    require(args.output.is_absolute() and not args.output.exists(), "output must be a new absolute directory")
    args.output.mkdir(mode=0o755, parents=True)
    worker = Provisioner(args.output)
    # The unchanged native readiness address must fit sockaddr_un (<104 bytes).
    # Root owns this short parent; only h/ and t/ are writable by the fixture UID.
    work = Path(tempfile.mkdtemp(prefix="w", dir="/tmp"))
    work.chmod(0o755)
    username = "ww739-" + uuid.uuid4().hex[:12]
    account = None
    marker = None
    marker_identity = None
    user_output = None
    report = {"status": "FAILED", "native_acceptance": False,
              "inherited_umask": oct(inherited_umask), "fixture_setup_umask": "0o022",
              "wrong_executable_interception": "UNQUALIFIED; not attempted", "work": str(work)}
    try:
        code = work / "code"
        code.mkdir(mode=0o755)
        checkout = Path(__file__).resolve().parents[1]
        for folder in ("scripts", "tests"):
            regular_tree(checkout / folder, code / folder)
        inputs = work / "inputs"
        regular_tree(args.inputs, inputs)
        record = json.loads((inputs / "verified-inputs.json").read_text())
        for label in ("a", "b"):
            record[label]["path"] = str(inputs / label / "ww")
            record[label]["archive_path"] = str(inputs / (label + ".zip"))
            verify_producer_files(record[label], "aarch64-unknown-linux-gnu", 0)
        write_json(inputs / "verified-inputs.json", record)
        verify_fhs(inputs, record)
        if record.get("policy") == CURRENT_POLICY:
            require(verify_build_files(inputs / "b", "release", owner=0) == record["current_build"],
                    "copied current build receipt differs from verified input")
        tools = work / "tools"
        tools.mkdir(mode=0o755)
        for source, name in ((args.package_tool, "ww-release-package"), (args.ipfs, "ipfs")):
            shutil.copyfile(source, tools / name)
            (tools / name).chmod(0o755)
            require(sha256(source) == sha256(tools / name), "tool copy changed")
        packages = worker.packages(inputs, work / "packages", tools / "ww-release-package")
        home = work / "h"
        try:
            pwd.getpwnam(username)
        except KeyError:
            pass
        else:
            raise RuntimeError("generated fixture account already exists")
        try:
            worker.run("create-account", ["useradd", "--create-home", "--home-dir", home,
                                          "--shell", "/bin/sh", username])
        finally:
            # useradd can create passwd state before a later HOME/setup failure.
            try:
                created = pwd.getpwnam(username)
            except KeyError:
                pass
            else:
                require(created.pw_uid >= 1000 and created.pw_dir == str(home), "created account differs")
                account = created
        require(account is not None, "useradd did not create the fixture account")
        home.chmod(0o700)
        temporary_root = work / "t"
        temporary_root.mkdir(mode=0o700)
        os.chown(temporary_root, account.pw_uid, account.pw_gid)
        marker_dir = Path("/run/ww739-qualification")
        marker_dir.mkdir(mode=0o755, exist_ok=True)
        custody(marker_dir, 0, directory=True)
        marker = marker_dir / f"{account.pw_uid}.json"
        marker_identity = create_marker(marker, {"version": 1, "purpose": "disposable-native-upgrade",
            "uid": account.pw_uid, "home": str(home), "platform": "linux-aarch64",
            "release_a": packages["release_a"], "release_b": packages["release_b"],
            "a": record["a"], "b": record["b"]})
        report.update(uid=account.pw_uid, username=username, home=str(home),
                      packages=packages, producer_receipts={k: record[k] for k in ("a", "b")})
        worker.run("start-user-manager", ["systemctl", "start", f"user@{account.pw_uid}.service"])
        runtime = f"/run/user/{account.pw_uid}"
        deadline = time.monotonic() + 15
        while True:
            result = worker.run("user-bus-ready", ["runuser", "-u", username, "--", "busctl",
                "--address=unix:path=" + runtime + "/bus", "call", "org.freedesktop.DBus",
                "/org/freedesktop/DBus", "org.freedesktop.DBus", "NameHasOwner", "s",
                "org.freedesktop.systemd1"], timeout=5, check=False)
            if result.returncode == 0 and result.stdout.strip() == b"b true":
                break
            require(time.monotonic() < deadline, "user manager did not acquire its bus name")
            time.sleep(0.1)
        worker.run("environment", ["sh", "-c", "uname -a; ldd --version; systemd --version; cat /etc/os-release"])
        user_output = home / "evidence"
        command = ["runuser", "-u", username, "--", "env", "-i", "HOME=" + str(home),
            "USER=" + username, "LOGNAME=" + username, "LANG=C.UTF-8",
            "XDG_RUNTIME_DIR=" + runtime, "PATH=" + str(tools) + ":/usr/bin:/bin",
            "DBUS_SESSION_BUS_ADDRESS=unix:path=" + runtime + "/bus",
            "PYTHONDONTWRITEBYTECODE=1", "sh", "-c", 'cd "$HOME" && exec "$@"',
            "ww739-release-integration", "python3", str(code / "tests/native_release_integration.py"),
            "--inputs", str(inputs), "--packages", str(work / "packages"),
            "--package-tool", str(tools / "ww-release-package"), "--output", str(user_output),
            "--temporary-root", str(temporary_root),
            "--timeout-secs", str(args.timeout_secs)]
        result = worker.run("user-integration", command, timeout=3000, check=False)
        report["user_exit"] = result.returncode
        require(result.returncode == 0, "user integration failed; retained result remains failed")
        report["status"] = "PASS_IMPLEMENTED_INTEGRATION_CONTRACT"
    except Exception as error:
        report["error"] = str(error)
    finally:
        # Reserve a separate bounded cleanup window after the normal run budget.
        worker.deadline = time.monotonic() + 180
        cleanup = []
        if account is not None:
            try:
                require(pwd.getpwnam(username) == account, "fixture account identity changed")
                worker.run("stop-user-manager", ["systemctl", "stop", f"user@{account.pw_uid}.service"])
                report["cleanup_killed_pids"] = quiesce_account(account)
                if user_output is not None and user_output.exists():
                    report["evidence_sha256"] = copy_evidence(user_output, args.output / "user")
                worker.run("delete-account", ["userdel", username])
            except Exception as error:
                cleanup.append(str(error))
        if marker_identity is not None:
            try:
                info = custody(marker, 0)
                require((info.st_dev, info.st_ino, sha256(marker)) == marker_identity,
                        "owned qualification marker identity changed")
                marker.unlink()
            except Exception as error:
                cleanup.append(str(error))
        if not cleanup:
            shutil.rmtree(work)
        report["cleanup_errors"] = cleanup
        report["fixture_directory_removed"] = not work.exists()
        if cleanup:
            report["status"] = "FAILED"
        write_json(args.output / "provision-result.json", report)
    print(json.dumps({"status": report["status"], "result": str(args.output / "provision-result.json")}))
    return 0 if report["status"] == "PASS_IMPLEMENTED_INTEGRATION_CONTRACT" else 1


if __name__ == "__main__":
    raise SystemExit(main())
