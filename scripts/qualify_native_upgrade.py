#!/usr/bin/env python3
"""Run the installed native CLI in a separately provisioned disposable account.

The account must be named ww739-* and have a root-owned qualification marker at
/run/ww739-qualification/<uid>.json. The marker supplies independently checked
producer receipts and bootstrap paths. This runner neither signs nor publishes.
It never substitutes an implementation of ServiceControl.
"""

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import platform
import pwd
import re
import selectors
import select
import signal
import socket
import stat
import struct
import subprocess
import time
from urllib.parse import urlsplit


TARGETS = {
    "linux-x86_64": ("Linux", "x86_64", "x86_64-unknown-linux-gnu"),
    "linux-musl-x86_64": ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
    "linux-aarch64": ("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
    "macos-x86_64": ("Darwin", "x86_64", "x86_64-apple-darwin"),
    "macos-aarch64": ("Darwin", "arm64", "aarch64-apple-darwin"),
}
MAX_RECORD = 64 * 1024
MAX_COMMAND_OUTPUT = 4 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def absolute_path(value):
    path = Path(value)
    require(path.is_absolute() and ".." not in path.parts
            and not any(ord(char) < 32 for char in str(path)),
            "path must be absolute without .. or control characters")
    return path


def custody(path, owner, directory=False):
    info = path.lstat()
    require(info.st_uid == owner and not info.st_mode & 0o022,
            f"path has another owner or permits other-user writes: {path}")
    require(stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode),
            f"path has the wrong type or is a symlink: {path}")
    if not directory:
        require(info.st_nlink == 1, f"path has hardlinks: {path}")
    return info


def record(path, owner):
    custody(path, owner)
    with path.open("rb") as stream:
        raw = stream.read(MAX_RECORD + 1)
    require(len(raw) <= MAX_RECORD, f"record exceeds limit: {path}")
    return json.loads(raw)


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate_producer(receipt, target):
    require(set(receipt) == {"path", "sha256", "revision", "run_id", "attempt",
                             "archive_sha256", "builder"}, "unknown or missing producer receipt field")
    require(re.fullmatch(r"[0-9a-f]{40}", receipt["revision"]), "invalid producer revision")
    for key in ("sha256", "archive_sha256"):
        require(re.fullmatch(r"[0-9a-f]{64}", receipt[key]), f"invalid producer {key}")
    require(type(receipt["run_id"]) is int and receipt["run_id"] > 0
            and type(receipt["attempt"]) is int and receipt["attempt"] > 0,
            "producer run and attempt must be positive integers")
    expected = f'github-actions:{receipt["run_id"]}:{receipt["attempt"]}:{target}'
    require(receipt["builder"] == expected, "source fixture is not an original producer artifact")
    return absolute_path(receipt["path"])


def validate_arguments(args):
    require(0 < args.timeout_secs <= 3600, "timeout must be positive and at most 3600 seconds")
    require(args.release_a != args.release_b, "qualification requires two different release roots")
    for root in (args.release_a, args.release_b):
        require(re.fullmatch(r"b[a-z2-7]+", root), "release must be an explicit canonical CIDv1 locator")
    endpoint = urlsplit(args.ipfs_url)
    require(endpoint.scheme == "http" and endpoint.hostname in ("127.0.0.1", "::1")
            and endpoint.port and endpoint.username is None and endpoint.password is None
            and endpoint.path in ("", "/") and not endpoint.query and not endpoint.fragment,
            "qualification requires an explicit loopback Kubo endpoint")
    expected_os, expected_arch, target = TARGETS[args.platform]
    require(platform.system() == expected_os and platform.machine() == expected_arch,
            "qualification must run on the selected native platform")
    require(args.service_manager == ("systemd" if expected_os == "Linux" else "launchd"),
            "service manager does not match platform")
    require(expected_os == "Linux", "macOS exact running-executable evidence remains unqualified")
    uid = os.geteuid()
    account = pwd.getpwuid(uid)
    home = absolute_path(args.home)
    require(uid >= 1000 and account.pw_name.startswith("ww739-")
            and home == Path(account.pw_dir) and home == home.resolve(strict=True),
            "qualification requires a dedicated ww739-* account and its exact HOME")
    custody(home, uid, directory=True)
    marker_directory = Path("/run/ww739-qualification")
    custody(marker_directory, 0, directory=True)
    marker = record(marker_directory / f"{uid}.json", 0)
    require(set(marker) == {"version", "purpose", "uid", "home", "platform",
                            "release_a", "release_b", "a", "b"}, "invalid qualification marker fields")
    require(marker["version"] == 1 and marker["purpose"] == "disposable-native-upgrade"
            and marker["uid"] == uid and marker["home"] == str(home)
            and marker["platform"] == args.platform
            and marker["release_a"] == args.release_a and marker["release_b"] == args.release_b,
            "qualification marker does not authorize this disposable fixture")
    for name in ("a", "b"):
        path = validate_producer(marker[name], target)
        custody(path, 0)
        require(os.access(path, os.X_OK) and sha256(path) == marker[name]["sha256"],
                f"original producer executable changed: {name}")
    require(marker["a"]["sha256"] != marker["b"]["sha256"],
            "qualification requires different native bytes")
    trust = absolute_path(args.trusted_release_keys)
    require(trust == trust.resolve(strict=True), "trust path must be independently provisioned without aliases")
    trust_info = trust.lstat()
    require(trust_info.st_uid in (0, uid), "independent trust has another owner")
    custody(trust, trust_info.st_uid)
    require(0 < trust_info.st_size <= 122, "independent trust exceeds the public keyring bound")
    output = absolute_path(args.output)
    require(output != home and home in output.parents, "evidence must remain inside the disposable HOME")
    require(output.parent == output.parent.resolve(strict=True), "evidence parent has aliases")
    custody(output.parent, uid, directory=True)
    if output.exists() or output.is_symlink():
        custody(output, uid, directory=True)
        require(not any(output.iterdir()), "evidence directory must be empty")
    require(not (home / ".ww").exists() and not (home / ".ww").is_symlink(),
            "qualification requires an uninitialized disposable installation")
    unit = home / ".config/systemd/user/ww.service"
    require(not unit.exists() and not unit.is_symlink(), "qualification refuses an existing service definition")
    return home, output, trust, marker


class Runner:
    def __init__(self, home, output, timeout):
        self.home, self.output, self.timeout = home, output, timeout
        self.commands = []
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("WW_", "IPFS_"))
                    and key.lower() not in ("http_proxy", "https_proxy", "all_proxy", "no_proxy")}
        self.env["HOME"] = str(home)
        self.env["XDG_RUNTIME_DIR"] = f"/run/user/{os.geteuid()}"

    def run(self, name, argv, expected=0):
        argv = [str(item) for item in argv]
        started = time.monotonic()
        record_name = f"{len(self.commands):04d}-{name}"
        process = subprocess.Popen(argv, cwd=self.home, env=self.env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        chunks = {"stdout": bytearray(), "stderr": bytearray()}
        try:
            with selectors.DefaultSelector() as selector:
                for label in chunks:
                    stream = getattr(process, label)
                    os.set_blocking(stream.fileno(), False)
                    selector.register(stream, selectors.EVENT_READ, label)
                while selector.get_map():
                    require(time.monotonic() - started < self.timeout + 10, f"command deadline: {name}")
                    for key, _ in selector.select(0.05):
                        data = os.read(key.fileobj.fileno(), 65536)
                        if not data:
                            selector.unregister(key.fileobj)
                        else:
                            chunks[key.data].extend(data)
                            require(len(chunks[key.data]) <= MAX_COMMAND_OUTPUT, f"command output exceeds bound: {name}")
            process.wait(timeout=max(0.01, self.timeout + 10 - (time.monotonic() - started)))
        except BaseException:
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                except PermissionError as error:
                    # A process can finish between poll and the group signal.
                    # Preserve a real refusal while reaping an already exited child.
                    try:
                        process.wait(timeout=0.1)
                    except subprocess.TimeoutExpired:
                        raise error
            process.wait(timeout=5)
            raise
        finally:
            for label, data in chunks.items():
                (self.output / f"{record_name}.{label}").write_bytes(data)
            self.commands.append({"name": name, "argv": argv, "exit": process.returncode,
                                  "elapsed_seconds": time.monotonic() - started,
                                  "stdout": record_name + ".stdout", "stderr": record_name + ".stderr"})
            self.save("commands.json", self.commands)
            process.stdout.close()
            process.stderr.close()
        allowed = expected if isinstance(expected, tuple) else (expected,)
        require(process.returncode in allowed, f"unexpected exit for {name}; inspect retained output")
        return chunks["stdout"].decode("utf-8")

    def save(self, name, value):
        (self.output / name).write_text(json.dumps(value, indent=2) + "\n")


def native_tuple(home, release, receipt):
    root = home / ".ww"
    state = record(root / "bin/.ww-host-upgrade/startup-state.json", os.geteuid())
    locator = record(root / "installed-release.json", os.geteuid())
    require(state["native"]["transport_root"] == release and locator == {"version": 1, "release": release},
            "native state and compact release locator disagree")
    require(state["native"]["artifact"]["digest"] == receipt["sha256"]
            and sha256(root / "bin/ww") == receipt["sha256"]
            and state["native"]["revision"] == receipt["revision"], "installed native producer identity differs")
    require(not (root / "host-upgrade.json").exists(), "native operation left pending recovery")
    return state


def process_token(pid):
    stat_text = Path(f"/proc/{pid}/stat").read_text()
    fields = stat_text.rsplit(")", 1)[1].split()
    return [Path("/proc/sys/kernel/random/boot_id").read_text().strip(), int(fields[19])]


def recv_exact(stream, size):
    chunks = bytearray()
    while len(chunks) < size:
        part = stream.recv(size - len(chunks))
        require(part, "readiness stream ended early")
        chunks.extend(part)
    return bytes(chunks)


@contextmanager
def pinned_pid(pid):
    descriptor = os.pidfd_open(pid, 0)
    poller = select.poll()
    poller.register(descriptor, select.POLLIN)
    try:
        require(not poller.poll(0), "manager process already exited")
        yield
        require(not poller.poll(0), "manager process exited during observation")
    finally:
        os.close(descriptor)


def readiness(runner, state, name):
    deadline = time.monotonic() + runner.timeout
    last_error = "no running process"
    while time.monotonic() < deadline:
        try:
            pid = int(runner.run(name + "-pid", ["/usr/bin/systemctl", "--user", "show", "ww.service", "--property=MainPID", "--value"]).strip())
            require(pid > 0, "service has no main process")
            with pinned_pid(pid):
                token = process_token(pid)
                expected = state["native"]["artifact"]["digest"]
                require(sha256(Path(f"/proc/{pid}/exe")) == expected, "wrong running native executable")
                encoded = json.dumps(token, separators=(",", ":")).encode()
                domain = b"wetware.readiness-address.v1"
                digest = hashlib.sha256(len(domain).to_bytes(8, "big") + domain + len(encoded).to_bytes(8, "big") + encoded).hexdigest()[:32]
                path = runner.home / f".ww/bin/.ww-host-upgrade/r-{pid}-{digest}.sock"
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                    stream.settimeout(max(0.001, deadline - time.monotonic()))
                    stream.connect(str(path))
                    peer_pid, peer_uid, _ = struct.unpack("3i", stream.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                    require(peer_pid == pid and peer_uid == os.geteuid(), "readiness peer differs from manager process")
                    nonce = list(os.urandom(32))
                    request = json.dumps({"version": 1, "nonce": nonce, "launch_nonce": None,
                                          "timeout_millis": max(1, int((deadline - time.monotonic()) * 1000))}, separators=(",", ":")).encode()
                    stream.sendall(len(request).to_bytes(4, "big") + request)
                    length = int.from_bytes(recv_exact(stream, 4), "big")
                    require(0 < length <= MAX_RECORD, "readiness reply exceeds bound")
                    response = json.loads(recv_exact(stream, length))
                require(response["version"] == 1 and response["nonce"] == nonce
                        and response["artifact"] == state["native"]["artifact"], "readiness reply identifies another request or artifact")
                require(response["contract"] == state["disposition"]["contract"], "readiness contract differs from committed state")
                require(process_token(pid) == token and sha256(Path(f"/proc/{pid}/exe")) == expected,
                        "process changed across readiness observation")
                require(response["serving_ready"] and response["effective_root"]
                        and response["kernel_source"] and response["kernel_runtime"], "Host is not serving ready")
                runner.save(name + "-readiness.json", {"pid": pid, "process_start": token, "response": response})
                return response
        except (RuntimeError, OSError, KeyError, ValueError) as error:
            last_error = str(error)
            time.sleep(min(0.1, max(0, deadline - time.monotonic())))
    raise RuntimeError(f"readiness deadline: {last_error}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=TARGETS, required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--service-manager", choices=("systemd", "launchd"), required=True)
    parser.add_argument("--release-a", required=True)
    parser.add_argument("--release-b", required=True)
    parser.add_argument("--trusted-release-keys", required=True)
    parser.add_argument("--ipfs-url", required=True)
    parser.add_argument("--timeout-secs", type=int, required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    home, output, trust, marker = validate_arguments(args)
    output.mkdir(mode=0o700, exist_ok=True)
    os.chmod(output, 0o700)
    runner = Runner(home, output, args.timeout_secs)
    report = {"version": 1, "platform": args.platform, "status": "RUNNING", "checks": [],
              "producer_receipts": {key: marker[key] for key in ("a", "b")},
              "trust_sha256": sha256(trust), "production_activation": False,
              "remaining": ["native crash boundaries", "failed start and readiness recovery",
                            "compatibility refusal", "enabled and absent service cases", "login/boot persistence"]}
    runner.save("report.json", report)
    unit = home / ".config/systemd/user/ww.service"
    owned_unit = None
    try:
        load = runner.run("initial-service", ["/usr/bin/systemctl", "--user", "show", "ww.service", "--property=LoadState", "--value"], expected=(0, 4)).strip()
        require(load == "not-found", "qualification refuses an existing loaded service")
        common = ["--ipfs-url", args.ipfs_url, "--trusted-release-keys", trust]
        binary = home / ".ww/bin/ww"
        runner.run("install-a", [marker["a"]["path"], "install", "--release", args.release_a, *common, "--json"])
        initial = native_tuple(home, args.release_a, marker["a"])
        require(initial["disposition"]["kind"] == "required-unbound", "fresh installation is already admitted")
        report["checks"].append("authenticated fresh A remains unbound")
        private = home / ".ww/data/native-qualification-sentinel"
        private.parent.mkdir(mode=0o700, exist_ok=True)
        private.write_bytes(os.urandom(32))
        preserved = [home / ".ww/identity", home / ".ww/fhs/boot/kernel.wasm", home / ".ww/fhs/bin/status.wasm", private]
        hashes = {str(path): sha256(path) for path in preserved}
        runner.run("daemon-register", [binary, "daemon", "install", "--executor-threads", "2", "--listen", "/ip4/127.0.0.1/tcp/0", "--with-http-admin", "off", "--ipfs-url", args.ipfs_url, "--json"])
        original_unit = unit.read_text()
        require(original_unit.count("[Service]\n") == 1, "unexpected generated service definition")
        owned_unit = original_unit.replace("[Service]\n", "[Service]\nEnvironment=WW_COMPILE_WORKERS=2\nWorkingDirectory=" + str(home) + "\n").encode()
        unit.write_bytes(owned_unit)
        runner.run("daemon-reload", ["/usr/bin/systemctl", "--user", "daemon-reload"])
        runner.run("admit-a", [binary, "startup", "admit", "--timeout-secs", args.timeout_secs, "--json", "service"])
        state = native_tuple(home, args.release_a, marker["a"])
        runner.run("start-a", ["/usr/bin/systemctl", "--user", "start", "ww.service"])
        readiness(runner, state, "a")
        report["checks"].append("actual systemd A starts with HTTP admin disabled")
        runner.run("upgrade-a-b", [binary, "upgrade", "--release", args.release_b, *common, "--timeout-secs", args.timeout_secs, "--json"])
        state = native_tuple(home, args.release_b, marker["b"])
        readiness(runner, state, "b")
        report["checks"].append("actual changed A to B commits matching provenance and readiness")
        runner.run("rollback-b-a", [binary, "upgrade", "--rollback", "--timeout-secs", args.timeout_secs, "--json"])
        state = native_tuple(home, args.release_a, marker["a"])
        readiness(runner, state, "restored-a")
        report["checks"].append("actual retained rollback restores A with measured readiness")
        before_noop = json.dumps(state, sort_keys=True)
        noop = json.loads(runner.run("exact-noop", [binary, "upgrade", "--release", args.release_a, *common, "--json"]))
        require(noop["outcome"]["kind"] == "already-installed" and noop["changed_paths"] == [] and noop["activation"] is False, "exact no-op changed software or activated service")
        require(json.dumps(native_tuple(home, args.release_a, marker["a"]), sort_keys=True) == before_noop, "exact no-op changed startup state")
        runner.run("stop-before-stopped-upgrade", ["/usr/bin/systemctl", "--user", "stop", "ww.service"])
        result = json.loads(runner.run("stopped-upgrade", [binary, "upgrade", "--release", args.release_b, *common, "--timeout-secs", args.timeout_secs, "--json"]))
        native_tuple(home, args.release_b, marker["b"])
        require(result["activation"] is False, "stopped upgrade activated service")
        require(runner.run("stopped-pid", ["/usr/bin/systemctl", "--user", "show", "ww.service", "--property=MainPID", "--value"]).strip() == "0", "stopped service started during upgrade")
        require(unit.read_bytes() == owned_unit and all(sha256(Path(path)) == value for path, value in hashes.items()), "native operation changed preserved configuration, identity, FHS or private state")
        report["checks"].extend(["exact-artifact no-op preserves provenance", "stopped service remains stopped", "custom configuration, identity, FHS and private bytes preserved"])
        # Core-path success cannot claim the remaining failure/platform matrix.
        report["status"] = "PARTIAL"
    except BaseException as error:
        report["status"] = "FAILED"
        report["error"] = str(error)
        raise
    finally:
        try:
            if owned_unit is not None:
                require(unit.read_bytes() == owned_unit, "cleanup refuses a changed service definition")
                runner.run("cleanup-stop", ["/usr/bin/systemctl", "--user", "stop", "ww.service"])
                runner.run("cleanup-disable", ["/usr/bin/systemctl", "--user", "disable", "ww.service"])
                unit.unlink()
                runner.run("cleanup-reload", ["/usr/bin/systemctl", "--user", "daemon-reload"])
                report["cleanup"] = "fixture service stopped and removed; installation evidence retained"
        finally:
            report["executed_checks"] = len(report["checks"])
            runner.save("report.json", report)


if __name__ == "__main__":
    main()
