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
            api = urllib.parse.urlparse(kubo['IPFS_API'])
            kubo_api = f'/ip4/{api.hostname}/tcp/{api.port}'
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
            assert str(home / '.ww/fhs') not in argv, argv
            assert '--kernel' not in argv, argv
            assert argv.count('--ipfs-url') == 1, f'generated service must carry --ipfs-url: {argv}'
            assert argv[argv.index('--ipfs-url') + 1] == kubo['IPFS_API'], argv
            print(f'PASS: generated {services[0].suffix} endpoint {kubo["IPFS_API"]}', flush=True)
            # Change listeners for isolation; preserve the generated dependency endpoints.
            # Keep executable, identity, namespace and dependency arguments exact.
            # The unrelated CWD and private state must never become image layers.
            cwd = root / 'unrelated-cwd'
            cwd.mkdir()
            (cwd / 'cwd-secret').write_bytes(b'PRIVATE_CWD_SENTINEL')
            (home / '.ww/private-secret').write_bytes(b'PRIVATE_STATE_SENTINEL')
            kernel = home / '.ww/fhs/boot/kernel.wasm'
            local_kernel = root / 'override.wasm'
            # A valid custom section changes runtime bytes without changing behavior.
            tag = b'landing3-installed-override'
            local_kernel.write_bytes(kernel.read_bytes() + bytes([0, len(tag) + 1, len(tag)]) + tag)
            source_cid = subprocess.check_output(
                [ipfs, '--api', kubo_api, 'add', '-Q', '--cid-version=1', str(local_kernel)],
                env=env, text=True, timeout=30).strip()
            peer_id = None
            default_kernel_cid = None
            override_kernel_cid = None
            for mode in ('generated', 'bare', 'local-target', 'immutable-target', 'explicit-root'):

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
                if mode == 'bare':
                    at = command.index('--namespace-root')
                    del command[at:at + 2]
                elif mode == 'local-target':
                    command += [f'{local_kernel}:/boot/kernel.wasm']
                elif mode == 'immutable-target':
                    command += [f'/ipfs/{source_cid}:/boot/kernel.wasm']
                elif mode == 'explicit-root':
                    # Only the explicitly named directory becomes a root layer.
                    command += [str(fhs)]
                log = root / f'host-{mode}.log'
                with log.open('w') as output:
                    host = subprocess.Popen(command, cwd=cwd, env=host_env,
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
                    version = json.loads(wait_http(f'http://127.0.0.1:{admin}/version', host, log))
                    effective = version['kernel_effective_root']
                    assert effective and version['kernel_source_cid'] and version['kernel_epoch_seq'] is not None, version
                    assert version['kernel_source'] == f'/ipfs/{effective}/boot/kernel.wasm', version
                    selected = subprocess.check_output(
                        [ipfs, '--api', kubo_api, 'cat', f'/ipfs/{effective}/boot/kernel.wasm'],
                        env=env, timeout=30)
                    expected = local_kernel.read_bytes() if mode in ('local-target', 'immutable-target') else kernel.read_bytes()
                    assert selected == expected
                    if mode in ('local-target', 'immutable-target'):
                        if override_kernel_cid is None:
                            override_kernel_cid = version['kernel_cid']
                        assert version['kernel_cid'] == override_kernel_cid
                        assert version['kernel_cid'] != default_kernel_cid
                    else:
                        if default_kernel_cid is None:
                            default_kernel_cid = version['kernel_cid']
                        assert version['kernel_cid'] == default_kernel_cid
                    for forbidden in ('cwd-secret', 'private-secret', 'identity', 'installation.json'):
                        lookup = subprocess.run(
                            [ipfs, '--api', kubo_api, 'resolve', f'/ipfs/{effective}/{forbidden}'],
                            env=env, capture_output=True, timeout=30)
                        assert lookup.returncode != 0, (forbidden, effective)
                    print(f'PASS: installed {mode}, /readyz, /status, root={effective}, kernel={version["kernel_cid"]}, peer={peer_id}')
                finally:
                    stop(host)


if __name__ == '__main__':
    main()
