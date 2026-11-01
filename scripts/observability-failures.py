#!/usr/bin/env python3
"""Exercise collector failure while real Cells run; requires a built probe binary."""

import argparse
import fcntl
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import subprocess
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path, default=Path(".context/observability/failures"))
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    reports = []
    with open("/tmp/ww-fuel-coordination/build-benchmark.lock", "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for case in ("collector-stops", "slow-saturated"):
            received = []
            slow = case == "slow-saturated"

            class Collector(BaseHTTPRequestHandler):
                def do_POST(self):
                    size = int(self.headers.get("Content-Length", "0"))
                    assert size <= 2 * 1024 * 1024
                    body = self.rfile.read(size)
                    received.append({"bytes": len(body), "time": time.monotonic()})
                    if slow:
                        time.sleep(2)
                    try:
                        self.send_response(200)
                        self.send_header("Content-Length", "2")
                        self.end_headers()
                        self.wfile.write(b"{}")
                    except (BrokenPipeError, ConnectionResetError):
                        pass

                def log_message(self, *_):
                    pass

            server = HTTPServer(("127.0.0.1", 0), Collector)
            server.timeout = 1
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            stopped = threading.Event()

            def stop():
                server.shutdown()
                server.server_close()
                stopped.set()

            timer = None
            if not slow:
                timer = threading.Timer(2, stop)
                timer.start()
            directory = args.output / case
            directory.mkdir(exist_ok=True)
            env = {key: value for key, value in os.environ.items() if not key.startswith(("WW_TRACE_", "WW_OBSERVABILITY_")) and key not in {"WW_OTLP_ENDPOINT", "WW_CWASM_DIR", "WW_CWASM_CACHE_MAX_BYTES"}}
            env.update(WW_LOG_FORMAT="json", RUST_LOG="ww=info,cell=info,cache=info,rpc=info", WW_OTLP_ENDPOINT=f"http://127.0.0.1:{server.server_port}/v1/traces", WW_TRACE_SAMPLE_RATIO="1", WW_TRACE_MAX_SPANS_PER_SECOND="10000", WW_OBSERVABILITY_OUTPUT=str(directory.resolve()), WW_OBSERVABILITY_SAMPLES="1000")
            start = time.monotonic()
            try:
                with (directory / "runtime.jsonl").open("w") as log:
                    result = subprocess.run([str(args.binary.resolve()), "--exact", "cost_probe" if slow else "incident_probe", "--ignored", "--nocapture"], env=env, stdout=subprocess.PIPE, stderr=log, text=True, timeout=60)
                elapsed = time.monotonic() - start
                (directory / "stdout.txt").write_text(result.stdout)
                assert result.returncode == 0, f"Cell correctness failed in {case}"
                metrics = (directory / "telemetry.prom").read_text()
                values = {line.split()[0]: float(line.split()[1]) for line in metrics.splitlines() if line and not line.startswith("#")}
                assert values['ww_telemetry_dropped_total{reason="export"}'] > 0, "export failure was not counted"
                if slow:
                    assert values['ww_telemetry_dropped_total{reason="queue"}'] > 0, "queue was not saturated"
                    assert result.stdout.count("OBSERVABILITY_COST=") == 1000
                else:
                    assert received, "collector did not accept telemetry before stopping"
                    assert stopped.is_set(), "collector did not stop during execution"
                drain = [int(line.partition("=")[2]) for line in result.stdout.splitlines() if line.startswith("OBSERVABILITY_DRAIN_NS=")]
                assert len(drain) == 1, "missing measured exporter shutdown"
                assert drain[0] < 2_500_000_000, "exporter drain exceeded its two-second guard budget plus scheduling allowance"
                reports.append({"case": case, "cell_correctness_passed": True, "process_seconds": elapsed, "drain_ns": drain[0], "requests_received": len(received), "request_bytes": sum(request["bytes"] for request in received), "metrics": values})
            finally:
                if timer:
                    timer.cancel()
                if not stopped.is_set():
                    stop()
                thread.join(timeout=3)
        (args.output / "report.json").write_text(json.dumps(reports, indent=2) + "\n")
        print(json.dumps(reports, indent=2))


if __name__ == "__main__":
    main()
