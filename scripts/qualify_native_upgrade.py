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
import mmap
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
import threading
import time
import zipfile
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
MAX_CONTAINMENT_SECONDS = 30


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
    require(set(receipt) == {"path", "archive_path", "sha256", "revision", "run_id", "attempt",
                             "archive_sha256", "builder"}, "unknown or missing producer receipt field")
    require(re.fullmatch(r"[0-9a-f]{40}", receipt["revision"]), "invalid producer revision")
    for key in ("sha256", "archive_sha256"):
        require(re.fullmatch(r"[0-9a-f]{64}", receipt[key]), f"invalid producer {key}")
    require(type(receipt["run_id"]) is int and receipt["run_id"] > 0
            and type(receipt["attempt"]) is int and receipt["attempt"] > 0,
            "producer run and attempt must be positive integers")
    expected = f'github-actions:{receipt["run_id"]}:{receipt["attempt"]}:{target}'
    require(receipt["builder"] == expected, "source fixture is not an original producer artifact")
    absolute_path(receipt["archive_path"])
    return absolute_path(receipt["path"])


def verify_producer_files(receipt, target, owner=0):
    """Verify independently provisioned original ZIP and extracted producer files."""
    binary = validate_producer(receipt, target)
    info = custody(binary, owner)
    require(0 < info.st_size <= 1024 ** 3 and sha256(binary) == receipt["sha256"],
            "original producer executable changed")
    with binary.open("rb") as stream:
        header = stream.read(64)
        machine = 183 if target == "aarch64-unknown-linux-gnu" else 62
        require(len(header) == 64 and header[:6] == b"\x7fELF\x02\x01"
                and int.from_bytes(header[18:20], "little") == machine, "producer ELF target differs")
        with mmap.mmap(stream.fileno(), 0, access=mmap.ACCESS_READ) as data:
            require(data.find(receipt["revision"].encode()) >= 0, "producer lacks expected embedded revision bytes")
    expected = {"REVISION": (receipt["revision"] + "\n").encode(),
                "SHA256SUMS": (receipt["sha256"] + "  ww\n").encode()}
    provenance = record(binary.parent / "PROVENANCE.json", owner)
    require(provenance == {"repository": "https://github.com/wetware/ww",
                           "revision": receipt["revision"], "builder": receipt["builder"]},
            "original producer provenance differs")
    expected["PROVENANCE.json"] = (binary.parent / "PROVENANCE.json").read_bytes()
    for name, raw in expected.items():
        custody(binary.parent / name, owner)
        require((binary.parent / name).read_bytes() == raw, "original producer sidecar differs: " + name)
    archive = absolute_path(receipt["archive_path"])
    custody(archive, owner)
    require(sha256(archive) == receipt["archive_sha256"], "original producer archive digest differs")
    with zipfile.ZipFile(archive) as original:
        members = original.infolist()
        names = [member.filename for member in members]
        require(len(names) <= 8192 and len(names) == len(set(names)), "duplicate or excessive archive members")
        require(sum(member.file_size for member in members) <= 2 * 1024 ** 3, "producer archive exceeds bound")
        for member in members:
            path = Path(member.filename)
            require(not path.is_absolute() and ".." not in path.parts
                    and not any(ord(char) < 32 for char in member.filename)
                    and not stat.S_ISLNK(member.external_attr >> 16), "unsafe producer archive member")
        require({"ww", *expected}.issubset(names), "producer archive lacks original receipt files")
        require(original.getinfo("ww").file_size == info.st_size, "archive executable size differs")
        with original.open("ww") as stream:
            require(hashlib.file_digest(stream, "sha256").hexdigest() == receipt["sha256"],
                    "archive executable differs")
        for name, raw in expected.items():
            require(original.getinfo(name).file_size <= MAX_RECORD and original.read(name) == raw,
                    "archive producer sidecar differs: " + name)
    return {"size": info.st_size, "sha256": receipt["sha256"], "archive_sha256": receipt["archive_sha256"],
            "embedded_revision_bytes": receipt["revision"], "provenance": provenance}


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
    require(hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"),
            "qualification requires Linux pidfd observation and signaling")
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
        verify_producer_files(marker[name], target)
        require(os.access(path, os.X_OK), f"original producer executable is not executable: {name}")
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

    def run(self, name, argv, expected=0, deadline=None, cancelled=None):
        argv = [str(item) for item in argv]
        started = time.monotonic()
        command_budget = self.timeout + MAX_CONTAINMENT_SECONDS + 10
        deadline = min(deadline, started + command_budget) if deadline is not None else started + command_budget
        require(time.monotonic() < deadline, f"command deadline: {name}")
        require(cancelled is None or not cancelled(), f"command cancelled: {name}")
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
                    require(time.monotonic() < deadline, f"command deadline: {name}")
                    require(cancelled is None or not cancelled(), f"command cancelled: {name}")
                    for key, _ in selector.select(0.05):
                        data = os.read(key.fileobj.fileno(), 65536)
                        if not data:
                            selector.unregister(key.fileobj)
                        else:
                            chunks[key.data].extend(data)
                            require(len(chunks[key.data]) <= MAX_COMMAND_OUTPUT, f"command output exceeds bound: {name}")
            while process.poll() is None:
                require(time.monotonic() < deadline, f"command deadline: {name}")
                require(cancelled is None or not cancelled(), f"command cancelled: {name}")
                time.sleep(min(0.05, max(0, deadline - time.monotonic())))
        except BaseException:
            # Do not reap the leader before signaling its owned group. A leader
            # can exit while descendants retain the output pipes. During output
            # drain, the unreaped leader also prevents its group ID being reused.
            # In the later wait loop, cancellation is checked only while poll()
            # reports that the leader is still live.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except PermissionError as error:
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
    reference = state["native"]["proof"]
    require(reference["root"] == release and 0 < reference["size"] <= 8 * 1024 * 1024,
            "retained proof does not bind the native transport root")
    proof = root / "bin/.ww-host-upgrade/proofs" / release
    proof_info = custody(proof, os.geteuid())
    require(proof_info.st_size == reference["size"] and sha256(proof) == reference["sha256"],
            "retained native proof bytes differ from their reference")
    require(not (root / "host-upgrade.json").exists(), "native operation left pending recovery")
    return state


def durable_snapshot(home):
    """Read native acceptance files; caches and runtime sockets are not authority."""
    root = home / ".ww"
    store = root / "bin/.ww-host-upgrade"
    paths = [home / ".ww-startup.lock", root / "bin/ww", root / "installation.json",
             root / "installed-release.json", root / "host-upgrade.json", root / "host-upgrade-retained.json",
             store / "startup-state.json", store / "startup-contract.json"]
    proofs = store / "proofs"
    if proofs.exists():
        paths.extend(sorted(proofs.iterdir()))
    result = {}
    for path in paths:
        if path.exists() or path.is_symlink():
            info = custody(path, os.geteuid())
            result[str(path.relative_to(home))] = {"sha256": sha256(path), "size": info.st_size,
                "inode": info.st_ino, "mtime_ns": info.st_mtime_ns, "mode": stat.S_IMODE(info.st_mode)}
    return result


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


def acknowledge_live_reply(stream):
    # The Host keeps this CLOEXEC socket open until acknowledgement. EOF or
    # extra bytes invalidate the reply before the verifier releases the Host.
    stream.setblocking(False)
    try:
        stream.recv(1, socket.MSG_PEEK)
    except BlockingIOError:
        stream.sendall(b"\x06")
    else:
        raise RuntimeError("readiness peer closed or sent extra data before acknowledgement")


def manager_process(runner, name, deadline=None, cancelled=None):
    output = runner.run(name, ["/usr/bin/systemctl", "--user", "show", "ww.service",
                               "--property=MainPID", "--property=InvocationID"],
                        deadline=deadline, cancelled=cancelled)
    values = dict(line.split("=", 1) for line in output.splitlines() if "=" in line)
    require(set(values) == {"MainPID", "InvocationID"}
            and re.fullmatch(r"[0-9a-f]{32}", values["InvocationID"]),
            "manager did not identify a service invocation")
    pid = int(values["MainPID"])
    require(pid > 0, "service has no main process")
    return pid, values["InvocationID"]


@contextmanager
def pinned_pid(pid, require_live_after=True):
    descriptor = os.pidfd_open(pid, 0)
    poller = select.poll()
    poller.register(descriptor, select.POLLIN)
    try:
        require(not poller.poll(0), "manager process already exited")
        yield descriptor
        if require_live_after:
            require(not poller.poll(0), "manager process exited during observation")
    finally:
        os.close(descriptor)


def readiness_socket_path(home, pid, token):
    encoded = json.dumps(token, separators=(",", ":")).encode()
    domain = b"wetware.readiness-address.v1"
    digest = hashlib.sha256(len(domain).to_bytes(8, "big") + domain
                            + len(encoded).to_bytes(8, "big") + encoded).hexdigest()[:32]
    return home / f".ww/bin/.ww-host-upgrade/r-{pid}-{digest}.sock"


def pause_verified_process(pid, expected_digest, check_context, deadline, fault_signal=signal.SIGSTOP):
    """Signal only a live, same-owner process pinned before executable checks."""
    require(time.monotonic() < deadline, "candidate fault deadline expired")
    require(fault_signal in (signal.SIGSTOP, signal.SIGKILL), "unsupported candidate fault signal")
    with pinned_pid(pid, require_live_after=False) as descriptor:
        require(Path(f"/proc/{pid}").stat().st_uid == os.geteuid(), "candidate process has another owner")
        token = process_token(pid)
        require(sha256(Path(f"/proc/{pid}/exe")) == expected_digest, "candidate executable differs")
        check_context(pid, token)
        require(process_token(pid) == token
                and sha256(Path(f"/proc/{pid}/exe")) == expected_digest,
                "candidate process changed before fault injection")
        check_context(pid, token)
        require(time.monotonic() < deadline, "candidate fault deadline expired")
        signal.pidfd_send_signal(descriptor, fault_signal)
        return {"pid": pid, "process_start": token, "sha256": expected_digest,
                "signal": signal.Signals(fault_signal).name, "injected_monotonic": time.monotonic()}


class CandidatePauseWatcher:
    """Observe the owned candidate; never edit transaction state or signal by PID."""

    def __init__(self, runner, candidate_sha256, original_manager, fault="readiness-timeout"):
        require(fault in ("readiness-timeout", "failed-start"), "unsupported candidate fault kind")
        self.fault_signal = signal.SIGSTOP if fault == "readiness-timeout" else signal.SIGKILL
        output = runner.output / (fault + "-watcher")
        output.mkdir(mode=0o700)
        self.runner = Runner(runner.home, output, runner.timeout)
        self.runner.env = runner.env.copy()
        self.candidate_sha256, self.original_manager = candidate_sha256, original_manager
        self.deadline = time.monotonic() + runner.timeout
        self.cancelled = threading.Event()
        self.result = {"injected": False}
        self.thread = threading.Thread(target=self._watch, name="ww739-candidate-fault", daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.cancelled.set()
        # Every watcher command checks cancellation and reaps its child.
        self.thread.join(timeout=6)
        require(not self.thread.is_alive(), "candidate watcher did not stop within cleanup bound")

    def require_injected(self):
        require(self.result["injected"], "candidate watcher did not inject: " + self.result.get("error", "unknown"))
        return self.result

    def _journal(self):
        path = self.runner.home / ".ww/host-upgrade.json"
        try:
            journal = record(path, os.geteuid())
        except FileNotFoundError:
            return None
        require(journal["version"] == 2 and journal["operation"] == "upgrade",
                "candidate watcher found another transaction kind")
        require(journal["after"]["generation"]["artifact"]["digest"] == self.candidate_sha256,
                "candidate watcher found another target artifact")
        return journal

    def _check_context(self, pid, token, invocation, transaction_id):
        require(not self.cancelled.is_set(), "candidate watcher cancelled before injection")
        path = readiness_socket_path(self.runner.home, pid, token)
        info = path.lstat()
        require(stat.S_ISSOCK(info.st_mode) and info.st_uid == os.geteuid()
                and stat.S_IMODE(info.st_mode) == 0o600,
                "candidate readiness socket is not an owned private socket")
        require(manager_process(self.runner, "verify-manager", self.deadline, self.cancelled.is_set)
                == (pid, invocation), "candidate manager invocation changed")
        journal = self._journal()
        require(journal is not None and journal["transaction_id"] == transaction_id
                and journal["phase"] in ("starting", "verified"),
                "candidate transaction changed before injection")
        return journal["phase"]

    def _watch(self):
        try:
            for _ in range(16_384):
                require(time.monotonic() < self.deadline, "candidate watcher deadline expired")
                require(not self.cancelled.is_set(), "candidate watcher cancelled")
                journal = self._journal()
                if journal is not None and journal["phase"] in ("starting", "verified"):
                    try:
                        observed = manager_process(self.runner, "watch-manager", self.deadline, self.cancelled.is_set)
                    except (RuntimeError, OSError):
                        observed = None
                    if (observed is not None and observed[0] != self.original_manager[0]
                            and observed[1] != self.original_manager[1]):
                        pid, invocation = observed
                        token = process_token(pid)
                        if readiness_socket_path(self.runner.home, pid, token).exists():
                            phases = []

                            def verify(current_pid, current_token):
                                phases.append(self._check_context(current_pid, current_token,
                                                                  invocation, journal["transaction_id"]))

                            receipt = pause_verified_process(pid, self.candidate_sha256, verify, self.deadline,
                                                             fault_signal=self.fault_signal)
                            self.result = {"injected": True, **receipt, "invocation": invocation,
                                           "transaction_id": journal["transaction_id"], "phase": phases[-1]}
                            return
                self.cancelled.wait(0.25)
            raise RuntimeError("candidate watcher exceeded observation bound")
        except BaseException as error:
            self.result["error"] = str(error)
        finally:
            self.runner.save("result.json", self.result)


def require_enablement(runner, name, expected, unit, owned_unit):
    state = runner.run(name, ["/usr/bin/systemctl", "--user", "show", "ww.service",
                             "--property=UnitFileState", "--value"]).strip()
    require(state == expected, f"service enablement changed: expected {expected}, got {state}")
    require(unit.read_bytes() == owned_unit, "custom service definition changed")


def require_fault_stop_policy(runner):
    output = runner.run("fault-stop-policy", ["/usr/bin/systemctl", "--user", "show", "ww.service",
                        "--property=TimeoutStopUSec", "--property=KillMode", "--property=SendSIGKILL"])
    values = dict(line.split("=", 1) for line in output.splitlines() if "=" in line)
    require(values == {"TimeoutStopUSec": "5s", "KillMode": "control-group", "SendSIGKILL": "yes"},
            "fault injection requires effective five-second stop and control-group SIGKILL policy")


def readiness(runner, state, name):
    deadline = time.monotonic() + runner.timeout
    last_error = "no running process"
    while time.monotonic() < deadline:
        try:
            pid, invocation = manager_process(runner, name + "-process", deadline=deadline)
            with pinned_pid(pid):
                token = process_token(pid)
                expected = state["native"]["artifact"]["digest"]
                require(sha256(Path(f"/proc/{pid}/exe")) == expected, "wrong running native executable")
                path = readiness_socket_path(runner.home, pid, token)
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                    stream.settimeout(max(0.001, deadline - time.monotonic()))
                    stream.connect(str(path))
                    peer_pid, peer_uid, _ = struct.unpack("3i", stream.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                    require(peer_pid == pid and peer_uid == os.geteuid(), "readiness peer differs from manager process")
                    nonce = list(os.urandom(32))
                    request = json.dumps({"version": 1, "nonce": nonce, "launch_nonce": None,
                                          "timeout_millis": min(300_000, max(1, int((deadline - time.monotonic()) * 1000)))}, separators=(",", ":")).encode()
                    stream.sendall(len(request).to_bytes(4, "big") + request)
                    length = int.from_bytes(recv_exact(stream, 4), "big")
                    require(0 < length <= MAX_RECORD, "readiness reply exceeds bound")
                    response = json.loads(recv_exact(stream, length))
                    require(response["version"] == 1 and response["nonce"] == nonce
                            and response["artifact"] == state["native"]["artifact"], "readiness reply identifies another request or artifact")
                    require(response["contract"] == state["disposition"]["contract"], "readiness contract differs from committed state")
                    require(process_token(pid) == token and sha256(Path(f"/proc/{pid}/exe")) == expected,
                            "process changed across readiness observation")
                    require(manager_process(runner, name + "-process-after", deadline=deadline) == (pid, invocation),
                            "manager invocation changed across readiness observation")
                    require(response["serving_ready"] and response["effective_root"]
                            and response["kernel_source"] and response["kernel_runtime"], "Host is not serving ready")
                    require(time.monotonic() < deadline, "readiness deadline expired before acknowledgement")
                    acknowledge_live_reply(stream)
                runner.save(name + "-readiness.json", {"pid": pid, "invocation": invocation,
                                                       "process_start": token, "response": response})
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
              "remaining": ["initialization, admission, replacement, rollback and terminal crash boundaries",
                            "wrong executable, PID reuse and same-PID exec refusal",
                            "stale socket and nonce replay refusal", "changed inputs, service definition and trust refusal",
                            "missing or substituted proof recovery", "moving-head and missing-route readiness cases",
                            "compatibility and legacy-protocol refusal", "concurrent manual process exclusion",
                            "absent service case", "login/boot persistence"]}
    runner.save("report.json", report)
    runner.save("original-producer-verification.json", {
        key: verify_producer_files(marker[key], TARGETS[args.platform][2]) for key in ("a", "b")})
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
        owned_unit = unit.read_bytes()
        original_unit = owned_unit.decode("utf-8")
        require(original_unit.count("[Service]\n") == 1, "unexpected generated service definition")
        require("TimeoutStopSec=" not in original_unit, "generated service already specifies a stop budget")
        owned_unit = original_unit.replace("[Service]\n", "[Service]\nTimeoutStopSec=5\nEnvironment=WW_COMPILE_WORKERS=2\nWorkingDirectory=" + str(home) + "\n").encode()
        unit.write_bytes(owned_unit)
        runner.run("daemon-reload", ["/usr/bin/systemctl", "--user", "daemon-reload"])
        require_fault_stop_policy(runner)
        runner.run("enable-before-admission", ["/usr/bin/systemctl", "--user", "enable", "ww.service"])
        require_enablement(runner, "enabled-before-admission", "enabled", unit, owned_unit)
        check_before = durable_snapshot(home)
        runner.run("admit-a-check", [binary, "startup", "admit", "--check", "--timeout-secs", args.timeout_secs,
                                    "--json", "service"])
        require(durable_snapshot(home) == check_before, "admission check changed native acceptance files")
        report["checks"].append("stopped admission check preserves native acceptance files")
        runner.run("admit-a", [binary, "startup", "admit", "--timeout-secs", args.timeout_secs, "--json", "service"])
        state = native_tuple(home, args.release_a, marker["a"])
        runner.run("start-a", ["/usr/bin/systemctl", "--user", "start", "ww.service"])
        readiness(runner, state, "a")
        require_enablement(runner, "enabled-a", "enabled", unit, owned_unit)
        report["checks"].append("actual systemd A starts with HTTP admin disabled")
        budget_before = durable_snapshot(home)
        budget_manager = manager_process(runner, "budget-before-manager")
        runner.run("missing-upgrade-budget", [binary, "upgrade", "--release", args.release_b, *common,
                                             "--json"], expected=1)
        runner.run("zero-upgrade-budget", [binary, "upgrade", "--release", args.release_b, *common,
                                          "--timeout-secs", "0", "--json"], expected=2)
        require(durable_snapshot(home) == budget_before
                and manager_process(runner, "budget-after-manager") == budget_manager,
                "missing or invalid budget changed native state or the service process")
        report["checks"].append("missing and zero changed-upgrade budgets refuse without native or service mutation")
        runner.run("upgrade-a-b", [binary, "upgrade", "--release", args.release_b, *common, "--timeout-secs", args.timeout_secs, "--json"])
        state = native_tuple(home, args.release_b, marker["b"])
        readiness(runner, state, "b")
        require_enablement(runner, "enabled-b", "enabled", unit, owned_unit)
        report["checks"].append("actual changed A to B commits matching provenance and readiness")
        runner.run("rollback-b-a", [binary, "upgrade", "--rollback", "--timeout-secs", args.timeout_secs, "--json"])
        state = native_tuple(home, args.release_a, marker["a"])
        readiness(runner, state, "restored-a")
        require_enablement(runner, "enabled-restored-a", "enabled", unit, owned_unit)
        report["checks"].append("actual retained rollback restores A with measured readiness")
        report["checks"].append("enabled A to B to A preserves enablement and custom unit bytes")
        before_noop = durable_snapshot(home)
        before_noop_process = manager_process(runner, "noop-before-manager")
        noop = json.loads(runner.run("exact-noop", [binary, "upgrade", "--release", args.release_a, *common, "--json"]))
        require(noop["outcome"]["kind"] == "already-installed" and noop["changed_paths"] == [] and noop["activation"] is False, "exact no-op changed software or activated service")
        native_tuple(home, args.release_a, marker["a"])
        require(durable_snapshot(home) == before_noop
                and manager_process(runner, "noop-after-manager") == before_noop_process,
                "exact no-op changed native acceptance files or service invocation")
        for fault in ("readiness-timeout", "failed-start"):
            original_manager = manager_process(runner, fault + "-original-manager")
            with CandidatePauseWatcher(runner, marker["b"]["sha256"], original_manager, fault=fault) as watcher:
                runner.run(fault, [binary, "upgrade", "--release", args.release_b, *common,
                           "--timeout-secs", args.timeout_secs, "--json"], expected=1)
            injection = watcher.require_injected()
            pending = record(home / ".ww/host-upgrade.json", os.geteuid())
            blocked = record(home / ".ww/bin/.ww-host-upgrade/startup-state.json", os.geteuid())
            require(pending["version"] == 2 and pending["operation"] == "upgrade"
                    and pending["phase"] == "containing" and pending["transaction_id"] == injection["transaction_id"]
                    and pending["disposition"]["stop_intent"] is True
                    and blocked["disposition"]["kind"] == "blocked",
                    fault + " did not retain a nonrunnable blocked transaction")
            require(runner.run(fault + "-contained-pid", ["/usr/bin/systemctl", "--user", "show", "ww.service",
                               "--property=MainPID", "--value"]).strip() == "0",
                    fault + " did not contain the owned candidate")
            require_enablement(runner, fault + "-enabled-after-fault", "enabled", unit, owned_unit)
            runner.save(fault + "-state.json", {"journal": pending, "state": blocked})
            recovered = json.loads(runner.run(fault + "-recover", [binary, "upgrade", "--recover-only",
                                   "--release", args.release_b, *common, "--timeout-secs", args.timeout_secs, "--json"]))
            require(recovered["outcome"]["kind"] == "recovered", fault + " retry did not perform recovery")
            state = native_tuple(home, args.release_a, marker["a"])
            readiness(runner, state, fault + "-restored-a")
            require_enablement(runner, fault + "-enabled-after-recovery", "enabled", unit, owned_unit)
            report["checks"].append(fault + ": exact candidate signaled through pidfd; blocked intent retained; installed CLI recovery restored ready A")
        runner.run("stop-before-stopped-upgrade", ["/usr/bin/systemctl", "--user", "stop", "ww.service"])
        runner.run("disable-before-stopped-admission", ["/usr/bin/systemctl", "--user", "disable", "ww.service"])
        require_enablement(runner, "disabled-before-stopped-admission", "disabled", unit, owned_unit)
        runner.run("admit-stopped-disabled-a", [binary, "startup", "admit", "--timeout-secs", args.timeout_secs, "--json", "service"])
        result = json.loads(runner.run("stopped-upgrade", [binary, "upgrade", "--release", args.release_b, *common, "--timeout-secs", args.timeout_secs, "--json"]))
        native_tuple(home, args.release_b, marker["b"])
        require(result["activation"] is False, "stopped upgrade activated service")
        require(runner.run("stopped-pid", ["/usr/bin/systemctl", "--user", "show", "ww.service", "--property=MainPID", "--value"]).strip() == "0", "stopped service started during upgrade")
        require_enablement(runner, "disabled-after-stopped-upgrade", "disabled", unit, owned_unit)
        result = json.loads(runner.run("stopped-rollback", [binary, "upgrade", "--rollback",
                                "--timeout-secs", args.timeout_secs, "--json"]))
        native_tuple(home, args.release_a, marker["a"])
        require(result["activation"] is False, "stopped rollback activated service")
        require(runner.run("stopped-rollback-pid", ["/usr/bin/systemctl", "--user", "show", "ww.service",
                            "--property=MainPID", "--value"]).strip() == "0", "stopped rollback started service")
        require_enablement(runner, "disabled-after-stopped-rollback", "disabled", unit, owned_unit)
        require(unit.read_bytes() == owned_unit and all(sha256(Path(path)) == value for path, value in hashes.items()), "native operation changed preserved configuration, identity, FHS or private state")
        report["checks"].extend(["exact-artifact no-op preserves provenance", "disabled stopped upgrade and rollback remain disabled and stopped", "custom configuration, identity, FHS and private bytes preserved"])
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
        except BaseException as error:
            report["status"] = "FAILED"
            report["cleanup"] = "fixture cleanup failed; inspect retained service and command evidence"
            report["cleanup_error"] = str(error)
            raise
        finally:
            report["executed_checks"] = len(report["checks"])
            runner.save("report.json", report)
    return 0 if report["status"] == "PASS" else 2


if __name__ == "__main__":
    raise SystemExit(main())
