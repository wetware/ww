#!/usr/bin/env python3
"""Emit descriptor metadata from the exact five-platform producer receipts."""
import argparse
import hashlib
import json
from pathlib import Path
import re

TARGETS = {
    "linux-x86_64": "x86_64-unknown-linux-gnu",
    "linux-aarch64": "aarch64-unknown-linux-gnu",
    "macos-aarch64": "aarch64-apple-darwin",
    "macos-x86_64": "x86_64-apple-darwin",
    "linux-musl-x86_64": "x86_64-unknown-linux-musl",
}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--tree", required=True, type=Path)
    parser.add_argument("--channel", required=True)
    parser.add_argument("--sequence", required=True, type=int)
    parser.add_argument("--builder", required=True)
    args = parser.parse_args()
    revision = (args.tree / "REVISION").read_text().strip()
    if not re.fullmatch("[0-9a-f]{40}", revision) or args.sequence <= 0:
        parser.error("invalid release revision or sequence")
    def provenance(rev):
        return {"repository": "https://github.com/wetware/ww", "revision": rev, "builder": args.builder}
    hosts = {}
    for platform, target in TARGETS.items():
        directory = args.artifacts / ("ww-" + platform)
        for name in ("REVISION", "SHA256SUMS", "PROVENANCE.json"):
            if (directory / name).is_symlink() or not (directory / name).is_file():
                parser.error("invalid producer receipt: " + str(directory / name))
        actual_revision = (directory / "REVISION").read_text().strip()
        if not re.fullmatch("[0-9a-f]{40}", actual_revision):
            parser.error("invalid Host build revision")
        platform_path = {
            "linux-x86_64": "linux/x86_64", "linux-aarch64": "linux/aarch64",
            "macos-aarch64": "macos/aarch64", "macos-x86_64": "macos/x86_64",
            "linux-musl-x86_64": "linux/musl-x86_64",
        }[platform]
        binary = args.tree / "bin/ww" / platform_path / "ww"
        if binary.is_symlink() or not binary.is_file():
            parser.error("invalid staged Host: " + str(binary))
        with binary.open("rb") as host:
            digest = hashlib.file_digest(host, "sha256").hexdigest()
        if (directory / "SHA256SUMS").read_text() != digest + "  ww\n":
            parser.error("Host checksum receipt mismatch")
        receipt = json.loads((directory / "PROVENANCE.json").read_text())
        if (set(receipt) != {"repository", "revision", "builder"}
                or receipt["repository"] != "https://github.com/wetware/ww"
                or receipt["revision"] != actual_revision
                or not isinstance(receipt["builder"], str) or not receipt["builder"]):
            parser.error("Host provenance receipt mismatch")
        hosts[target] = receipt
    print(json.dumps({"channel": args.channel, "sequence": args.sequence,
        "version": (args.tree / "VERSION").read_text().strip(),
        "source": provenance(revision), "hosts": hosts}, sort_keys=True))

if __name__ == "__main__":
    main()
