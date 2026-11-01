#!/usr/bin/env python3
"""Build and run the real Cell probe under the shared build/benchmark lock.

The stack must already be running for live evidence and enabled cost delivery.
Disabled, unavailable and sample-zero modes need no collector.
All output remains local; this script does not publish GitHub content.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parent.parent
LOCK = Path("/tmp/ww-fuel-coordination/build-benchmark.lock")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=["live", "disabled", "unavailable", "sample-zero", "cost"], default="live")
    parser.add_argument("--output", type=Path, default=ROOT / ".context/observability")
    parser.add_argument("--baseline", type=Path, help="identical probe built against master; used only for cost comparison")
    parser.add_argument("--repetitions", type=int, default=3, help="cost process repetitions, 1–10")
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    if args.baseline:
        args.baseline = args.baseline.resolve(strict=True)
    if not 1 <= args.repetitions <= 10:
        parser.error("--repetitions must be between 1 and 10")
    LOCK.parent.mkdir(parents=True, exist_ok=True)
    with LOCK.open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        with (args.output / "build.log").open("w") as log:
            build = subprocess.run(["cargo", "test", "--locked", "--test", "observability_incident", "--no-run", "--message-format=json"], cwd=ROOT, stdout=subprocess.PIPE, stderr=log, text=True)
        (args.output / "build.jsonl").write_text(build.stdout)
        if build.returncode:
            sys.exit(f"Build failed: {args.output / 'build.log'}")
        binary = None
        for line in build.stdout.splitlines():
            record = json.loads(line)
            if record.get("reason") == "compiler-artifact" and record.get("target", {}).get("name") == "observability_incident":
                binary = record.get("executable") or binary
        if binary is None:
            sys.exit("Cargo did not report the probe executable")
        env = os.environ.copy()
        for name in list(env):
            if name.startswith("WW_TRACE_") or name in {"WW_OTLP_ENDPOINT", "WW_CWASM_DIR", "WW_CWASM_CACHE_MAX_BYTES"}:
                del env[name]
        env.update(WW_LOG_FORMAT="json", RUST_LOG="ww=info,cell=info,cache=info,rpc=info", WW_OBSERVABILITY_OUTPUT=str(args.output))
        if args.mode == "live":
            env.update(WW_OTLP_ENDPOINT="http://127.0.0.1:14318/v1/traces", WW_TRACE_SAMPLE_RATIO="1", WW_TRACE_MAX_SPANS_PER_SECOND="100", WW_OBSERVABILITY_HOLD_SECONDS="10")
            env.setdefault("WW_OBSERVABILITY_ADMIN", "127.0.0.1:19091")
        elif args.mode == "unavailable":
            env.update(WW_OTLP_ENDPOINT="http://127.0.0.1:1/v1/traces", WW_TRACE_SAMPLE_RATIO="1")
        elif args.mode == "sample-zero":
            env.update(WW_OTLP_ENDPOINT="http://127.0.0.1:1/v1/traces", WW_TRACE_SAMPLE_RATIO="0")
        modes = (["baseline"] if args.baseline else []) + ["disabled", "enabled"]
        runs = [(mode, repetition) for repetition in range(args.repetitions) for mode in (modes if repetition % 2 == 0 else list(reversed(modes)))] if args.mode == "cost" else [(args.mode, 0)]
        reports = []
        for run, repetition in runs:
            run_env = env.copy()
            run_output = args.output / f"{run}-{repetition}" if args.mode == "cost" else args.output
            run_output.mkdir(exist_ok=True)
            run_env["WW_OBSERVABILITY_OUTPUT"] = str(run_output)
            if args.mode == "cost" and run == "enabled":
                run_env.update(WW_OTLP_ENDPOINT="http://127.0.0.1:14318/v1/traces", WW_TRACE_SAMPLE_RATIO="1", WW_TRACE_MAX_SPANS_PER_SECOND="1000")
            probe = "cost_probe" if args.mode == "cost" else "incident_probe"
            log_name = "runtime.jsonl"
            start = time.monotonic()
            with (run_output / log_name).open("w") as log:
                result = subprocess.run([str(args.baseline) if run == "baseline" else binary, "--exact", probe, "--ignored", "--nocapture"], cwd=ROOT, env=run_env, stdout=subprocess.PIPE, stderr=log, text=True, timeout=60)
            (run_output / f"{run}-stdout.txt").write_text(result.stdout)
            report = {"mode": run, "repetition": repetition, "exit_code": result.returncode, "process_seconds": time.monotonic() - start, "log_bytes": (run_output / log_name).stat().st_size}
            if args.mode == "cost":
                samples = [json.loads(line.partition("=")[2]) for line in result.stdout.splitlines() if line.startswith("OBSERVABILITY_COST=")]
                durations = [sample["wall_ns"] for sample in samples if not sample["warmup"]]
                if durations:
                    report.update(samples=len(durations), median_wall_ns=statistics.median(durations), min_wall_ns=min(durations), max_wall_ns=max(durations))
                report["limits"] = "debug build; tiny real P3 command; process order alternates; descriptive local comparison, not throughput qualification"
                telemetry = run_output / "telemetry.prom"
                if telemetry.exists():
                    report["telemetry"] = {line.split()[0]: float(line.split()[1]) for line in telemetry.read_text().splitlines() if line and not line.startswith("#")}
            reports.append(report)
            print(json.dumps(report))
            if result.returncode:
                sys.exit(f"Probe failed: {run_output / (run + '-stdout.txt')}")
        (args.output / f"{args.mode}-report.json").write_text(json.dumps(reports, indent=2) + "\n")


if __name__ == "__main__":
    main()
