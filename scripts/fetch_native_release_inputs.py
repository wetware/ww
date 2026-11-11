#!/usr/bin/env python3
"""Download the pinned original pair using read-only GitHub CLI requests."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import subprocess

from verify_native_release_inputs import read_json, require, validate_pins


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pins", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    pins = read_json(args.pins)
    validate_pins(pins)
    require(not args.output.exists(), "download output must be absent")
    args.output.mkdir(mode=0o700, parents=True)
    help_text = subprocess.check_output(["gh", "api", "--help"], text=True, timeout=15)
    raw = ["--allow-escape-sequences"] if "--allow-escape-sequences" in help_text else []
    requests = []
    for label, pin in pins["producers"].items():
        run, job, artifact = pin["run_id"], pin["build_job_id"], pin["artifact_id"]
        for filename, endpoint in [
            (label + "-run.json", f"runs/{run}"),
            (label + "-artifact.json", f"artifacts/{artifact}"),
            (label + "-build-job.json", f"jobs/{job}"),
            (label + "-jobs.json", f"runs/{run}/attempts/1/jobs?per_page=100"),
            (label + "-build.log", f"jobs/{job}/logs"),
            (label + ".zip", f"artifacts/{artifact}/zip"),
        ]:
            requests.append((filename, "repos/wetware/ww/actions/" + endpoint))

    def fetch(request):
        filename, endpoint = request
        path = args.output / filename
        with path.open("xb") as stream:
            subprocess.run(["gh", "api", *raw, "--method", "GET", endpoint],
                           stdout=stream, check=True, timeout=180)
        with path.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        return {"file": filename, "endpoint": endpoint, "sha256": digest,
                "bytes": path.stat().st_size}

    with ThreadPoolExecutor(max_workers=4) as pool:
        receipts = list(pool.map(fetch, requests))
    (args.output / "fetch-receipts.json").write_text(json.dumps(receipts, indent=2) + "\n")


if __name__ == "__main__":
    main()
