"""Ephemeral Ed25519 fixtures. Never use these temporary keys for publication."""
import argparse
import base64
import hashlib
import os
from pathlib import Path
import platform
import subprocess


def varint(value):
    encoded = bytearray()
    while value >= 128:
        encoded.append((value & 127) | 128)
        value >>= 7
    return bytes(encoded) + bytes([value])


def cid_bytes(data, codec=0x55):
    return bytes([1, codec, 0x12, 32]) + hashlib.sha256(data).digest()


def cid_text(data, codec=0x70):
    return 'b' + base64.b32encode(cid_bytes(data, codec)).decode().lower().rstrip('=')


def pb_field(tag, data):
    return bytes([tag]) + varint(len(data)) + data


def manifest_root(manifest):
    """Minimal producer-profile directory, with a raw signed-manifest link."""
    link = (pb_field(10, cid_bytes(manifest)) + pb_field(18, b'CHECKSUMS.txt')
            + b'\x18' + varint(len(manifest)))
    return pb_field(18, link) + pb_field(10, b'\x08\x01')


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


def bootstrap_arguments(directory, binary, payload):
    """Sign exact local test bytes for the native install bootstrap boundary."""
    directory = Path(directory)
    private = directory / 'keys/private.pem'
    keys = directory / 'keys/trusted-keys'
    if not private.exists():
        private, keys = generate_key(directory / 'keys')
    target_os = 'macos' if platform.system() == 'Darwin' else 'linux'
    arch = 'aarch64' if platform.machine() in ('arm64', 'aarch64') else 'x86_64'
    artifacts = [
        (f'bin/ww/{target_os}/{arch}/ww', Path(binary)),
        ('fhs/boot/kernel.wasm', Path(payload) / 'boot/kernel.wasm'),
        ('fhs/bin/status.wasm', Path(payload) / 'bin/status.wasm'),
    ]
    if target_os == 'linux' and arch == 'x86_64':
        # CI also tests the static musl Host on a GNU runner. The fixture signer
        # authorizes these exact local bytes at both supported x86_64 paths.
        artifacts.insert(1, ('bin/ww/linux/musl-x86_64/ww', Path(binary)))
    manifest = directory / 'CHECKSUMS.txt'
    manifest.write_text('# sha256\n' + ''.join(
        f'{hashlib.sha256(path.read_bytes()).hexdigest()}  {name}\n'
        for name, path in artifacts))
    signature = sign_manifest(private, manifest)
    return ['--payload', str(payload), '--manifest', str(manifest),
            '--signature', str(signature), '--trusted-release-keys', str(keys)]


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('keys', type=Path)
    parser.add_argument('manifest', type=Path)
    args = parser.parse_args()
    private = args.keys / 'private.pem'
    if not private.exists():
        private, _ = generate_key(args.keys)
    sign_manifest(private, args.manifest)
