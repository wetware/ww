#!/usr/bin/env python3
"""Qualify genuine release consumption with an isolated offline Kubo and HOME.

Consumes qualify_release_rehearsal.py output. No service is registered or changed.
The sequence-only release preserves original producer bytes. A separate FHS
transition fixture adds a valid custom section and is explicitly not original
producer-byte evidence. Test signing keys must never be production identities.
"""

import argparse
from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys
import threading
import urllib.parse
import urllib.request

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "tests"))
from kubo_fixture import isolated_kubo  # noqa: E402


def require(value, message):
    if not value:
        raise RuntimeError(message)


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def snapshot(root):
    return {str(path.relative_to(root)): sha256(path) for path in root.rglob("*")
            if path.is_file() and not path.is_symlink()}


@contextmanager
def substituted_release(endpoint, requested, substituted):
    """Serve another authentic release through the requested release's locator."""
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            parsed = urllib.parse.urlsplit(self.path)
            query = urllib.parse.parse_qsl(parsed.query)
            query = [(key, value.replace(requested, substituted)) for key, value in query]
            target = endpoint + parsed.path + "?" + urllib.parse.urlencode(query)
            request = urllib.request.Request(target, method="POST")
            with urllib.request.urlopen(request, timeout=30) as response:
                body = response.read(1024 * 1024 + 1)
                require(len(body) <= 1024 * 1024, "substitution unexpectedly fetched a payload")
                self.send_response(response.status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rehearsal", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--package-tool", type=Path, required=True)
    parser.add_argument("--alternate-binary", type=Path,
                        help="genuine separately built local Host; tests compatibility refusal and recovery only")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    rehearsal = args.rehearsal.resolve(strict=True)
    binary = args.binary.resolve(strict=True)
    package_tool = args.package_tool.resolve(strict=True)
    work = args.output.resolve()
    work.mkdir(mode=0o700)
    commands = []
    checks = []

    def run(name, command, env=None, failure=None):
        command = [str(part) for part in command]
        result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=300)
        (work / (name + ".stdout")).write_text(result.stdout)
        (work / (name + ".stderr")).write_text(result.stderr)
        commands.append({"name": name, "argv": command, "returncode": result.returncode,
                         "expected_error": failure})
        (work / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")
        require(result.returncode == (1 if failure else 0), f"{name}: {result.stderr}")
        if failure:
            require(failure in result.stderr, f"{name}: wrong rejection: {result.stderr}")
            return None
        return result.stdout

    trust = rehearsal / "verifier-trust"
    initial = json.loads((rehearsal / "finalize-b.stdout").read_text())
    metadata = json.loads((rehearsal / "metadata.json").read_text())
    releases = [(initial, rehearsal / "release-b.car")]
    for name, derived in [("next-sequence", False), ("derived-fhs", True)]:
        tree, spool = work / (name + "-tree"), work / (name + "-spool")
        shutil.copytree(rehearsal / "tree-b", tree)
        (tree / "CHECKSUMS.txt.sig").unlink()
        metadata["sequence"] += 1
        if derived:
            # Same canonical code; a valid metadata section changes exact bytes.
            with (tree / "fhs/bin/status.wasm").open("ab") as stream:
                stream.write(b"\x00\x04\x03742")
            metadata["source"]["builder"] = "disposable-derived-fhs-transition-fixture"
        meta = work / (name + "-metadata.json")
        meta.write_text(json.dumps(metadata))
        run(name + "-prepare", [package_tool, "prepare", "--tree", tree,
                               "--spool", spool, "--metadata", meta])
        run(name + "-sign", ["bash", REPO / "scripts/sign_release.sh",
                            tree / "CHECKSUMS.txt", tree / "CHECKSUMS.txt.sig",
                            rehearsal / "signer/test-only-private.pem", trust])
        identity = json.loads(run(name + "-finalize", [package_tool, "finalize", "--tree", tree,
                                                     "--spool", spool, "--trusted-keys", trust]))
        car = work / (name + ".car")
        run(name + "-export", [package_tool, "export-car", "--spool", spool, "--car", car])
        releases.append((identity, car))
    require(len({identity["root"] for identity, _ in releases}) == 3, "distinct release roots required")

    with isolated_kubo() as kubo:
        endpoint = kubo["IPFS_API"]
        address = urllib.parse.urlsplit(endpoint)
        api = f"/ip4/{address.hostname}/tcp/{address.port}"
        for index, (_, car) in enumerate(releases):
            run(f"kubo-import-{index}", ["ipfs", "--api", api, "dag", "import", car])
        home = work / "home"
        home.mkdir(mode=0o700)
        env = {key: value for key, value in os.environ.items() if not key.startswith(("WW_", "IPFS_"))}
        env.update(HOME=str(home), WW_CWASM_DIR=str(work / "cwasm"), **kubo)

        def consume(name, action, root, check=False, failure=None, api_url=None, selected=None):
            command = [selected or binary, *action, "--release", root,
                       "--trusted-release-keys", trust, "--ipfs-url", api_url or endpoint, "--json"]
            if check:
                command.append("--check")
            output = run(name, command, env=env, failure=failure)
            return json.loads(output) if output is not None else None

        first, second, changed = [identity["root"] for identity, _ in releases]
        checked = consume("fresh-preflight", ["install"], first, check=True)
        require(checked["activation"] is False and not (home / ".ww/identity").exists(), "preflight mutated identity")
        installed_report = consume("fresh-install", ["install"], first)
        installed = home / ".ww/bin/ww"
        require(sha256(installed) == sha256(binary), "installed Host differs from genuine selected artifact")
        require(installed_report["activation"] is False, "installation unexpectedly activated")
        require(not list(home.rglob("*.plist")) and not list(home.rglob("*.service")), "installation registered a service")
        checks.append("fresh typed install and preflight with exact genuine Host")
        private = {".ww/identity": (home / ".ww/identity").read_bytes(),
                   ".ww/data/private": b"application data", ".ww/ipns-watermark": b"signed freshness sentinel",
                   ".ww/signed-ipns-record": b"signed evidence sentinel",
                   ".ww/etc/ns/ww": b"custom namespace policy",
                   ".config/systemd/user/ww.service": b"custom identity listen routing FHS",
                   "Library/LaunchAgents/io.wetware.ww.plist": b"custom launchd settings"}
        for name, payload in private.items():
            path = home / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(payload)
        before = snapshot(home)
        repeated = consume("repeat-install", ["install"], first, selected=installed)
        require(repeated["changed_paths"] == [] and snapshot(home) == before, "repeat install changed state")
        checks.append("repeat typed install preserves identity private state and custom definitions")
        noop = consume("exact-host-upgrade", ["upgrade"], second, selected=installed)
        require(noop["outcome"] == {"kind": "already-installed"} and noop["activation"] is False,
                "exact Host upgrade not a no-op")
        require(snapshot(home) == before, "exact Host upgrade changed installed state")
        checks.append("explicit exact-artifact upgrade no-op; no replacement acceptance")
        unchanged = consume("sequence-only-image-update", ["image", "update"], second, selected=installed)
        require(unchanged["changed_paths"] == [] and unchanged["activation"] is False, "sequence-only update changed artifacts")
        require(snapshot(home) == before, "sequence-only update changed state")
        checks.append("new signed sequence with original producer bytes performs FHS no-op")
        with substituted_release(endpoint, first, second) as adversary:
            consume("signed-other-release-refused", ["install"], first, api_url=adversary,
                    failure="release transport root CID mismatch", selected=installed)
        require(snapshot(home) == before, "validly signed substitution changed installed state")
        checks.append("another validly signed release cannot substitute the requested root")
        consume("changed-fhs-preflight", ["image", "update"], changed, check=True, selected=installed)
        require(snapshot(home) == before, "changed FHS preflight changed state")
        updated = consume("changed-fhs-update", ["image", "update"], changed, selected=installed)
        require(updated["activation"] is False and updated["changed_paths"], "changed FHS update did not apply")
        require(sha256(installed) == sha256(binary), "FHS update replaced native Host")
        for name, payload in private.items():
            require((home / name).read_bytes() == payload, f"FHS update changed private state: {name}")
        require(sha256(home / ".ww/installed-release.json") == before[".ww/installed-release.json"],
                "FHS update changed installed Host release locator")
        changed_snapshot = snapshot(home)
        no_change = consume("repeat-changed-fhs-update", ["image", "update"], changed, selected=installed)
        require(no_change["changed_paths"] == [] and snapshot(home) == changed_snapshot, "repeat FHS update changed state")
        checks.append("derived FHS transition fixture changes only baseline and retains complete generations")
        root = home / ".ww"
        shutil.copy2(root / "image-update-retained.json", root / "image-update.json")
        (root / ".fhs-recovery" / updated["fhs"]["status_blake3"]).unlink()
        recovered = consume("recover-fhs-without-network", ["image", "update"], changed,
                            api_url="http://127.0.0.1:1", selected=installed)
        require(recovered["release"] is None and recovered["recovery"] == "restored previous FHS baseline",
                "FHS recovery did not restore the previous generation before network selection")
        require(sha256(root / "fhs/bin/status.wasm") == before[".ww/fhs/bin/status.wasm"], "FHS recovery restored wrong bytes")
        require(sha256(installed) == sha256(binary), "FHS recovery changed Host")
        for name, payload in private.items():
            require((home / name).read_bytes() == payload, f"FHS recovery changed private state: {name}")
        checks.append("replayed interrupted FHS journal recovers before network selection with Host/private state preserved")

        if args.alternate_binary:
            alternate = args.alternate_binary.resolve(strict=True)
            require(sha256(alternate) != sha256(binary), "alternate Host must have different actual bytes")
            targets = {("Darwin", "arm64"): ("aarch64-apple-darwin", "macos/aarch64"),
                       ("Darwin", "x86_64"): ("x86_64-apple-darwin", "macos/x86_64"),
                       ("Linux", "aarch64"): ("aarch64-unknown-linux-gnu", "linux/aarch64"),
                       ("Linux", "x86_64"): ("x86_64-unknown-linux-gnu", "linux/x86_64")}
            target, location = targets[(platform.system(), platform.machine())]
            tree, spool = work / "alternate-host-tree", work / "alternate-host-spool"
            shutil.copytree(rehearsal / "tree-b", tree)
            (tree / "CHECKSUMS.txt.sig").unlink()
            shutil.copy2(alternate, tree / "bin/ww" / location / "ww")
            meta = json.loads((rehearsal / "metadata.json").read_text())
            meta["sequence"] += 3
            meta["hosts"][target]["builder"] = "local-qualification-debug:" + sha256(alternate)
            meta["source"]["builder"] = "disposable-alternate-host-refusal-fixture"
            path = work / "alternate-host-metadata.json"
            path.write_text(json.dumps(meta))
            run("alternate-prepare", [package_tool, "prepare", "--tree", tree, "--spool", spool,
                                      "--metadata", path])
            run("alternate-sign", ["bash", REPO / "scripts/sign_release.sh", tree / "CHECKSUMS.txt",
                                   tree / "CHECKSUMS.txt.sig", rehearsal / "signer/test-only-private.pem", trust])
            selected = json.loads(run("alternate-finalize", [package_tool, "finalize", "--tree", tree,
                                     "--spool", spool, "--trusted-keys", trust]))
            car = work / "alternate-host.car"
            run("alternate-export", [package_tool, "export-car", "--spool", spool, "--car", car])
            run("alternate-import", ["ipfs", "--api", api, "dag", "import", car])
            releases.append((selected, car))
            # A separate HOME has no fabricated service definition. The native
            # adapter observes the platform manager and retains every admission gate.
            recovery_home = work / "recovery-home"
            recovery_home.mkdir(mode=0o700)
            env["HOME"] = str(recovery_home)
            consume("recovery-home-install", ["install"], first)
            recovery_root = recovery_home / ".ww"
            recovery_installed = recovery_root / "bin/ww"
            preserved = snapshot(recovery_home)
            consume("alternate-host-refused", ["upgrade"], selected["root"], selected=recovery_installed,
                    failure="production integration is unavailable")
            journal = recovery_root / "host-upgrade.json"
            require(journal.is_file(), "expected durable pre-replacement journal after compatibility refusal")
            require(json.loads(journal.read_text())["phase"] == "Staging", "unexpected refused transaction phase")
            for name, digest in preserved.items():
                require(sha256(recovery_home / name) == digest, f"refused upgrade changed {name}")
            # This wrapper only selects the real isolated Kubo endpoint. It does
            # not fabricate transport responses or bypass shell authentication.
            cli = work / "kubo-client"
            cli.mkdir()
            real_ipfs = shutil.which("ipfs")
            require(real_ipfs is not None, "real Kubo CLI missing")
            wrapper = cli / "ipfs"
            wrapper.write_text("#!/bin/sh\nexec " + shlex.quote(real_ipfs) + " --api "
                               + shlex.quote(api) + ' "$@"\n')
            wrapper.chmod(0o700)
            recovery_env = dict(env, PATH=str(cli) + os.pathsep + env["PATH"])
            run("authenticated-shell-recovery", ["sh", REPO / "scripts/install.sh", "--recover",
                "--release", first, "--trusted-release-keys", trust, "--ipfs-url", endpoint], env=recovery_env)
            require(not journal.exists(), "shell recovery left pending pre-replacement journal")
            require(snapshot(recovery_home) == preserved, "shell recovery changed original installation/private bytes")
            checks.append("genuine alternate Host refused by native compatibility; authenticated shell aborts pre-replacement journal")

    report = {"status": "PASS", "checks": checks, "releases": [identity for identity, _ in releases],
              "host_sha256": sha256(binary), "commands": len(commands),
              "derived_fhs": "custom-section fixture; not original CI producer-byte evidence",
              "not_qualified": ["changed native replacement", "native restart/readiness/rollback", "platform service lifecycle",
                                "automatic channel freshness", "actual process kill during FHS update"]}
    (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
