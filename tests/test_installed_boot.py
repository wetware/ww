#!/usr/bin/env python3
"""Boot the installed Host using generated service argv and an offline Kubo."""
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request
from kubo_fixture import isolated_kubo


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def stop(child):
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=20)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()


def wait_http(url, process, log, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f'process exited: {log.read_text()}')
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                if response.status == 200:
                    return response.read()
        except (OSError, TimeoutError):
            pass
        time.sleep(0.2)
    raise RuntimeError(f'no readiness at {url}: {log.read_text()}')


def main():
    binary, fhs = (Path(arg).resolve(strict=True) for arg in sys.argv[1:])
    ipfs = shutil.which('ipfs')
    if not ipfs:
        raise RuntimeError('required Kubo executable ipfs is unavailable')
    with tempfile.TemporaryDirectory(prefix='ww-installed-boot-') as root:
        root = Path(root).resolve()
        home = root / 'home'
        home.mkdir()
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(('WW_', 'IPFS_'))}
        env.update(HOME=str(home), IPFS_PATH=str(root / 'ipfs'), NO_COLOR='1')
        with isolated_kubo() as kubo:
            # Install against a nondefault endpoint; boot must get it from service argv.
            assert urllib.parse.urlparse(kubo['IPFS_API']).port != 5001, kubo
            env.update(kubo)
            subprocess.run([str(binary), 'perform', 'install', '--payload', str(fhs)],
                           env=env, check=True, timeout=120)
            installed = home / '.ww/bin/ww'
            assert hashlib.sha256(installed.read_bytes()).digest() == hashlib.sha256(binary.read_bytes()).digest()
            identity = (home / '.ww/identity').read_bytes()
            services = list(home.rglob('*.plist')) + list(home.rglob('*.service'))
            assert len(services) == 1, services
            if services[0].suffix == '.plist':
                argv = plistlib.loads(services[0].read_bytes())['ProgramArguments']
            else:
                command = next(line[len('ExecStart='):] for line in services[0].read_text().splitlines()
                               if line.startswith('ExecStart='))
                argv = shlex.split(command)
            assert argv[0] == str(installed), argv
            assert str(home / '.ww/fhs') in argv, argv
            assert argv.count('--ipfs-url') == 1, f'generated service must carry --ipfs-url: {argv}'
            assert argv[argv.index('--ipfs-url') + 1] == kubo['IPFS_API'], argv
            print(f'PASS: generated {services[0].suffix} endpoint {kubo["IPFS_API"]}', flush=True)
            # Change listeners for isolation; preserve the generated dependency endpoints.
            # Keep executable, identity, namespace and positional FHS arguments exact.
            peer_id = None
            for mode in ('embedded', 'cli', 'environment'):
                admin, http = port(), port()
                command = list(argv)
                for flag, value in [('--listen', '/ip4/127.0.0.1/tcp/0'),
                                    ('--http-listen', f'127.0.0.1:{http}')]:
                    while command.count(flag) > 1:
                        at = command.index(flag)
                        del command[at:at + 2]
                    at = command.index(flag)
                    command[at + 1] = value
                command += ['--with-http-admin', f'127.0.0.1:{admin}', '--executor-threads', '1']
                host_env = dict(env, WW_KUBO_WAIT_MAX_SECS='15', WW_CWASM_DIR=str(root / 'cwasm'))
                host_env.pop('IPFS_API', None)
                host_env.pop('IPFS_ROUTING_API', None)
                kernel = str(home / '.ww/fhs/boot/kernel.wasm')
                if mode == 'cli':
                    command += ['--kernel', kernel]
                if mode == 'environment':
                    host_env['WW_KERNEL'] = kernel
                log = root / f'host-{mode}.log'
                with log.open('w') as output:
                    host = subprocess.Popen(command, cwd=root, env=host_env,
                                            stdout=output, stderr=subprocess.STDOUT)
                try:
                    ready = json.loads(wait_http(f'http://127.0.0.1:{admin}/readyz', host, log))
                    assert ready['ready'] is True and ready['phase'] == 'ready', ready
                    status = json.loads(wait_http(f'http://127.0.0.1:{http}/status', host, log))
                    assert status['status'] == 'ok', status
                    assert isinstance(status['version'], str) and status['version'], status
                    assert isinstance(status['peer_id'], str) and status['peer_id'], status
                    assert isinstance(status['listen_addrs'], list), status
                    assert any(addr.startswith('/ip4/127.0.0.1/tcp/')
                               for addr in status['listen_addrs']), status
                    assert isinstance(status['peer_count'], int) and status['peer_count'] >= 0, status
                    if peer_id is None:
                        peer_id = status['peer_id']
                    assert status['peer_id'] == peer_id, status
                    assert (home / '.ww/identity').read_bytes() == identity
                    print(f'PASS: installed generated argv, {mode} kernel, /readyz and /status')
                finally:
                    stop(host)


if __name__ == '__main__':
    main()
