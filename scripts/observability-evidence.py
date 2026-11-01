#!/usr/bin/env python3
"""Verify the metric → structured event → exported span chain in the local stack."""

import argparse
import base64
from datetime import datetime
import json
from pathlib import Path
import time
from urllib.parse import urlencode
from urllib.request import urlopen


def get_json(url):
    with urlopen(url, timeout=5) as response:
        return json.load(response)


def spans(value):
    if isinstance(value, dict):
        if "spanId" in value and "name" in value:
            yield value
        for child in value.values():
            yield from spans(child)
    elif isinstance(value, list):
        for child in value:
            yield from spans(child)


def identifier(value):
    # Tempo's protobuf JSON versions can encode byte identifiers as base64.
    if all(char in "0123456789abcdefABCDEF" for char in value) and len(value) in (16, 32):
        return value.lower()
    return base64.b64decode(value).hex()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path(".context/observability"))
    args = parser.parse_args()
    records = [json.loads(line) for line in (args.output / "runtime.jsonl").read_text().splitlines() if line.startswith("{")]
    events = [record for record in records if record.get("fields", {}).get("event_code") == 300]
    assert len(events) == 1, "expected one real instantiation timeout event"
    event = events[0]
    event_seconds = datetime.fromisoformat(event["timestamp"].replace("Z", "+00:00")).timestamp()
    trace_id, span_id = event["trace_id"], event["span_id"]
    assert event["fields"]["elapsed_ms"] >= event["fields"]["timeout_ms"] == 5000
    deadline = time.monotonic() + 40
    while True:
        try:
            trace = get_json(f"http://127.0.0.1:13200/api/traces/{trace_id}")
            selected = [span for span in spans(trace) if identifier(span["spanId"]) == span_id]
            assert len(selected) == 1, "log span_id missing or duplicated in exported trace"
            span = selected[0]
            assert identifier(span["traceId"]) == trace_id, "trace payload disagrees with the correlated log"
            assert span["name"] == "cell"
            duration_ns = int(span["endTimeUnixNano"]) - int(span["startTimeUnixNano"])
            assert duration_ns >= 5_000_000_000, "Cell span must cover the five-second timeout"
            exported_events = span.get("events", [])
            attributes = [{attribute["key"]: attribute.get("value", {}).get("intValue") for attribute in item.get("attributes", [])} for item in exported_events]
            timeout_events = [fields for fields in attributes if str(fields.get("event_code")) == "300"]
            assert len(timeout_events) == 1, "exported span lacks the unique runtime timeout event"
            for field in ("elapsed_ms", "timeout_ms"):
                assert int(timeout_events[0][field]) == event["fields"][field], f"exported {field} disagrees with log"
            loki = get_json("http://127.0.0.1:13100/loki/api/v1/query_range?" + urlencode({"query": '{job="wetware"} |= "' + trace_id + '"', "limit": "100", "start": str(time.time_ns() - 600_000_000_000), "end": str(time.time_ns())}))
            assert any(json.loads(line).get("span_id") == span_id for stream in loki["data"]["result"] for _, line in stream["values"]), "correlated event not ingested in Loki"
            prom = get_json("http://127.0.0.1:19090/api/v1/query_range?" + urlencode({"query": 'ww_wasmtime_instantiations_total{job="wetware",outcome="timeout",instance="host.docker.internal:19091"}', "start": str(event_seconds - 8), "end": str(event_seconds + 5), "step": "1"}))
            series = prom["data"]["result"]
            assert len(series) == 1, "expected the current fixture scrape target"
            samples = series[0]["values"]
            assert any(timestamp < event_seconds and float(value) == 0 for timestamp, value in samples), "Prometheus did not observe the current run's zero baseline"
            assert any(timestamp > event_seconds and float(value) == 1 for timestamp, value in samples), "Prometheus did not observe the current run's timeout increment"
            break
        except Exception:
            if time.monotonic() >= deadline:
                raise
            time.sleep(1)
    evidence = {"trace_id": trace_id, "span_id": span_id, "event_timestamp": event["timestamp"], "owner_id": event["span"]["owner_id"], "cell_span_duration_ns": duration_ns, "elapsed_ms": event["fields"]["elapsed_ms"], "timeout_ms": event["fields"]["timeout_ms"], "prometheus_zero_then_timeout_observed": True, "loki_correlation_observed": True, "tempo_timeout_event_observed": True, "unavailable": "The recorded timeout does not establish why the epoch publisher stopped advancing."}
    for name, data in (("trace.json", trace), ("loki.json", loki), ("prometheus.json", prom), ("evidence-chain.json", evidence)):
        (args.output / name).write_text(json.dumps(data, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))


if __name__ == "__main__":
    main()
