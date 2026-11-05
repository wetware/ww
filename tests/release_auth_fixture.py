"""Ephemeral Ed25519 fixtures. Never use these temporary keys for publication."""
import argparse
import base64
import os
from pathlib import Path
import subprocess


def generate_key(directory):
    directory = Path(directory)
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    private = directory / 'private.pem'
    # Explicitly create with private permissions before OpenSSL writes the key.
    descriptor = os.open(private, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.close(descriptor)
    subprocess.run(['openssl', 'genpkey', '-algorithm', 'ED25519', '-out', str(private)],
                   check=True, capture_output=True)
    public = subprocess.run(['openssl', 'pkey', '-in', str(private), '-pubout',
                             '-outform', 'DER'], check=True, capture_output=True).stdout
    assert len(public) == 44 and public[:12].hex() == '302a300506032b6570032100'
    keyring = directory / 'trusted-keys'
    keyring.write_bytes(base64.b64encode(public) + b'\n')
    return private, keyring


def sign_manifest(private, manifest, signature=None):
    manifest = Path(manifest)
    signature = Path(signature) if signature else manifest.with_suffix(manifest.suffix + '.sig')
    subprocess.run(['openssl', 'pkeyutl', '-sign', '-rawin', '-inkey', str(private),
                    '-in', str(manifest), '-out', str(signature)],
                   check=True, capture_output=True)
    assert signature.stat().st_size == 64
    return signature


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('keys', type=Path)
    parser.add_argument('manifest', type=Path)
    args = parser.parse_args()
    private = args.keys / 'private.pem'
    if not private.exists():
        private, _ = generate_key(args.keys)
    sign_manifest(private, args.manifest)
