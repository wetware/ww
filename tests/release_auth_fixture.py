"""Ephemeral Ed25519 fixtures. Never use these temporary keys for publication."""
import argparse
import base64
from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import threading
from urllib.parse import parse_qs, urlsplit


# These hashes identify the four R1 executables reviewed as bootstrap verifiers.
R1_VERIFIERS = {
    'macos/aarch64': '1fd3a88757d8a1068c0ead50b4b2b535af580ad3c51375ea423d7d069e473e84',
    'macos/x86_64': '34d105775e63b9c6223414c8ee18b2f0ab1153ab51994ef37cbd6b2a13df4fb0',
    'linux/aarch64': 'b69bc0047fad1e69de8b3cf55f0c4b66cfbd6c54d7973a3e89b4db485227b927',
    'linux/x86_64': 'edb12cd48e34cf13d60eeb9ac3b3789196a1f6d5e26a8b52a269a0952e42a9d8',
}
R1_ROOT = 'bafybeihdekx4kiimojt7l3qcl52xwrwiuukst4556ongnacbo4ulj4aqpi'


def pin_fixture_verifiers(source, destination, verifiers):
    """Substitute only the verifier pins in a private test copy of install.sh."""
    content = source.read_text()
    for platform_name, binary in verifiers.items():
        original = R1_VERIFIERS[platform_name]
        assert content.count(original) == 1, platform_name
        content = content.replace(original, hashlib.sha256(binary.read_bytes()).hexdigest())
    destination.write_text(content)
    return destination


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


def manifest_root(manifest, descriptor=None):
    """Minimal producer-profile directory, with a raw signed-manifest link."""
    link = (pb_field(10, cid_bytes(manifest)) + pb_field(18, b'CHECKSUMS.txt')
            + b'\x18' + varint(len(manifest)))
    links = pb_field(18, link)
    if descriptor is not None:
        link = (pb_field(10, cid_bytes(descriptor, 0x71))
                + pb_field(18, b'release-descriptor')
                + b'\x18' + varint(len(descriptor)))
        links += pb_field(18, link)
    return links + pb_field(10, b'\x08\x01')


def package_release(tool, tree, spool, private, trusted_keys, targets, *, provenance=None,
                    command_runner=None):
    """Package exact fixture bytes; producer provenance must be supplied explicitly."""
    if provenance is None:
        provenance = {'repository': 'https://github.com/wetware/ww',
                      'revision': 'a' * 40, 'builder': 'installer-test-fixture'}
    value = int.from_bytes(bytes.fromhex('0172002408011220') + bytes([1]) * 32, 'big')
    channel = ''
    while value:
        value, digit = divmod(value, 36)
        channel = '0123456789abcdefghijklmnopqrstuvwxyz'[digit] + channel
    metadata = {'channel': 'k' + channel, 'sequence': 1,
                'version': '0.0.0-fixture', 'source': provenance,
                'hosts': {target: provenance for target in targets}}
    (tree / 'VERSION').write_text(metadata['version'] + '\n')
    (tree / 'REVISION').write_text(provenance['revision'] + '\n')
    metadata_path = tree.parent / 'metadata.json'
    metadata_path.write_text(json.dumps(metadata))
    (tree / 'CHECKSUMS.txt.sig').unlink(missing_ok=True)
    prepare = [str(tool), 'prepare', '--tree', str(tree), '--spool', str(spool),
               '--metadata', str(metadata_path)]
    if command_runner is None:
        subprocess.run(prepare, check=True, capture_output=True, timeout=90)
    else:
        command_runner('package-prepare', prepare)
    sign_manifest(private, tree / 'CHECKSUMS.txt')
    finalize = [str(tool), 'finalize', '--tree', str(tree),
                '--spool', str(spool), '--trusted-keys', str(trusted_keys)]
    if command_runner is None:
        result = subprocess.run(finalize, check=True, capture_output=True, text=True, timeout=90)
        output = result.stdout
    else:
        output = command_runner('package-finalize', finalize)
    return json.loads(output)


@contextmanager
def packaged_bootstrap(directory, binary, payload, tool, producer_provenance, *, command_runner=None,
                       current_build_receipt=None, build_profile=None):
    """Complete GNU ARM64 bootstrap using owned copies and independent test trust.

    The lane verifies the original or current-build receipt before this helper.
    This fixture preserves its sidecar verbatim; its fresh signature is test
    authority only, never evidence of an officially published release.
    """
    directory = Path(directory).resolve()
    directory.mkdir(mode=0o700)
    provenance = json.loads(Path(producer_provenance).read_text())
    assert set(provenance) == {'repository', 'revision', 'builder'}, provenance
    assert re.fullmatch(r'[0-9a-f]{40}', provenance['revision']), provenance
    if current_build_receipt is None:
        assert build_profile is None
        assert provenance['repository'] == 'https://github.com/wetware/ww', provenance
        assert re.fullmatch(r'github-actions:[1-9][0-9]*:[1-9][0-9]*:aarch64-unknown-linux-gnu',
                            provenance['builder']), provenance
    else:
        # A current-source build is not an original release artifact.
        # Verify the separate receipt before packaging any of its exact bytes.
        sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
        from prepare_current_native_release import verify_build_files
        assert build_profile in ('development', 'release')
        receipt = Path(current_build_receipt).resolve(strict=True)
        inputs = receipt.parent
        assert receipt.name == 'BUILD-RECEIPT.json'
        assert Path(binary).resolve(strict=True) == inputs / 'ww'
        assert Path(payload).resolve(strict=True) == inputs / 'fhs'
        assert Path(producer_provenance).resolve(strict=True) == inputs / 'PROVENANCE.json'
        verify_build_files(inputs, build_profile, owner=0)
    tree, spool = directory / 'release', directory / 'spool'
    owned_binary = tree / 'bin/ww/linux/aarch64/ww'
    owned_binary.parent.mkdir(parents=True)
    shutil.copyfile(binary, owned_binary)
    owned_binary.chmod(0o700)
    shutil.copytree(payload, tree / 'fhs', copy_function=shutil.copyfile)
    owned_tool = directory / 'ww-release-package'
    shutil.copyfile(tool, owned_tool)
    owned_tool.chmod(0o700)
    private, trusted_keys = generate_key(directory / 'keys')
    try:
        package = package_release(owned_tool, tree, spool, private, trusted_keys,
                                  ['aarch64-unknown-linux-gnu'], provenance=provenance,
                                  command_runner=command_runner)
    finally:
        # No later boot/admission operation needs access to signing authority.
        private.unlink(missing_ok=True)
    root = package['root']
    descriptor = (tree / 'RELEASE-DESCRIPTOR').read_text().strip()
    metadata = ReleaseMetadataServer()
    try:
        metadata.routes = {
            ('/api/v0/cat', f'/ipfs/{root}/{name}'): (tree / name).read_bytes()
            for name in ('CHECKSUMS.txt', 'CHECKSUMS.txt.sig', 'RELEASE-DESCRIPTOR')}
        metadata.routes.update({
            ('/api/v0/block/get', descriptor): (tree / 'RELEASE-DESCRIPTOR.cbor').read_bytes(),
            ('/api/v0/block/get', root): (spool / 'blocks' / root).read_bytes()})
        yield {'binary': owned_binary, 'payload': tree / 'fhs', 'tree': tree,
               'root': root, 'descriptor': descriptor, 'trusted_keys': trusted_keys,
               'provenance': provenance, 'metadata': metadata,
               'arguments': ['--payload', str(tree / 'fhs'),
                             '--manifest', str(tree / 'CHECKSUMS.txt'),
                             '--signature', str(tree / 'CHECKSUMS.txt.sig'),
                             '--trusted-release-keys', str(trusted_keys),
                             '--bootstrap-release', root]}
    finally:
        metadata.close()


class ReleaseMetadataServer:
    """Bounded loopback transport for exact native release-metadata requests."""

    def __init__(self):
        self.routes = {}
        self.requests = []
        self.errors = []
        fixture = self

        def record_error(message):
            if len(fixture.errors) < 8:
                fixture.errors.append(str(message)[:256])

        class Server(HTTPServer):
            def handle_error(self, *_):
                record_error(sys.exc_info()[1])

        class Handler(BaseHTTPRequestHandler):
            def setup(self):
                super().setup()
                self.connection.settimeout(2)

            def do_POST(self):
                parsed = urlsplit(self.path)
                try:
                    query = parse_qs(parsed.query, strict_parsing=True)
                    assert set(query) == {'arg'} and len(query['arg']) == 1
                    assert len(parsed.path) <= 64 and len(query['arg'][0]) <= 256
                    assert not parsed.fragment and not parsed.netloc
                    assert self.headers.get('Content-Length', '0') == '0'
                    assert 'Transfer-Encoding' not in self.headers
                    assert len(fixture.requests) < 128
                    key = (parsed.path, query['arg'][0])
                    fixture.requests.append(key)
                    body = fixture.routes[key]
                    if isinstance(body, Path):
                        size = body.stat().st_size
                        assert 0 < size <= 1024 * 1024 * 1024
                    else:
                        size = len(body)
                        assert size <= 2 * 1024 * 1024
                except (AssertionError, KeyError, ValueError):
                    self.send_error(404, 'unexpected fixture request')
                    return
                self.send_response(200)
                self.send_header('Content-Length', str(size))
                self.end_headers()
                if isinstance(body, Path):
                    with body.open('rb') as artifact:
                        shutil.copyfileobj(artifact, self.wfile)
                else:
                    self.wfile.write(body)

            def log_message(self, *_):
                pass

            def log_error(self, message, *args):
                record_error(message % args)

        self.server = Server(('127.0.0.1', 0), Handler)
        self.endpoint = f'http://127.0.0.1:{self.server.server_port}'
        self.thread = threading.Thread(target=self.server.serve_forever,
                                       kwargs={'poll_interval': 0.05})
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)
        assert not self.thread.is_alive(), 'metadata fixture did not stop'
        assert not self.errors, f'unexpected metadata requests: {self.errors}'


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


def canonical_seed(private):
    """Independent OpenSSL fixture conversion; never use production identities."""
    der = subprocess.run(['openssl', 'pkey', '-in', str(private), '-outform', 'DER'],
                         check=True, capture_output=True).stdout
    assert len(der) == 48 and der[:16].hex() == '302e020100300506032b657004220420'
    raw = der[16:]
    alphabet = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
    value = int.from_bytes(raw, 'big')
    encoded = ''
    while value:
        value, digit = divmod(value, 58)
        encoded = alphabet[digit] + encoded
    return ('1' * (len(raw) - len(raw.lstrip(bytes([0])))) + encoded).encode()


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
