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
import http.client
import io
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
PAIR_POLICY = "pr739-reduced-current-pair-native-diagnostic-v1"
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
    local = receipt["builder"] == f'local-native-diagnostic:{receipt["revision"]}:{target}:release'
    require((local and target == "aarch64-unknown-linux-gnu"
             and receipt["run_id"] is None and receipt["attempt"] is None)
            or (not local and type(receipt["run_id"]) is int and receipt["run_id"] > 0
                and type(receipt["attempt"]) is int and receipt["attempt"] > 0),
            "producer run and attempt must match its local or CI context")
    expected = f'github-actions:{receipt["run_id"]}:{receipt["attempt"]}:{target}'
    current = f'github-actions-pr-native:{receipt["run_id"]}:{receipt["attempt"]}:{target}:release'
    require(receipt["builder"] == expected or (target == "aarch64-unknown-linux-gnu"
            and (receipt["builder"] == current or local)), "source fixture has no supported producer identity")
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
    if receipt["builder"].startswith(("github-actions-pr-native:", "local-native-diagnostic:")):
        build = record(binary.parent / "BUILD-RECEIPT.json", owner)
        require(build.get("policy") == "same-job-native-build-v1" and build.get("profile") == "release"
                and build.get("target") == target and build.get("builder") == receipt["builder"]
                and build["ci"]["revision"] == receipt["revision"]
                and build["host"] == {"size": info.st_size, "sha256": receipt["sha256"]}
                and build.get("native_acceptance") is False and build.get("publication_allowed") is False,
                "current native build receipt differs")
        expected["BUILD-RECEIPT.json"] = (binary.parent / "BUILD-RECEIPT.json").read_bytes()
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
    require(args.platform == "linux-aarch64", "reduced native platform must be GNU ARM64 Linux")
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
                            "release_a", "release_b", "a", "b", "input_policy", "input_manifest_sha256"}, "invalid qualification marker fields")
    require(marker["version"] == 2 and marker["input_policy"] == PAIR_POLICY
            and re.fullmatch(r"[0-9a-f]{64}", marker["input_manifest_sha256"])
            and marker["purpose"] == "disposable-native-upgrade"
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

    def run(self, name, argv, expected=0, deadline=None, cancelled=None, executable_receipt=None, on_spawn=None):
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
            if on_spawn is not None:
                on_spawn(process)
            if executable_receipt is not None:
                observed = observe_initiator(process.pid, executable_receipt, deadline)
                self.save(name + "-initiator.json", {"argv": argv, **observed})
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
    state = record(root / "host-upgrade-retained.json", os.geteuid())
    require(state.get("version") == 3 and set(state) == {"version", "current", "previous"},
            "native retention is not the reduced V3 contract")
    current = state["current"]
    locator = record(root / "installed-release.json", os.geteuid())
    require(current["transport_root"] == release and locator == {"version": 1, "release": release},
            "native state and compact release locator disagree")
    binary = root / "bin/ww"
    require(current["version"] == 1 and current["target"] == "aarch64-unknown-linux-gnu"
            and current["artifact"] == {"algorithm": "sha256", "digest": receipt["sha256"],
                                        "size": binary.stat().st_size}
            and sha256(binary) == receipt["sha256"]
            and current["revision"] == receipt["revision"], "installed native producer identity differs")
    reference = current["proof"]
    require(reference["root"] == release and 0 < reference["size"] <= 8 * 1024 * 1024,
            "retained proof does not bind the native transport root")
    proof = root / "bin/.ww-host-upgrade/proofs" / release
    proof_info = custody(proof, os.geteuid())
    require(proof_info.st_size == reference["size"] and sha256(proof) == reference["sha256"],
            "retained native proof bytes differ from their reference")
    installation = record(root / "installation.json", os.geteuid())
    require(installation.get("complete") is True and installation.get("version") in (1, 2),
            "native installation receipt is incomplete")
    require(not (root / "host-upgrade.json").exists(), "native operation left pending recovery")
    return state


def durable_snapshot(home):
    """Acceptance files only; ordinary FHS locks/caches do not authorize releases."""
    root = home / ".ww"
    store = root / "bin/.ww-host-upgrade"
    paths = [root / "bin/ww", root / "installation.json", root / "installed-release.json",
             root / "host-upgrade.json", root / "host-upgrade-retained.json"]
    if store.exists():
        custody(store, os.geteuid(), directory=True)
        for path in sorted(store.rglob("*")):
            if stat.S_ISDIR(path.lstat().st_mode):
                custody(path, os.geteuid(), directory=True)
            else:
                paths.append(path)
    result = {}
    for path in paths:
        if path.exists() or path.is_symlink():
            info = custody(path, os.geteuid())
            result[str(path.relative_to(home))] = {"sha256": sha256(path), "size": info.st_size,
                "inode": info.st_ino, "mtime_ns": info.st_mtime_ns, "mode": stat.S_IMODE(info.st_mode)}
    return result


def rollback_refusal_evidence(home, before, corrupt_digest, source_size):
    """A failed bounded copy may leave only this exact untrusted scratch file."""
    scratch_name = ".ww/bin/.ww-host-upgrade/stage"
    require(scratch_name not in before, "corrupt rollback started with unexpected scratch stage")
    after = durable_snapshot(home)
    scratch = after.pop(scratch_name, None)
    require(after == before, "corrupt rollback changed installed state or discarded evidence")
    if scratch is not None:
        require(0 < source_size <= 1024 ** 3 and scratch["size"] == source_size
                and scratch["sha256"] == corrupt_digest,
                "rollback scratch is not the exact bounded corrupt source")
    return scratch


def service_disposition(runner, name):
    keys = ("MainPID", "InvocationID", "ActiveState", "SubState", "UnitFileState")
    raw = runner.run(name, ["/usr/bin/systemctl", "--user", "show", "ww.service",
                           *["--property=" + key for key in keys]])
    value = dict(line.split("=", 1) for line in raw.splitlines() if "=" in line)
    require(set(value) == set(keys), "service disposition is incomplete")
    return value


def pending_native(home):
    journal = record(home / ".ww/host-upgrade.json", os.geteuid())
    require(set(journal) == {"version", "work"} and journal["version"] == 3
            and set(journal["work"]) == {"kind", "transaction"}
            and journal["work"]["kind"] == "native", "expected reduced V3 native journal")
    return journal["work"]["transaction"]


def process_token(pid):
    stat_text = Path(f"/proc/{pid}/stat").read_text()
    fields = stat_text.rsplit(")", 1)[1].split()
    return [Path("/proc/sys/kernel/random/boot_id").read_text().strip(), int(fields[19])]


def observe_initiator(pid, receipt, deadline):
    """Measure the actual owned CLI process before accepting its upgrade result."""
    require(time.monotonic() < deadline, "initiator observation deadline expired")
    with pinned_pid(pid):
        proc = Path(f"/proc/{pid}")
        require(proc.stat().st_uid == os.geteuid(), "initiator belongs to another account")
        token = process_token(pid)
        with (proc / "exe").open("rb") as executable:
            before = os.fstat(executable.fileno())
            digest = hashlib.file_digest(executable, "sha256").hexdigest()
            after = os.fstat(executable.fileno())
        require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns)
                == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns),
                "initiator executable changed during observation")
        require(digest == receipt["sha256"] and process_token(pid) == token
                and sha256(proc / "exe") == digest, "initiator executable identity differs")
        require(time.monotonic() < deadline, "initiator observation deadline expired")
        return {"pid": pid, "uid": os.geteuid(), "process_start": token,
                "sha256": digest, "revision": receipt["revision"],
                "artifact_size": after.st_size, "pidfd_live": True}


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
        require(fault in ("readiness-timeout", "failed-start", "updater-interrupted"), "unsupported candidate fault kind")
        self.fault_signal = signal.SIGKILL if fault == "failed-start" else signal.SIGSTOP
        self.fault = fault
        self.updater = None
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
        if self.updater is not None:
            os.close(self.updater[1])

    def require_injected(self):
        require(self.result["injected"], "candidate watcher did not inject: " + self.result.get("error", "unknown"))
        return self.result

    def bind_updater(self, process):
        require(self.updater is None and process.poll() is None, "updater child is not live")
        # Runner owns and retains this unreaped child; the stable descriptor is
        # acquired at spawn, never by rediscovering a numeric PID later.
        self.updater = (process.pid, os.pidfd_open(process.pid))

    def _journal(self):
        try:
            journal = pending_native(self.runner.home)
        except FileNotFoundError:
            return None
        require(journal["operation"] == "Upgrade", "candidate watcher found another transaction kind")
        require(journal["after"]["generation"]["artifact"]["digest"] == self.candidate_sha256,
                "candidate watcher found another target artifact")
        return journal

    def _check_context(self, pid, token, invocation, transaction_id):
        require(not self.cancelled.is_set(), "candidate watcher cancelled before injection")
        require(manager_process(self.runner, "verify-manager", self.deadline, self.cancelled.is_set)
                == (pid, invocation), "candidate manager invocation changed")
        journal = self._journal()
        require(journal is not None and journal["transaction_id"] == transaction_id
                and journal["phase"] in ("Starting", "Verified"),
                "candidate transaction changed before injection")
        return journal["phase"]

    def _watch(self):
        try:
            observations = 0
            for _ in range(16_384):
                require(time.monotonic() < self.deadline, "candidate watcher deadline expired")
                require(not self.cancelled.is_set(), "candidate watcher cancelled")
                journal = self._journal()
                if journal is not None and journal["phase"] in ("Starting", "Verified"):
                    observations += 1
                    require(observations <= 512, "candidate watcher exceeded manager observation bound")
                    try:
                        observed = manager_process(self.runner, "watch-manager", self.deadline, self.cancelled.is_set)
                    except (RuntimeError, OSError):
                        observed = None
                    if (observed is not None and observed[0] != self.original_manager[0]
                            and observed[1] != self.original_manager[1]):
                        pid, invocation = observed
                        token = process_token(pid)
                        phases = []

                        def verify(current_pid, current_token):
                            phases.append(self._check_context(current_pid, current_token,
                                                              invocation, journal["transaction_id"]))

                        receipt = pause_verified_process(pid, self.candidate_sha256, verify, self.deadline,
                                                         fault_signal=self.fault_signal)
                        self.result = {"injected": True, **receipt, "invocation": invocation,
                                       "transaction_id": journal["transaction_id"], "phase": phases[-1]}
                        if self.fault == "updater-interrupted":
                            require(self.updater is not None, "owned updater was not bound at spawn")
                            # The candidate is paused, leaving a real readiness wait
                            # and durable journal. Kill only our pinned updater child.
                            verify(pid, token)
                            signal.pidfd_send_signal(self.updater[1], signal.SIGKILL)
                            self.result["updater_pid"] = self.updater[0]
                            self.result["updater_signal"] = "SIGKILL"
                        return
                self.cancelled.wait(0.01)
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
                        "--property=TimeoutStopUSec", "--property=KillMode", "--property=SendSIGKILL",
                        "--property=Restart"])
    values = dict(line.split("=", 1) for line in output.splitlines() if "=" in line)
    require(values == {"TimeoutStopUSec": "5s", "KillMode": "control-group", "SendSIGKILL": "yes",
                       "Restart": "no"},
            "fault injection requires effective five-second stop, control-group SIGKILL and no automatic restart")


def probe_ready(endpoint, deadline):
    address = urlsplit(endpoint)
    require(address.scheme == "http" and address.hostname in ("127.0.0.1", "::1")
            and address.port and not address.username and not address.password
            and address.path in ("", "/") and not address.query and not address.fragment,
            "readiness requires an explicit loopback HTTP endpoint")
    remaining = deadline - time.monotonic()
    require(remaining > 0, "readiness deadline expired")
    # Read the entire bounded Connection: close response with an absolute
    # deadline. A socket timeout alone can be extended forever by a slow drip.
    with socket.create_connection((address.hostname, address.port), timeout=min(1, remaining)) as stream:
        stream.sendall(f"GET /readyz HTTP/1.1\r\nHost: {address.netloc}\r\nConnection: close\r\n\r\n".encode())
        raw = bytearray()
        while True:
            remaining = deadline - time.monotonic()
            require(remaining > 0, "readiness deadline expired")
            stream.settimeout(min(1, remaining))
            part = stream.recv(min(8192, MAX_RECORD + 1 - len(raw)))
            if not part:
                break
            raw.extend(part)
            require(len(raw) <= MAX_RECORD, "readiness body exceeds bound")
    class BufferedResponse:
        def makefile(self, *_):
            return io.BytesIO(raw)
    response = http.client.HTTPResponse(BufferedResponse())
    try:
        response.begin()
        value = json.loads(response.read(MAX_RECORD + 1))
        require(response.status == 200 and value.get("ready") is True and value.get("phase") == "ready",
                "Host /readyz does not report serving readiness")
        require(time.monotonic() < deadline, "readiness deadline expired")
        return {"status": response.status, "body": value, "endpoint": endpoint + "/readyz"}
    finally:
        response.close()


def readiness(runner, state, name):
    deadline = time.monotonic() + runner.timeout
    last_error = "no running process"
    while time.monotonic() < deadline:
        try:
            pid, invocation = manager_process(runner, name + "-process", deadline=deadline)
            with pinned_pid(pid):
                token = process_token(pid)
                installed = os.stat(runner.home / ".ww/bin/ww")
                observed = os.stat(f"/proc/{pid}/exe")
                require((installed.st_dev, installed.st_ino) == (observed.st_dev, observed.st_ino),
                        "manager executable file differs from installed Host")
                response = probe_ready(runner.admin_endpoint, deadline)
                require(manager_process(runner, name + "-process-after", deadline=deadline) == (pid, invocation),
                        "manager invocation changed across readiness observation")
                after = os.stat(f"/proc/{pid}/exe")
                require(process_token(pid) == token and (after.st_dev, after.st_ino) == (installed.st_dev, installed.st_ino),
                        "manager executable changed across readiness observation")
                result = {"pid": pid, "invocation": invocation, "process_start": token,
                          "artifact": state["current"]["artifact"], "response": response,
                          "installed_file": [installed.st_dev, installed.st_ino]}
                runner.save(name + "-readiness.json", result)
                return result
        except (RuntimeError, OSError, KeyError, ValueError, http.client.HTTPException) as error:
            last_error = str(error)
            time.sleep(min(0.1, max(0, deadline - time.monotonic())))
    raise RuntimeError(f"readiness deadline: {last_error}")


def write_fault(path, raw):
    """Durable corruption of one fixture-owned metadata file, with no symlinks."""
    if path.exists():
        custody(path, os.geteuid())
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())
    directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


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
    runner.admin_endpoint = "http://127.0.0.1:2026"
    report = {"version": 2, "platform": args.platform, "status": "RUNNING", "checks": [],
              "producer_receipts": {key: marker[key] for key in ("a", "b")},
              "trust_sha256": sha256(trust), "production_activation": False, "native_acceptance": False,
              "wrong_executable_interception": "UNQUALIFIED; not attempted",
              "remaining": ["exhaustive native crash-boundary matrix", "login/boot persistence",
                            "native unsupported-readiness and absent-service refusal", "native corrupt retained-proof refusal"],
              "fixture_sequence_scope": "ephemeral fixture channel only; independent of Git ancestry"}
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
        require(initial["previous"] is None, "fresh installation has previous native software")
        report["checks"].append("authenticated current R1 installs without startup admission")
        private = home / ".ww/data/native-qualification-sentinel"
        private.parent.mkdir(mode=0o700, exist_ok=True)
        private.write_bytes(os.urandom(32))
        preserved = [home / ".ww/identity", home / ".ww/fhs/boot/kernel.wasm", home / ".ww/fhs/bin/status.wasm", private]
        hashes = {str(path): sha256(path) for path in preserved}
        runner.run("daemon-register", [binary, "daemon", "install", "--executor-threads", "2", "--listen", "/ip4/127.0.0.1/tcp/0", "--with-http-admin", "127.0.0.1:2026", "--ipfs-url", args.ipfs_url, "--json"])
        owned_unit = unit.read_bytes()
        original_unit = owned_unit.decode("utf-8")
        require(original_unit.count("[Service]\n") == 1, "unexpected generated service definition")
        require(original_unit.count("\nWorkingDirectory=%h\n") == 1,
                "generated service must require the account home directory")
        require("TimeoutStopSec=" not in original_unit, "generated service already specifies a stop budget")
        require(original_unit.count("\nRestart=") == 1
                and "\nRestart=on-failure\n" in original_unit, "generated restart policy differs")
        # Select the disposable unit's policy before startup. A single killed
        # candidate must be a terminal start failure for this control; the
        # production on-failure policy can legitimately restart and reach ready.
        original_unit = original_unit.replace("\nRestart=on-failure\n", "\nRestart=no\n")
        owned_unit = original_unit.replace("[Service]\n", "[Service]\nTimeoutStopSec=5\nEnvironment=WW_COMPILE_WORKERS=2\n").encode()
        unit.write_bytes(owned_unit)
        runner.run("daemon-reload", ["/usr/bin/systemctl", "--user", "daemon-reload"])
        require_fault_stop_policy(runner)
        runner.run("enable-before-startup", ["/usr/bin/systemctl", "--user", "enable", "ww.service"])
        require_enablement(runner, "enabled-before-startup", "enabled", unit, owned_unit)
        state = native_tuple(home, args.release_a, marker["a"])
        runner.run("start-a", ["/usr/bin/systemctl", "--user", "start", "ww.service"])
        readiness(runner, state, "a")
        require_enablement(runner, "enabled-a", "enabled", unit, owned_unit)
        report["checks"].append("ordinary systemd R1 starts with loopback serving readiness")
        budget_before = durable_snapshot(home)
        checked = json.loads(runner.run("upgrade-check", [binary, "upgrade", "--check", "--release", args.release_b, *common, "--json"]))
        require(checked["outcome"]["kind"] == "checked" and durable_snapshot(home) == budget_before, "upgrade check mutated acceptance files")
        budget_manager = manager_process(runner, "budget-before-manager")
        runner.run("missing-upgrade-budget", [binary, "upgrade", "--release", args.release_b, *common,
                                             "--json"], expected=1)
        runner.run("zero-upgrade-budget", [binary, "upgrade", "--release", args.release_b, *common,
                                          "--timeout-secs", "0", "--json"], expected=2)
        require(durable_snapshot(home) == budget_before
                and manager_process(runner, "budget-after-manager") == budget_manager,
                "missing or invalid budget changed native state or the service process")
        report["checks"].append("missing and zero changed-upgrade budgets refuse without native or service mutation")
        runner.run("upgrade-a-b", [binary, "upgrade", "--release", args.release_b, *common, "--timeout-secs", args.timeout_secs, "--json"],
                   executable_receipt=marker["a"])
        state = native_tuple(home, args.release_b, marker["b"])
        readiness(runner, state, "b")
        require_enablement(runner, "enabled-b", "enabled", unit, owned_unit)
        report["checks"].append("current R1 to R0 commits matching provenance and readiness")
        runner.run("rollback-b-a", [binary, "upgrade", "--rollback", "--timeout-secs", args.timeout_secs, "--json"])
        state = native_tuple(home, args.release_a, marker["a"])
        readiness(runner, state, "restored-a")
        require_enablement(runner, "enabled-restored-a", "enabled", unit, owned_unit)
        report["checks"].append("retained rollback restores R1 with measured readiness")
        report["checks"].append("enabled R1 to R0 to R1 preserves enablement and custom unit bytes")
        before_noop = durable_snapshot(home)
        before_noop_process = manager_process(runner, "noop-before-manager")
        noop = json.loads(runner.run("exact-noop", [binary, "upgrade", "--release", args.release_a, *common, "--json"]))
        require(noop["outcome"]["kind"] == "already-installed" and noop["changed_paths"] == [] and noop["activation"] is False, "exact no-op changed software or activated service")
        native_tuple(home, args.release_a, marker["a"])
        require(durable_snapshot(home) == before_noop
                and manager_process(runner, "noop-after-manager") == before_noop_process,
                "exact no-op changed native acceptance files or service invocation")
        for fault in ("readiness-timeout", "failed-start", "updater-interrupted"):
            original_manager = manager_process(runner, fault + "-original-manager")
            with CandidatePauseWatcher(runner, marker["b"]["sha256"], original_manager, fault=fault) as watcher:
                runner.run(fault, [binary, "upgrade", "--release", args.release_b, *common,
                           "--timeout-secs", args.timeout_secs, "--json"],
                           expected=-signal.SIGKILL if fault == "updater-interrupted" else 1,
                           on_spawn=watcher.bind_updater)
            injection = watcher.require_injected()
            if fault == "updater-interrupted":
                pending = pending_native(home)
                require(pending["operation"] == "Upgrade"
                        and pending["phase"] in ("Starting", "Verified")
                        and pending["transaction_id"] == injection["transaction_id"]
                        and pending["stop_intent"] is True,
                        fault + " did not retain the interrupted reduced transaction")
            else:
                require(not (home / ".ww/host-upgrade.json").exists(),
                        fault + " bounded live rollback left a pending journal")
                pending = None
                native_tuple(home, args.release_a, marker["a"])
            require_enablement(runner, fault + "-enabled-after-fault", "enabled", unit, owned_unit)
            runner.save(fault + "-state.json", {"journal": pending, "installed_sha256": sha256(binary)})
            recovery = runner.run(fault + "-recover", [binary, "upgrade", "--recover-only",
                                  "--release", args.release_b, *common, "--timeout-secs", args.timeout_secs, "--json"],
                                  expected=0 if pending else 1)
            if pending:
                require(json.loads(recovery)["outcome"]["kind"] == "recovered",
                        fault + " retry did not perform recovery")
            state = native_tuple(home, args.release_a, marker["a"])
            readiness(runner, state, fault + "-restored-a")
            require_enablement(runner, fault + "-enabled-after-recovery", "enabled", unit, owned_unit)
            report["checks"].append(fault + (": exact candidate signaled through pidfd; V3 intent retained; installed CLI recovery restored ready R1" if fault == "updater-interrupted" else ": exact candidate signaled through pidfd; bounded live rollback restored ready R1"))
        idempotent_before = durable_snapshot(home)
        runner.run("recovery-idempotent", [binary, "upgrade", "--recover-only", "--release", args.release_a,
                   *common, "--timeout-secs", args.timeout_secs, "--json"], expected=1)
        require(durable_snapshot(home) == idempotent_before, "recovery retry without pending work mutated native state")
        report["checks"].append("recovery retry without pending transaction preserves state")
        runner.run("stop-before-stopped-upgrade", ["/usr/bin/systemctl", "--user", "stop", "ww.service"])
        runner.run("disable-before-stopped-upgrade", ["/usr/bin/systemctl", "--user", "disable", "ww.service"])
        require_enablement(runner, "disabled-before-stopped-upgrade", "disabled", unit, owned_unit)
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
        # Damage only the fixture-owned retained rollback copy; never alter the
        # root-owned original or the executable currently installed.
        retained = native_tuple(home, args.release_a, marker["a"])
        previous = retained["previous"]
        require(previous["artifact"]["digest"] == marker["b"]["sha256"], "rollback source differs")
        artifact = previous["artifact"]
        retained_path = home / ".ww/bin/.ww-host-upgrade" / f'{artifact["algorithm"]}-{artifact["digest"]}-{artifact["size"]}.ww'
        custody(retained_path, os.geteuid())
        original_digest = sha256(retained_path)
        with retained_path.open("r+b") as stream:
            first = stream.read(1)
            require(bool(first), "retained executable is empty")
            stream.seek(0)
            stream.write(bytes([first[0] ^ 1]))
            stream.flush()
            os.fsync(stream.fileno())
        corrupt_digest = sha256(retained_path)
        corrupt_before = durable_snapshot(home)
        corrupt_service = service_disposition(runner, "corrupt-rollback-service-before")
        failed = json.loads(runner.run("corrupt-rollback", [binary, "upgrade", "--rollback",
                            "--timeout-secs", args.timeout_secs, "--json"], expected=1))
        require(failed["status"] == "failed", "corrupt rollback did not report refusal")
        scratch = rollback_refusal_evidence(home, corrupt_before, corrupt_digest, artifact["size"])
        service_after = service_disposition(runner, "corrupt-rollback-service-after")
        require(service_after == corrupt_service, "corrupt rollback changed service disposition")
        runner.save("corrupt-rollback.json", {"path": str(retained_path), "before_sha256": original_digest,
                    "after_sha256": corrupt_digest, "installed_sha256": sha256(binary), "result": failed,
                    "source_size": artifact["size"], "scratch_stage": scratch,
                    "service_before": corrupt_service, "service_after": service_after})
        report["checks"].append("corrupt retained bytes refuse rollback without replacement")

        journal_path = home / ".ww/host-upgrade.json"
        damaged = b'{"version":999,"damaged":"qualification"}'
        write_fault(journal_path, damaged)
        damaged_before = durable_snapshot(home)
        runner.run("ordinary-damaged-start", ["/usr/bin/systemctl", "--user", "start", "ww.service"])
        readiness(runner, retained, "damaged-ordinary")
        require(durable_snapshot(home) == damaged_before, "ordinary startup changed damaged native evidence")
        runner.run("ordinary-damaged-stop", ["/usr/bin/systemctl", "--user", "stop", "ww.service"])
        runner.run("damaged-mutation-refusal", [binary, "upgrade", "--release", args.release_b, *common,
                   "--timeout-secs", args.timeout_secs, "--json"], expected=1)
        require(durable_snapshot(home) == damaged_before, "ambiguous mutation changed damaged native evidence")
        repair_args = [binary, "upgrade", "--repair-baseline", "--release", args.release_a, *common,
                       "--timeout-secs", args.timeout_secs, "--json"]
        runner.run("repair-missing-ack", repair_args, expected=1)
        require(durable_snapshot(home) == damaged_before, "unacknowledged repair changed damaged evidence")
        repair = json.loads(runner.run("repair-baseline", [*repair_args, "--acknowledge-missing-rollback"]))
        outcome = repair["outcome"]
        require(outcome["kind"] == "repaired-baseline" and outcome["historical_rollback_available"] is False
                and outcome["live_ready"] is False and repair["activation"] is False
                and repair["changed_paths"] == [], "repair overstated restoration or readiness")
        archive = absolute_path(outcome["archive"])
        store = home / ".ww/bin/.ww-host-upgrade"
        require(archive.parent == store and re.fullmatch(r"repair-[0-9a-f]{64}", archive.name),
                "repair archive escaped native store")
        custody(archive, os.geteuid(), directory=True)
        custody(archive / "journal", os.geteuid())
        require((archive / "journal").read_bytes() == damaged and sha256(retained_path) == corrupt_digest,
                "repair lost damaged evidence or modified retained bytes")
        manifest = record(archive / "manifest.json", os.geteuid())
        require(manifest["committed"] is True and manifest["historical_rollback_available"] is False,
                "repair archive lacks its committed manifest")
        state = native_tuple(home, args.release_a, marker["a"])
        require(state["previous"] is None, "repair invented historical rollback")
        require(runner.run("repair-stopped-pid", ["/usr/bin/systemctl", "--user", "show", "ww.service",
                           "--property=MainPID", "--value"]).strip() == "0", "metadata repair activated service")
        require_enablement(runner, "repair-disabled", "disabled", unit, owned_unit)
        runner.save("repair-baseline.json", {"result": repair, "manifest": manifest,
                    "archived_journal_sha256": sha256(archive / "journal"),
                    "damaged_journal_sha256": hashlib.sha256(damaged).hexdigest(),
                    "retained_corruption_sha256": corrupt_digest})
        report["checks"].append("ordinary startup ignores damaged native journal and mutation refuses")
        report["checks"].append("acknowledged baseline repair archives evidence without activation or invented rollback")
        # Current R1 is again the authenticated baseline. Sequence 2 still selects
        # R0, and the next transaction must retain current R1 before replacement.
        runner.run("post-repair-upgrade", [binary, "upgrade", "--release", args.release_b, *common,
                   "--timeout-secs", args.timeout_secs, "--json"])
        native_tuple(home, args.release_b, marker["b"])
        runner.run("post-repair-rollback", [binary, "upgrade", "--rollback",
                   "--timeout-secs", args.timeout_secs, "--json"])
        native_tuple(home, args.release_a, marker["a"])
        require(unit.read_bytes() == owned_unit and all(sha256(Path(path)) == value for path, value in hashes.items()),
                "repair or subsequent replacement changed preserved installation data")
        report["checks"].append("next upgrade after repair retains authenticated current bytes for rollback")
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
