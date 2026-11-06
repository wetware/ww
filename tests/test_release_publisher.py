#!/usr/bin/env python3
"""Isolated Kubo 0.43.1 + installed Rust publisher qualification.

No bootstrap peers, swarm listener, production identity, or production socket.
--fhs exercises preparation with real validated components. --fixture accepts a
test-only signed five-platform package and discovery key for fast local runs.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import time
from urllib.parse import parse_qs, urlsplit


def run(command, *, env=None, stdin=None):
    result = subprocess.run(command, input=stdin, env=env, capture_output=True,
                            timeout=120, check=False)
    if result.returncode:
        raise AssertionError(f"command failed ({result.returncode}): {command[0]}\n"
                             + result.stderr.decode(errors='replace')[-2000:])
    return result.stdout


class LostAckProxy:
    """Relay Unix HTTP, checking retention and dropping one completed name PUT."""

    def __init__(self, path, upstream, journal, kubo):
        self.path, self.upstream = str(path), str(upstream)
        self.journal, self.kubo = journal, kubo
        self.drop_once, self.announcements, self.errors = True, 0, []
        self.stopping = threading.Event()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(self.path)
        self.listener.listen(8)
        self.listener.settimeout(0.1)
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        while not self.stopping.is_set():
            try:
                client, _ = self.listener.accept()
            except TimeoutError:
                continue
            except OSError:
                return
            try:
                with client, socket.socket(socket.AF_UNIX) as upstream:
                    client.settimeout(30)
                    upstream.settimeout(30)
                    upstream.connect(self.upstream)
                    request = bytearray()
                    while not request.endswith(b'\r\n\r\n'):
                        part = client.recv(1)
                        if not part:
                            raise AssertionError('incomplete proxy request')
                        request.extend(part)
                        assert len(request) < 16 * 1024
                    request = bytes(request)
                    first = request.split(b'\r\n', 1)[0].decode()
                    target = first.split(' ')[1]
                    publishing = target.startswith('/api/v0/name/publish?')
                    if publishing:
                        self.announcements += 1
                        root = parse_qs(urlsplit(target).query)['arg'][0]
                        state = json.loads((self.journal / 'state.json').read_text())
                        current = state['current']
                        encoded = root.removeprefix('/ipfs/')[1:].upper()
                        identity = base64.b32decode(encoded + '=' * ((-len(encoded)) % 8))
                        assert current['root'] == list(identity)
                        selected = next(r for r in state['releases']
                                        if r['publication'] == current)
                        assert selected['state'] == 'Published'
                        assert selected['announcement']
                        assert selected['pending_stage'] is None
                        assert current['root'] in selected['stages']
                        self.kubo('pin', 'ls', '--type=recursive', root)
                    upstream.sendall(request[:-2] + b'Connection: close\r\n\r\n')
                    headers = dict(line.split(b':', 1) for line in request.split(b'\r\n')[1:] if b':' in line)
                    remaining = int(next((v for k, v in headers.items()
                                          if k.lower() == b'content-length'), b'0'))
                    while remaining:
                        chunk = client.recv(min(65536, remaining))
                        assert chunk
                        upstream.sendall(chunk)
                        remaining -= len(chunk)
                    dropping = publishing and self.drop_once
                    response_prefix = bytearray()
                    while True:
                        chunk = upstream.recv(65536)
                        if not chunk:
                            break
                        if len(response_prefix) < 128:
                            response_prefix.extend(chunk[:128])
                        if not dropping:
                            client.sendall(chunk)
                    if dropping:
                        assert response_prefix.startswith(b'HTTP/1.1 200 ')
                        self.drop_once = False
            except Exception as error:  # preserve failures from the server thread
                self.errors.append(error)

    def close(self):
        self.stopping.set()
        self.listener.close()
        self.thread.join(timeout=5)
        assert not self.thread.is_alive()
        if self.errors:
            raise AssertionError(f'proxy failed: {self.errors}')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--ipfs', type=Path, required=True)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--fhs', type=Path)
    source.add_argument('--fixture', type=Path)
    args = parser.parse_args()
    binary, ipfs = str(args.binary.resolve()), str(args.ipfs.resolve())
    assert run([ipfs, 'version', '--number']).strip() == b'0.43.1'
    # /tmp keeps Unix socket paths below platform sockaddr_un length limits.
    with tempfile.TemporaryDirectory(prefix='ww-publisher-', dir='/tmp') as tmp:
        root = Path(tmp)
        repo = root / 'repo'
        env = dict(os.environ, IPFS_PATH=str(repo))
        kubo = lambda *args: run([ipfs, *args], env=env)
        kubo('init', '--profile=test')
        config = json.loads((repo / 'config').read_text())
        api_socket = root / 'kubo.sock'
        config['Addresses']['API'] = '/unix' + str(api_socket)
        config['Addresses']['Gateway'] = ''
        config['Addresses']['Swarm'] = []
        config['Bootstrap'] = []
        config['Discovery']['MDNS']['Enabled'] = False
        config['Routing']['Type'] = 'none'
        (repo / 'config').write_text(json.dumps(config))
        if args.fixture:
            fixture = args.fixture.resolve()
            channel = kubo('key', 'import', 'ww-release', str(fixture / 'test-discovery-key.protobuf')).decode().strip()
            spool = fixture / 'spool'
            keys = fixture / 'trusted-release-keys'
        else:
            channel = kubo('key', 'gen', '--type=ed25519', 'ww-release').decode().strip()
            tree, spool = root / 'tree', root / 'spool'
            tree.mkdir()
            shutil.copytree(args.fhs.resolve(), tree / 'fhs')
            (tree / 'VERSION').write_text('0.1.1\n')
            (tree / 'REVISION').write_text('a' * 40 + '\n')
            targets = {
                'x86_64-unknown-linux-gnu': 'linux/x86_64',
                'aarch64-unknown-linux-gnu': 'linux/aarch64',
                'aarch64-apple-darwin': 'macos/aarch64',
                'x86_64-apple-darwin': 'macos/x86_64',
                'x86_64-unknown-linux-musl': 'linux/musl-x86_64',
            }
            provenance = dict(repository='https://github.com/wetware/ww',
                              revision='a' * 40, builder='isolated-publisher-test')
            for index, path in enumerate(targets.values()):
                artifact = tree / 'bin/ww' / path / 'ww'
                artifact.parent.mkdir(parents=True)
                with artifact.open('wb') as output:
                    # 80 MiB across five artifacts forces multiple import stages.
                    for chunk in range(16):
                        output.write(bytes([index, chunk]) + b'\0' * (1024 * 1024 - 2))
            metadata = root / 'metadata.json'
            metadata.write_text(json.dumps(dict(channel=channel, sequence=1,
                version='0.1.1', source=provenance,
                hosts={target: provenance for target in targets})))
            run([binary, 'prepare', '--tree', str(tree), '--spool', str(spool), '--metadata', str(metadata)])
            private = root / 'test-key.pem'
            keys = root / 'keys'
            run(['openssl', 'genpkey', '-algorithm', 'ED25519', '-out', str(private)])
            der = run(['openssl', 'pkey', '-in', str(private), '-pubout', '-outform', 'DER'])
            keys.write_bytes(base64.b64encode(der) + b'\n')
            run(['openssl', 'pkeyutl', '-sign', '-rawin', '-inkey', str(private),
                 '-in', str(tree / 'CHECKSUMS.txt'), '-out', str(tree / 'CHECKSUMS.txt.sig')])
            run([binary, 'finalize', '--tree', str(tree), '--spool', str(spool), '--trusted-keys', str(keys)])
        checked = json.loads(run([binary, 'verify', '--spool', str(spool), '--trusted-keys', str(keys)]))
        candidate = checked['root']
        request = run([binary, 'request', '--root', candidate])
        expected_request = json.loads(request)
        assert expected_request['request_id'] == hashlib.sha256(candidate.encode()).hexdigest()[:32]
        journal = root / 'journal'
        journal.mkdir(mode=0o700)
        publisher_config = root / 'publisher.json'
        publisher_config.write_text(json.dumps(dict(version=1, domain='isolated-release-channel',
            channel=channel, key='ww-release', journal=str(journal), trusted_keys=str(keys),
            operation_timeout_seconds=30)))
        publisher_config.chmod(0o600)
        daemon = subprocess.Popen([ipfs, 'daemon'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        proxy = None
        try:
            for _ in range(150):
                if daemon.poll() is not None:
                    raise AssertionError('isolated Kubo exited')
                if api_socket.exists():
                    break
                time.sleep(0.1)
            else:
                raise AssertionError('Kubo startup timed out')
            run([binary, 'seed', '--spool', str(spool), '--trusted-keys', str(keys),
                 '--kubo-api', '/unix' + str(api_socket)])
            proxy_path = root / 'proxy.sock'
            proxy = LostAckProxy(proxy_path, api_socket, journal, kubo)
            command = [binary, 'publish', '--config', str(publisher_config),
                       '--kubo-api', '/unix' + str(proxy_path)]
            first = json.loads(run(command, stdin=request))
            assert first['local_committed'] is True, first
            assert first['delivery_acknowledged'] is False, first
            assert proxy.announcements == 1
            # A fresh process acquires the same journal and fetches the graph.
            retry = json.loads(run(command, stdin=request))
            assert retry['local_committed'] is True and retry['delivery_acknowledged'] is True, retry
            assert retry['generation'] == first['generation']
            assert proxy.announcements == 2
            state = json.loads((journal / 'state.json').read_text())
            assert len(state['releases']) == 1
            assert state['releases'][0]['publishing'] is False
            renewed = json.loads(run(command + ['--package', str(spool)], stdin=request))
            assert renewed['local_committed'] is True and renewed['delivery_acknowledged'] is True
            assert renewed['generation'] == retry['generation']
            assert proxy.announcements == 3
            assert kubo('cat', '/ipfs/' + candidate + '/CHECKSUMS.txt.sig')
            assert kubo('cat', '/ipfs/' + candidate + '/bin/ww/linux/x86_64/ww')
            # Wrong independent trust must fail without another IPNS operation.
            original_keys = keys.read_bytes()
            invalid_keys = root / 'invalid-keys'
            invalid_keys.write_bytes(b'not a release key\n')
            changed = json.loads(publisher_config.read_text())
            changed['trusted_keys'] = str(invalid_keys)
            publisher_config.write_text(json.dumps(changed))
            refused = json.loads(run(command, stdin=request))
            assert refused['local_committed'] is None and refused['delivery_acknowledged'] is False
            assert proxy.announcements == 3
            assert keys.read_bytes() == original_keys
            print(json.dumps(dict(kubo='0.43.1', local_committed=True,
                lost_ack_recovered=True, generation=retry['generation'],
                stages=checked['stages'], legacy_paths=True, invalid_trust_refused=True,
                acknowledged_current_reannounced=True,
                fhs_validated=bool(args.fhs), public_network=False)))
        finally:
            try:
                if proxy is not None:
                    proxy.close()
            finally:
                daemon.terminate()
                try:
                    daemon.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()


if __name__ == '__main__':
    main()
