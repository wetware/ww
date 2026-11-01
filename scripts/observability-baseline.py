#!/usr/bin/env python3
"""Build the same cost workload against the audited master revision, locally."""

import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
REVISION = "f7881b75d96f548066807e8ed625f878be4ba738"


def main():
    output = ROOT / ".context/observability"
    output.mkdir(parents=True, exist_ok=True)
    lock_path = Path("/tmp/ww-fuel-coordination/build-benchmark.lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        source = Path(tempfile.mkdtemp(prefix="baseline-source-", dir=output))
        subprocess.run(["git", "worktree", "add", "--detach", str(source), REVISION], cwd=ROOT, check=True)
        harness = (ROOT / "tests/observability_incident.rs").read_text()
        prefix, separator, rest = harness.partition("fn telemetry_metrics() {")
        assert separator, "missing post-measurement telemetry helper"
        body, separator, suffix = rest.partition("\n}\n")
        assert separator and "ww::telemetry::render_metrics" in body
        # Master has no exporter counters. This helper runs after measured work.
        harness = prefix + "fn telemetry_metrics() {}\n" + suffix
        (source / "tests/observability_incident.rs").write_text(harness)
        env = os.environ.copy()
        env["CARGO_TARGET_DIR"] = str(ROOT / "target")
        with (output / "baseline-build.log").open("w") as log:
            build = subprocess.run(["cargo", "test", "--locked", "--test", "observability_incident", "--no-run", "--message-format=json"], cwd=source, env=env, stdout=subprocess.PIPE, stderr=log, text=True)
        (output / "baseline-build.jsonl").write_text(build.stdout)
        build.check_returncode()
        executable = None
        for line in build.stdout.splitlines():
            record = json.loads(line)
            if record.get("reason") == "compiler-artifact" and record.get("target", {}).get("name") == "observability_incident":
                executable = record.get("executable") or executable
        assert executable, "Cargo did not report the baseline executable"
        destination = output / "rebuilt-master-probe"
        shutil.copy2(executable, destination)
        sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip()
        with destination.open("rb") as binary:
            digest = hashlib.file_digest(binary, "sha256").hexdigest()
        metadata = {"git_sha": sha, "source": str(source), "executable": str(destination), "sha256": digest, "adaptation": "telemetry_metrics helper replaced with no-op after measured work; Cell workload unchanged", "profile": "debug"}
        (output / "rebuilt-baseline.json").write_text(json.dumps(metadata, indent=2) + "\n")
        print(json.dumps(metadata, indent=2))


if __name__ == "__main__":
    main()
