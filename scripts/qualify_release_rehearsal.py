#!/usr/bin/env python3
"""Rehearse the public CAR boundary with genuine CI artifacts and disposable trust.

This tool does not admit a production workflow, publish a release, or register a
service. Supply the CAR digest and source/run identity from independent CI reads.
The fresh output directory retains commands, receipts, packages, and test keys.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess


PLATFORMS = {
    "linux-x86_64": ("x86_64-unknown-linux-gnu", "linux/x86_64", "elf", 62),
    "linux-aarch64": ("aarch64-unknown-linux-gnu", "linux/aarch64", "elf", 183),
    "macos-aarch64": ("aarch64-apple-darwin", "macos/aarch64", "macho", 0x100000C),
    "macos-x86_64": ("x86_64-apple-darwin", "macos/x86_64", "macho", 0x1000007),
    "linux-musl-x86_64": ("x86_64-unknown-linux-musl", "linux/musl-x86_64", "elf", 62),
}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def read_varint(data, offset):
    value = 0
    for index in range(10):
        byte = data[offset]
        offset += 1
        value |= (byte & 127) << (7 * index)
        if byte < 128:
            return value, offset
    raise RuntimeError("invalid test CAR framing")


def varint(value):
    result = bytearray()
    while value >= 128:
        result.append((value & 127) | 128)
        value >>= 7
    return bytes(result) + bytes([value])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package-tool", type=Path, required=True)
    parser.add_argument("--input-car", type=Path, required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--run-id", type=int, required=True)
    parser.add_argument("--attempt", type=int, default=1)
    parser.add_argument("--channel", required=True, help="disposable publisher's canonical IPNS name")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    tool = args.package_tool.resolve(strict=True)
    source = args.input_car.resolve(strict=True)
    require(len(args.revision) == 40 and all(c in "0123456789abcdef" for c in args.revision),
            "expected a full source revision")
    require(digest(source) == args.sha256.removeprefix("sha256:"), "CI CAR digest mismatch")
    work = args.output.resolve()
    work.mkdir(mode=0o700)
    repo = Path(__file__).resolve().parents[1]
    commands = []

    def run(name, command, failure=False):
        command = [str(part) for part in command]
        result = subprocess.run(command, capture_output=True, text=True, timeout=300)
        (work / (name + ".stdout")).write_text(result.stdout)
        (work / (name + ".stderr")).write_text(result.stderr)
        commands.append({"name": name, "argv": command, "returncode": result.returncode,
                         "expected_failure": failure})
        (work / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")
        # Rust's Result-returning CLI exits 1 for a rejected input. A panic,
        # signal, or argument-parser failure is not a successful negative test.
        require(result.returncode == (1 if failure else 0),
                f"unexpected command result: {name}; inspect {work / (name + '.stderr')}")
        if failure:
            require(failure in result.stderr, f"unexpected rejection reason: {name}")
        return result.stdout

    def package(name, *command, failure=False):
        return run(name, [tool, *command], failure=failure)

    def keypair(name):
        directory = work / name
        directory.mkdir(mode=0o700)
        private = directory / "test-only-private.pem"
        descriptor = os.open(private, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        os.close(descriptor)
        run(name + "-generate", ["openssl", "genpkey", "-algorithm", "ED25519", "-out", private])
        public = directory / "public.der"
        run(name + "-public", ["openssl", "pkey", "-in", private, "-pubout", "-outform", "DER", "-out", public])
        trust = directory / "trusted-keys"
        trust.write_bytes(base64.b64encode(public.read_bytes()) + b"\n")
        return private, trust

    imported = json.loads(package("input-import", "import-car", "--car", source,
                                  "--spool", work / "input-spool"))
    package("input-unpack", "unpack-tree", "--spool", work / "input-spool", "--tree", work / "input")
    tree = work / "input/release-tree"
    require((tree / "REVISION").read_text().strip() == args.revision, "release revision mismatch")
    hosts, receipts = {}, []
    for name, (target, location, kind, machine) in PLATFORMS.items():
        receipt = work / "input/artifacts" / ("ww-" + name)
        binary = tree / "bin/ww" / location / "ww"
        checksum = digest(binary)
        require((receipt / "SHA256SUMS").read_text().splitlines() == [checksum + "  ww"],
                f"original checksum mismatch: {target}")
        require((receipt / "REVISION").read_text().strip() == args.revision,
                f"original revision mismatch: {target}")
        provenance = json.loads((receipt / "PROVENANCE.json").read_text())
        require(provenance == {"repository": "https://github.com/wetware/ww",
                               "revision": args.revision,
                               "builder": f"github-actions:{args.run_id}:{args.attempt}:{target}"},
                f"original producer mismatch: {target}")
        with binary.open("rb") as stream:
            header = stream.read(64)
        if kind == "elf":
            require(header[:6] == b"\x7fELF\x02\x01" and struct.unpack_from("<H", header, 18)[0] == machine,
                    f"ELF target mismatch: {target}")
        else:
            require(header[:4] == b"\xcf\xfa\xed\xfe" and struct.unpack_from("<I", header, 4)[0] == machine,
                    f"Mach-O target mismatch: {target}")
        hosts[target] = provenance
        receipts.append({"target": target, "sha256": checksum, "size": binary.stat().st_size,
                         "provenance": provenance})

    metadata = {"channel": args.channel, "sequence": 1,
                "version": (tree / "VERSION").read_text().strip(),
                "source": {"repository": "https://github.com/wetware/ww", "revision": args.revision,
                           "builder": f"disposable-rehearsal:{args.run_id}:{args.attempt}"},
                "hosts": hosts}
    (work / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    private, signer_trust = keypair("signer")
    # Provision verifier trust from the local test identity, outside release content.
    verifier = work / "verifier-trust"
    verifier.write_bytes(signer_trust.read_bytes())
    for suffix in ("a", "b"):
        candidate = work / ("tree-" + suffix)
        spool = work / ("spool-" + suffix)
        shutil.copytree(tree, candidate)
        package("prepare-" + suffix, "prepare", "--tree", candidate, "--spool", spool,
                "--metadata", work / "metadata.json")
        run("sign-" + suffix, ["bash", repo / "scripts/sign_release.sh", candidate / "CHECKSUMS.txt",
                               candidate / "CHECKSUMS.txt.sig", private, signer_trust])
        package("finalize-" + suffix, "finalize", "--tree", candidate, "--spool", spool,
                "--trusted-keys", verifier)
        package("export-" + suffix, "export-car", "--spool", spool, "--car", work / ("release-" + suffix + ".car"))
    require((work / "finalize-a.stdout").read_bytes() == (work / "finalize-b.stdout").read_bytes(),
            "repeated construction changed signed identity")
    signed = work / "release-a.car"
    require(digest(signed) == digest(work / "release-b.car"), "CAR export is not deterministic")
    expected = json.loads((work / "finalize-a.stdout").read_text())
    package("signed-import", "import-car", "--car", signed, "--spool", work / "received-spool")
    verified = json.loads(package("independent-verify", "verify", "--spool", work / "received-spool",
                                 "--trusted-keys", verifier))
    require(verified == expected, "transport changed signed release identity")

    # A second signed sequence reuses genuine producer bytes. The private
    # disposable harness uses it for ordering/crash tests, never fake Hosts.
    second_tree, second_spool = work / "tree-next", work / "spool-next"
    shutil.copytree(tree, second_tree)
    metadata["sequence"] = 2
    (work / "metadata-next.json").write_text(json.dumps(metadata, indent=2) + "\n")
    package("prepare-next", "prepare", "--tree", second_tree, "--spool", second_spool,
            "--metadata", work / "metadata-next.json")
    run("sign-next", ["bash", repo / "scripts/sign_release.sh", second_tree / "CHECKSUMS.txt",
                      second_tree / "CHECKSUMS.txt.sig", private, signer_trust])
    second = json.loads(package("finalize-next", "finalize", "--tree", second_tree,
                                "--spool", second_spool, "--trusted-keys", verifier))
    package("export-next", "export-car", "--spool", second_spool, "--car", work / "release-next.car")
    require(second["sequence"] > expected["sequence"] and second["root"] != expected["root"],
            "expected distinct signed release sequences")
    for name, candidate_tree, car, identity in (
        ("candidate", work / "tree-b", signed, expected),
        ("candidate-next", second_tree, work / "release-next.car", second),
    ):
        receipt = {"car": str(car), "bytes": car.stat().st_size, "sha256": digest(car),
                   "synthetic_hosts": False, "channel": args.channel, "candidate": identity,
                   "files": {p.relative_to(candidate_tree).as_posix(): digest(p)
                             for p in sorted(candidate_tree.rglob("*"))
                             if p.is_file() and p.name != "RELEASE-DESCRIPTOR.cbor"}}
        (work / (name + ".json")).write_text(json.dumps(receipt, indent=2) + "\n")

    _, wrong_trust = keypair("untrusted-signer")
    package("wrong-key", "verify", "--spool", work / "received-spool", "--trusted-keys", wrong_trust,
            failure="release manifest signature is not authorized")

    # Mutate opaque records; production parsers remain the acceptance oracle.
    data = signed.read_bytes()
    header_length, offset = read_varint(data, 0)
    offset += header_length
    header = data[:offset]
    records = []
    while offset < len(data):
        start = offset
        length, offset = read_varint(data, offset)
        offset += length
        require(offset <= len(data), "unexpected exporter framing")
        records.append(data[start:offset])
    require(len(records) > 5, "expected a multi-block genuine release")
    mutations = {
        "truncated": (data[:-7], "truncated CAR section"),
        "missing-block": (header + b"".join(records[:-1]), "No such file"),
        "duplicate-block": (data + records[0], "duplicate CAR block"),
        "tampered-block": (data[:-1] + bytes([data[-1] ^ 1]), "release block CID mismatch"),
        "oversized-section": (header + varint(2 * 1024 * 1024 + 97), "CAR block section exceeds limit"),
    }
    for name, (payload, error) in mutations.items():
        path = work / (name + ".car")
        path.write_bytes(payload)
        spool = work / (name + "-spool")
        package(name, "import-car", "--car", path, "--spool", spool, failure=error)
        require(not spool.exists(), f"failed import retained a partial spool: {name}")
    reordered = work / "reordered.car"
    reordered.write_bytes(header + b"".join(reversed(records)))
    package("reordered-import", "import-car", "--car", reordered, "--spool", work / "reordered-spool")
    reordered_identity = json.loads(package("reordered-verify", "verify", "--spool", work / "reordered-spool",
                                           "--trusted-keys", verifier))
    require(reordered_identity == expected, "record reordering changed signed identity")
    altered_host = work / "tree-a/bin/ww/linux/x86_64/ww"
    with altered_host.open("ab") as stream:
        stream.write(b"tamper")
    package("changed-signed-host", "finalize", "--tree", work / "tree-a", "--spool", work / "spool-a",
            "--trusted-keys", verifier, failure="artifact root differs from descriptor")
    report = {"status": "PASS", "scope": "offline public package and CAR rehearsal; no publication admission",
              "source_revision": args.revision, "run_id": args.run_id, "attempt": args.attempt,
              "tools_sha256": {"package": digest(tool), "qualification": digest(Path(__file__)),
                               "signer": digest(repo / "scripts/sign_release.sh")},
              "input_sha256": digest(source), "input_root": imported["root"], "hosts": receipts,
              "signed": expected, "next_signed": second,
              "signed_car_sha256": digest(signed), "signed_car_bytes": len(data),
              "reachable_blocks": len(records), "rejected": [*mutations, "wrong-key", "changed-signed-host"],
              "reordered_records": "accepted with identical authenticated identity",
              "deterministic_descriptor_root_car": True,
              "not_qualified": ["protected workflow admission", "publication", "consumer lifecycle",
                                "embedded revision inspection", "live service upgrade"]}
    (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
