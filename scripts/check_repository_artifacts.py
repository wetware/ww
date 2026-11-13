#!/usr/bin/env python3
"""Reject native headers, oversized blobs and retired release fixtures in Git.

Scan committed Git bytes, not a possibly modified checkout. The 1 MiB ceiling
allows existing intentional CAR/ZIP/WASM fixtures and measurement files. Archive
contents are not inspected; an archive over the ceiling is rejected as a blob.
"""
import argparse
import os
import subprocess
import sys

MAX_BLOB_BYTES = 1024 * 1024
RETIRED_FIXTURES = "tests/fixtures/native-release-originals"
NATIVE_MAGIC = {b"\x7fELF", b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe",
                b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe",
                b"\xbe\xba\xfe\xca", b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"}


def native_binary(raw):
    if raw[:4] in NATIVE_MAGIC:
        return True
    if raw[:2] == b"MZ" and len(raw) >= 64:
        offset = int.from_bytes(raw[60:64], "little")
        return raw[offset:offset + 4] == b"PE\0\0"
    return False


def violations(revision="HEAD"):
    tree = subprocess.check_output(["git", "rev-parse", "--verify", "--end-of-options",
                                    revision + "^{tree}"]).strip()
    listing = subprocess.check_output(["git", "ls-tree", "-rlz", tree])
    errors, inspect = [], []
    for entry in listing.split(b"\0"):
        if not entry:
            continue
        metadata, name = entry.split(b"\t", 1)
        mode, kind, oid, size = metadata.split()
        path = os.fsdecode(name)
        if path == RETIRED_FIXTURES or path.startswith(RETIRED_FIXTURES + "/"):
            errors.append(f"{path!r}: retired historical release fixture path")
        elif kind == b"blob":
            if int(size) > MAX_BLOB_BYTES:
                errors.append(f"{path!r}: {int(size)} bytes exceeds the 1 MiB source-repository limit")
            elif mode in (b"100644", b"100755"):
                inspect.append((oid, path))
    # Use one Git process; never read the oversized blobs into Python.
    with subprocess.Popen(["git", "cat-file", "--batch"], stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE) as git:
        for oid, path in inspect:
            git.stdin.write(oid + b"\n")
            git.stdin.flush()
            actual, kind, size = git.stdout.readline().split()
            if actual != oid or kind != b"blob":
                raise RuntimeError("unexpected Git object response")
            raw = git.stdout.read(int(size))
            if len(raw) != int(size) or git.stdout.read(1) != b"\n":
                raise RuntimeError("truncated Git object response")
            if native_binary(raw):
                errors.append(f"{path!r}: compiled native executable/object header")
        git.stdin.close()
        if git.wait() != 0:
            raise RuntimeError("Git object scan failed")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revision", default="HEAD", help="committed tree to inspect (default: HEAD)")
    errors = violations(parser.parse_args().revision)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("Repository artifact check passed: no native headers, blobs over 1 MiB, or retired fixture paths in the checked tree.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
