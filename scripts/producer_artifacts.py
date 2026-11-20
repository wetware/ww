#!/usr/bin/env python3
"""Record and validate public Host/FHS build inputs; no release assembly or signing."""
import argparse
import hashlib
import json
from pathlib import Path
import re

TARGETS = {
    'ww-linux-x86_64': 'x86_64-unknown-linux-gnu',
    'ww-linux-aarch64': 'aarch64-unknown-linux-gnu',
    'ww-macos-aarch64': 'aarch64-apple-darwin',
    'ww-macos-x86_64': 'x86_64-apple-darwin',
    'ww-linux-musl-x86_64': 'x86_64-unknown-linux-musl',
}


def inventory(root):
    result = {}
    for path in sorted(root.rglob('*')):
        name = path.relative_to(root).as_posix()
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise ValueError('non-regular producer entry')
        if not name.isascii() or any(c.isspace() for c in name) or '\\' in name:
            raise ValueError('unsupported producer path')
        if path.is_file() and name != 'SHA256SUMS':
            result[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


def checksum_bytes(files):
    return ''.join(f'{digest}  {name}\n' for name, digest in files.items()).encode('ascii')


def checksums(root):
    (root / 'SHA256SUMS').write_bytes(checksum_bytes(inventory(root)))


def provenance(revision, run, attempt, target):
    if not re.fullmatch('[0-9a-f]{40}', revision) or not all(re.fullmatch('[1-9][0-9]*', v) for v in (run, attempt)):
        raise ValueError('invalid producer identity')
    return {'repository': 'https://github.com/wetware/ww', 'revision': revision,
            'builder': f'github-actions:{run}:{attempt}:{target}'}


def record(root, revision, run, attempt, target):
    value = provenance(revision, run, attempt, target)
    (root / 'REVISION').write_text(revision + '\n')
    (root / 'PROVENANCE.json').write_text(json.dumps(value, sort_keys=True) + '\n')
    checksums(root)


def validate_hosts(artifacts, revision, run, attempt):
    expected_fhs = None
    for name, target in TARGETS.items():
        root = artifacts / name
        files = inventory(root)
        if (root / 'SHA256SUMS').read_bytes() != checksum_bytes(files):
            raise ValueError(f'invalid checksum inventory: {name}')
        if (root / 'REVISION').read_text() != revision + '\n' or json.loads((root / 'PROVENANCE.json').read_bytes()) != provenance(revision, run, attempt, target):
            raise ValueError(f'stale or substituted producer identity: {name}')
        if not {'ww', 'REVISION', 'PROVENANCE.json', 'fhs/boot/kernel.wasm', 'fhs/bin/status.wasm'} <= files.keys():
            raise ValueError(f'missing Host/FHS inputs: {name}')
        if any(p not in ('ww', 'REVISION', 'PROVENANCE.json') and not p.startswith('fhs/') for p in files):
            raise ValueError(f'unexpected Host input: {name}')
        if not (root / 'ww').stat().st_size:
            raise ValueError('empty Host')
        fhs = {p: digest for p, digest in files.items() if p.startswith('fhs/')}
        if expected_fhs is not None and fhs != expected_fhs:
            raise ValueError('FHS differs between producer targets')
        expected_fhs = fhs


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=('record', 'validate-hosts'))
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--revision', required=True)
    parser.add_argument('--run', required=True)
    parser.add_argument('--attempt', required=True)
    parser.add_argument('--target', choices=(*TARGETS.values(), 'release-source'))
    args = parser.parse_args()
    if args.operation == 'record':
        if args.target is None:
            parser.error('record requires --target')
        record(args.root, args.revision, args.run, args.attempt, args.target)
    else:
        validate_hosts(args.root, args.revision, args.run, args.attempt)


if __name__ == '__main__':
    main()
