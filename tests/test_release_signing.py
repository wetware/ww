#!/usr/bin/env python3
"""Disposable ww keygen -> exact Ed25519 -> installer -> authenticated CAR proof."""
import argparse
import base64
import hashlib
import json
import os
import platform
from pathlib import Path
import shutil
import subprocess
import tempfile

REPO = Path(__file__).resolve().parents[1]
ALPHABET = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'


def run(*args, success=True):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, timeout=180)
    if success:
        assert result.returncode == 0, result.stderr.decode(errors='replace')
    else:
        assert result.returncode == 1, (result.returncode, result.stderr)
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ww', type=Path, required=True)
    parser.add_argument('--package-tool', type=Path, required=True)
    parser.add_argument('--fhs', type=Path, required=True)
    args = parser.parse_args()
    ww, tool, fhs = (path.resolve(strict=True) for path in (args.ww, args.package_tool, args.fhs))
    with tempfile.TemporaryDirectory(prefix='ww-canonical-signing-') as temporary:
        root = Path(temporary)
        seed = root / 'signing.seed'
        assert run(ww, 'keygen', '--output', seed) == b''
        encoded = seed.read_text()
        assert encoded and encoded == encoded.strip()
        assert seed.stat().st_mode & 0o077 == 0
        run(ww, 'keygen', '--output', seed, success=False)
        assert seed.read_text() == encoded
        # Independent test-only decoding into RFC 8410 PKCS#8 for OpenSSL.
        value = 0
        for char in encoded:
            value = value * 58 + ALPHABET.index(char)
        raw = b'\0' * (len(encoded) - len(encoded.lstrip('1')))
        raw += value.to_bytes((value.bit_length() + 7) // 8, 'big')
        assert len(raw) == 32
        pkcs8 = root / 'independent.der'
        descriptor = os.open(pkcs8, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'wb') as stream:
            stream.write(bytes.fromhex('302e020100300506032b657004220420') + raw)
        openssl_der = run('openssl', 'pkey', '-inform', 'DER', '-in', pkcs8,
                          '-pubout', '-outform', 'DER')
        public = root / 'public.der'
        run(tool, 'public-key', '--private-key', seed, '--output', public)
        assert public.read_bytes() == openssl_der
        assert len(openssl_der) == 44 and openssl_der[:12].hex() == '302a300506032b6570032100'
        fingerprint = hashlib.sha256(openssl_der).hexdigest()
        assert run('openssl', 'dgst', '-sha256', public).decode().strip().endswith(fingerprint)
        run(tool, 'public-key', '--private-key', seed, '--output', public, success=False)
        trust = root / 'trusted-keys'
        trust.write_bytes(base64.b64encode(openssl_der) + b'\n')
        tree = root / 'tree'
        shutil.copytree(fhs, tree / 'fhs')
        target, location = {
            ('Darwin', 'arm64'): ('aarch64-apple-darwin', 'macos/aarch64'),
            ('Darwin', 'x86_64'): ('x86_64-apple-darwin', 'macos/x86_64'),
            ('Linux', 'x86_64'): ('x86_64-unknown-linux-gnu', 'linux/x86_64'),
            ('Linux', 'aarch64'): ('aarch64-unknown-linux-gnu', 'linux/aarch64'),
        }[(platform.system(), platform.machine())]
        host = tree / 'bin/ww' / location / 'ww' 
        host.parent.mkdir(parents=True)
        shutil.copyfile(ww, host)
        (tree / 'VERSION').write_text('0.0.0-signing-test\n')
        (tree / 'REVISION').write_text('a' * 40 + '\n')
        value = int.from_bytes(bytes.fromhex('0172002408011220') + bytes([1]) * 32, 'big')
        channel = ''
        while value:
            value, digit = divmod(value, 36)
            channel = '0123456789abcdefghijklmnopqrstuvwxyz'[digit] + channel
        provenance = {'repository': 'https://github.com/wetware/ww',
                      'revision': 'a' * 40, 'builder': 'disposable-signing-qualification'}
        metadata = root / 'metadata.json'
        metadata.write_text(json.dumps({'channel': 'k' + channel, 'sequence': 1,
            'version': '0.0.0-signing-test', 'source': provenance,
            'hosts': {target: provenance}}))
        spool = root / 'spool'
        run(tool, 'prepare', '--tree', tree, '--spool', spool, '--metadata', metadata)
        manifest, signature = tree / 'CHECKSUMS.txt', tree / 'CHECKSUMS.txt.sig'
        command = [tool, 'sign', '--manifest', manifest, '--signature', signature,
                   '--private-key', seed, '--trusted-keys', trust]
        run(*command)
        expected = root / 'openssl.sig'
        run('openssl', 'pkeyutl', '-sign', '-rawin', '-keyform', 'DER', '-inkey', pkcs8,
            '-in', manifest, '-out', expected)
        assert len(signature.read_bytes()) == 64
        assert signature.read_bytes() == expected.read_bytes()
        run('openssl', 'pkeyutl', '-verify', '-rawin', '-pubin', '-keyform', 'DER',
            '-inkey', public, '-in', manifest, '-sigfile', signature)
        run('sh', REPO / 'scripts/install.sh', '--verify-manifest', manifest,
            '--signature', signature, '--trusted-release-keys', trust)
        run(*command, success=False)
        assert signature.read_bytes() == expected.read_bytes()
        finalized = json.loads(run(tool, 'finalize', '--tree', tree, '--spool', spool,
                                  '--trusted-keys', trust))
        car = root / 'release.car'
        run(tool, 'export-car', '--spool', spool, '--car', car)
        received = root / 'received'
        run(tool, 'import-car', '--car', car, '--spool', received)
        verified = json.loads(run(tool, 'verify', '--spool', received, '--trusted-keys', trust))
        assert verified == finalized
        # The native CLI itself must fail closed, independent of the shell wrapper.
        signature.unlink()
        original = manifest.read_bytes()
        manifest.write_bytes(original + b'bad\n')
        run(*command, success=False)
        assert not signature.exists()
        manifest.write_bytes(original)
        wrong_seed = root / 'wrong.seed'
        run(ww, 'keygen', '--output', wrong_seed)
        run(tool, 'sign', '--manifest', manifest, '--signature', signature,
            '--private-key', wrong_seed, '--trusted-keys', trust, success=False)
        assert not signature.exists()
        print(json.dumps({'complete': True, 'keygen_no_overwrite': True,
            'independent_spki_fingerprint': 'sha256:' + fingerprint,
            'openssl_signature_bytes_match': True, 'installer_verified': True,
            'authenticated_car_roundtrip': True, 'native_fail_closed': True,
            'production_activation': False}))


if __name__ == '__main__':
    main()
