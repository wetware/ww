"""Disposable local Kubo for executable installer acceptance tests."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time


@contextmanager
def isolated_kubo():
    executable = shutil.which('ipfs')
    if not executable:
        raise RuntimeError('required Kubo executable ipfs is unavailable')
    with tempfile.TemporaryDirectory(prefix='ww-package-kubo-') as directory:
        path = Path(directory)
        env = dict(os.environ, IPFS_PATH=directory)
        subprocess.run([executable, 'init', '--profile=test'], env=env,
                       check=True, stdout=subprocess.DEVNULL, timeout=30)
        config = json.loads((path / 'config').read_text())
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            gateway = listener.getsockname()[1]
        config['Addresses']['API'] = '/ip4/127.0.0.1/tcp/0'
        config['Addresses']['Gateway'] = f'/ip4/127.0.0.1/tcp/{gateway}'
        config['Gateway']['ExposeRoutingAPI'] = True
        config['Bootstrap'] = []
        config['Discovery']['MDNS']['Enabled'] = False
        (path / 'config').write_text(json.dumps(config))
        with (path / 'log').open('w') as log:
            daemon = subprocess.Popen([executable, 'daemon', '--offline'], env=env,
                                      stdout=log, stderr=subprocess.STDOUT)
        try:
            for _ in range(150):
                if daemon.poll() is not None:
                    raise RuntimeError((path / 'log').read_text())
                if (path / 'api').exists():
                    if subprocess.run([executable, 'id'], env=env,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                      timeout=5).returncode == 0:
                        break
                time.sleep(0.1)
            else:
                raise RuntimeError('Kubo startup failed: ' + (path / 'log').read_text())
            api = (path / 'api').read_text().strip().split('/')[-1]
            yield {'IPFS_API': f'http://127.0.0.1:{api}',
                   'IPFS_ROUTING_API': f'http://127.0.0.1:{gateway}'}
        finally:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
