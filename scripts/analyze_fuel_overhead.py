#!/usr/bin/env python3
"""Compare alternating baseline/instrumented fuel and HTTP benchmark logs."""

import argparse
import collections
import json
import math
import statistics


def read(path):
    values = collections.defaultdict(list)
    sizes = {}
    with open(path) as source:
        for line in source:
            start = line.find("{")
            if start < 0:
                continue
            try:
                row = json.loads(line[start:])
            except json.JSONDecodeError:
                continue
            if row.get("benchmark") == "fuel":
                key = (row["case"], row["workers"])
                # Per-worker batch latency; never divide contention cost by
                # worker count. Compute/I/O ratios are unchanged by this scale.
                values[key].append(row["elapsed_ns"] / (row["operations"] / row["workers"]))
            elif "http_ns" in row:
                key = ("http:" + row["mode"] + ":" + row["phase"], 0)
                values[key].extend(row["http_ns"])
                sizes[key] = row["body_bytes"]
    if not values:
        raise ValueError(f"no benchmark samples in {path}")
    return values, sizes


def quantile(values, fraction):
    return sorted(values)[math.ceil(len(values) * fraction) - 1]


def summarize(values):
    return {
        "n": len(values),
        "median_ns": statistics.median(values),
        "p95_ns": quantile(values, 0.95),
        "p05_ns": quantile(values, 0.05),
    }


def comparison(key, baseline, instrumented):
    before, after = summarize(baseline), summarize(instrumented)
    return {
        "case": key[0],
        "workers": key[1],
        "baseline": before,
        "instrumented": after,
        "median_added_ns": after["median_ns"] - before["median_ns"],
        "median_change_pct": 100 * (after["median_ns"] / before["median_ns"] - 1),
        "p95_added_ns": after["p95_ns"] - before["p95_ns"],
        "p95_change_pct": 100 * (after["p95_ns"] / before["p95_ns"] - 1),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pair", nargs=2, action="append", required=True,
                        metavar=("BASELINE", "INSTRUMENTED"))
    args = parser.parse_args()
    pooled = [collections.defaultdict(list), collections.defaultdict(list)]
    expected_keys = None
    for pair_index, paths in enumerate(args.pair, 1):
        (before, before_sizes), (after, after_sizes) = map(read, paths)
        if before.keys() != after.keys():
            raise ValueError(f"case mismatch in pair {pair_index}")
        if expected_keys is not None and before.keys() != expected_keys:
            raise ValueError(f"case mismatch across pairs at {pair_index}")
        expected_keys = before.keys()
        for key in sorted(before):
            row = comparison(key, before[key], after[key])
            row["pair"] = pair_index
            if key in before_sizes:
                row["added_body_bytes"] = after_sizes[key] - before_sizes[key]
            print(json.dumps(row, sort_keys=True))
            pooled[0][key].extend(before[key])
            pooled[1][key].extend(after[key])
    for key in sorted(pooled[0]):
        row = comparison(key, pooled[0][key], pooled[1][key])
        row["pair"] = "pooled"
        print(json.dumps(row, sort_keys=True))


if __name__ == "__main__":
    main()
