#!/usr/bin/env python3
"""Boot the installed Host explicitly with an offline Kubo; register no service."""
import argparse
from contextlib import nullcontext
import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request
from kubo_fixture import isolated_kubo
from release_auth_fixture import bootstrap_arguments, packaged_bootstrap


MAX_COMMAND_OUTPUT = 2 * 1024 * 1024
HOST_OUTER_TIMEOUT = 450  # Existing three HTTP waits and five IPFS checks, plus cleanup.


class BoundedProcess:
    """Continuously drain an owned process group with byte and wall-clock bounds."""

    def __init__(self, command, cwd, env, stdout, stderr, timeout, limit):
        self.command, self.timeout, self.limit = command, timeout, limit
        self.error = None
        self.bytes = {'stdout': 0, 'stderr': 0}
        self.stopping = threading.Event()
        self.process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                        stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE if stderr else subprocess.STDOUT,
                                        start_new_session=True)
        self.thread = threading.Thread(target=self._capture, args=(stdout, stderr))
        self.thread.start()

    @property
    def returncode(self):
        return self.process.returncode

    def _signal_group(self, sig):
        try:
            os.killpg(self.process.pid, sig)
        except ProcessLookupError:
            pass
        except PermissionError:
            # Darwin reports EPERM for a group containing only an unreaped
            # zombie. Linux qualification must still fail on permission errors.
            if sys.platform != 'darwin' or not os.waitid(
                    os.P_PID, self.process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT):
                raise

    def _capture(self, stdout, stderr):
        outputs = {}
        deadline = time.monotonic() + self.timeout
        stop_deadline = drain_deadline = None
        leader_exited = False
        try:
            with selectors.DefaultSelector() as selector:
                for label, path in (('stdout', stdout), ('stderr', stderr)):
                    if path is None:
                        continue
                    outputs[label] = Path(path).open('wb')
                    stream = getattr(self.process, label)
                    os.set_blocking(stream.fileno(), False)
                    selector.register(stream, selectors.EVENT_READ, label)
                while True:
                    now = time.monotonic()
                    if self.stopping.is_set() and stop_deadline is None:
                        self._signal_group(signal.SIGTERM)
                        stop_deadline = now + 20
                    if stop_deadline is None and now >= deadline:
                        raise subprocess.TimeoutExpired(self.command, self.timeout)
                    if stop_deadline is not None and now >= stop_deadline:
                        self._signal_group(signal.SIGKILL)
                        stop_deadline = now + 5
                        if drain_deadline is None:
                            drain_deadline = now + 2
                    # Observe without reaping: the leader's PID pins the owned
                    # process group until descendants have been signalled.
                    if not leader_exited and os.waitid(
                            os.P_PID, self.process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT):
                        leader_exited = True
                        self._signal_group(signal.SIGKILL)
                        drain_deadline = now + 2
                    if leader_exited and not selector.get_map():
                        break
                    if drain_deadline is not None and now >= drain_deadline:
                        raise RuntimeError('owned process group did not close its capture pipes')
                    for key, _ in selector.select(0.05):
                        data = os.read(key.fileobj.fileno(), 65536)
                        if not data:
                            selector.unregister(key.fileobj)
                            continue
                        remaining = self.limit - self.bytes[key.data]
                        kept = data[:remaining]
                        outputs[key.data].write(kept)
                        outputs[key.data].flush()
                        self.bytes[key.data] += len(kept)
                        if len(data) > remaining:
                            raise RuntimeError(f'{key.data} output exceeds {self.limit}-byte bound')
        except BaseException as error:
            self.error = error
        finally:
            try:
                # Signal before wait(), even when the leader already exited.
                self._signal_group(signal.SIGKILL)
            except BaseException as error:
                self.error = self.error or error
            try:
                self.process.wait(timeout=5)
            except BaseException as error:
                self.error = self.error or error
            finally:
                for output in outputs.values():
                    output.close()
                self.process.stdout.close()
                if self.process.stderr is not None:
                    self.process.stderr.close()

    def check(self):
        if self.error is not None:
            raise self.error

    def poll(self):
        self.check()
        return None if self.thread.is_alive() else self.returncode

    def wait(self):
        self.thread.join()
        self.check()
        return self.returncode

    def stop(self):
        self.stopping.set()
        self.thread.join(timeout=28)
        if self.thread.is_alive():
            raise RuntimeError('owned process supervision did not stop within cleanup bound')
        self.check()


def positive_seconds(value):
    seconds = int(value)
    if seconds <= 0:
        raise argparse.ArgumentTypeError('admission timeout must be positive')
    return seconds


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('fhs', type=Path)
    parser.add_argument('--guarded', action='store_true')
    parser.add_argument('--package-tool', type=Path)
    parser.add_argument('--producer-provenance', type=Path)
    parser.add_argument('--admission-timeout-secs', type=positive_seconds)
    parser.add_argument('--evidence', type=Path)
    args = parser.parse_args(argv)
    options = ('package_tool', 'producer_provenance', 'admission_timeout_secs', 'evidence')
    if args.guarded and any(getattr(args, name) is None for name in options):
        parser.error('--guarded requires --package-tool, --producer-provenance, '
                     '--admission-timeout-secs and --evidence')
    if not args.guarded and any(getattr(args, name) is not None for name in options):
        parser.error('guarded boot options require --guarded')
    return args


def guarded_environment(env):
    result = {key: value for key, value in env.items()
              if not key.startswith(('LD_', 'DYLD_', 'TEST_', 'WW_TEST_'))
              and key.lower() not in ('http_proxy', 'https_proxy', 'all_proxy', 'no_proxy')}
    result['WW_COMPILE_WORKERS'] = '2'
    return result


class GuardedEvidence:
    """Retain command and readiness receipts outside disposable boot state."""

    def __init__(self, path):
        path = Path(path).absolute()
        assert not path.is_symlink(), 'evidence directory must not be a symlink'
        parent = path.parent.resolve(strict=True)
        assert parent.stat().st_uid == os.geteuid(), 'evidence parent must belong to fixture user'
        self.path = parent / path.name
        if self.path.exists():
            info = self.path.stat()
            assert stat.S_ISDIR(info.st_mode) and info.st_uid == os.geteuid()
            assert not info.st_mode & 0o077, 'evidence directory must be private'
            assert not any(self.path.iterdir()), 'evidence directory must be empty'
        else:
            self.path.mkdir(mode=0o700)
        self.report = {'version': 1, 'status': 'running', 'commands': [], 'modes': {}}
        self.save()

    def save(self):
        (self.path / 'report.json').write_text(json.dumps(self.report, indent=2) + '\n')

    def __enter__(self):
        return self

    def __exit__(self, kind, error, traceback):
        self.report['status'] = 'failed' if error is not None else 'completed'
        if error is not None:
            self.report['error'] = str(error)
            for mode in self.report['modes'].values():
                if mode.get('status') != 'completed':
                    mode['status'] = 'failed'
        self.save()

    @staticmethod
    def context(command, cwd, env):
        # Bind the complete environment without publishing inherited credentials.
        selected = {key: value for key, value in env.items()
                    if key.startswith(('WW_', 'IPFS_'))
                    or key in ('HOME', 'PATH', 'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS', 'NO_COLOR')}
        return {'argv': list(command), 'cwd': str(cwd), 'stdin': 'DEVNULL',
                'environment': selected,
                'environment_sha256': hashlib.sha256(
                    json.dumps(env, sort_keys=True).encode()).hexdigest()}

    def run(self, name, command, cwd, env, timeout, *, group='commands'):
        record = dict(self.context(command, cwd, env), name=name, timeout_secs=timeout,
                      stdout=f'{name}.stdout', stderr=f'{name}.stderr', status='running',
                      output_limit_bytes_per_stream=MAX_COMMAND_OUTPUT)
        self.report.setdefault(group, []).append(record)
        self.save()
        started = time.monotonic()
        process = None
        try:
            process = BoundedProcess(command, cwd, env, self.path / record['stdout'],
                                     self.path / record['stderr'], timeout, MAX_COMMAND_OUTPUT)
            code = process.wait()
            if code != 0:
                raise subprocess.CalledProcessError(code, command)
            record.update(status='completed', returncode=code)
        except BaseException as error:
            record.update(status='failed', error=str(error),
                          returncode=getattr(error, 'returncode', None))
            raise
        finally:
            if process is not None:
                record.update(returncode=process.returncode, output_bytes=process.bytes)
            record['elapsed_secs'] = time.monotonic() - started
            self.save()
        return (self.path / record['stdout']).read_text()


def admit_guarded(command, cwd, env, timeout, evidence, mode):
    assert command[1] == 'run'
    evidence.report['modes'][mode] = {'status': 'running',
                                     'launch': evidence.context(command, cwd, env)}
    admission = [command[0], 'startup', 'admit', '--timeout-secs', str(timeout),
                 '--json', 'run', *command[2:]]
    # Allow the explicit operation budget plus bounded native containment/cleanup.
    result = json.loads(evidence.run(f'admit-{mode}', admission, cwd, env, timeout + 40))
    evidence.report['modes'][mode]['admission'] = result
    evidence.save()
    assert result['status'] == 'completed' and result['activation'] is False, result
    assert result['result']['kind'] == 'admitted', result


def verify_guarded_install(home, fixture):
    state_dir = home / '.ww/bin/.ww-host-upgrade'
    state = json.loads((state_dir / 'startup-state.json').read_text())
    assert state['disposition'] == {'kind': 'required-unbound'}, state
    assert state['trusted_release_keys'] == str(fixture['trusted_keys'].resolve()), state
    native = state['native']
    assert native['transport_root'] == fixture['root'], state
    assert native['descriptor'] == fixture['descriptor'], state
    original = fixture['binary'].read_bytes()
    assert native['artifact'] == {'algorithm': 'sha256',
                                  'digest': hashlib.sha256(original).hexdigest(),
                                  'size': len(original)}, state
    proof = state_dir / 'proofs' / fixture['root']
    assert hashlib.sha256(proof.read_bytes()).hexdigest() == native['proof']['sha256'], state
    assert proof.stat().st_size == native['proof']['size'], state
    installed = home / '.ww/bin/ww'
    assert not installed.is_symlink() and installed.read_bytes() == original
    for name in ('boot/kernel.wasm', 'bin/status.wasm'):
        assert (home / '.ww/fhs' / name).read_bytes() == (fixture['payload'] / name).read_bytes()
    locator = json.loads((home / '.ww/installed-release.json').read_text())
    assert locator['version'] == 1 and locator['release'] == fixture['root'], locator
    return state


def install_guarded(args, root, home, env, evidence):
    def package_command(name, command):
        return evidence.run(name, command, root, env, 90, group='package_commands')

    with packaged_bootstrap(root / 'bootstrap', args.binary.resolve(strict=True),
                            args.fhs.resolve(strict=True), args.package_tool.resolve(strict=True),
                            args.producer_provenance.resolve(strict=True),
                            command_runner=package_command) as fixture:
        evidence.report['bootstrap'] = {
            'root': fixture['root'], 'descriptor': fixture['descriptor'],
            'producer_provenance': fixture['provenance'],
            'authority': 'ephemeral independent test key',
            'binary_sha256': hashlib.sha256(fixture['binary'].read_bytes()).hexdigest(),
            'package_tool_sha256': hashlib.sha256(args.package_tool.read_bytes()).hexdigest()}
        for name in ('CHECKSUMS.txt', 'CHECKSUMS.txt.sig', 'RELEASE-DESCRIPTOR',
                     'RELEASE-DESCRIPTOR.cbor'):
            shutil.copyfile(fixture['tree'] / name, evidence.path / name)
        shutil.copyfile(fixture['trusted_keys'], evidence.path / 'trusted-keys')
        shutil.copyfile(args.producer_provenance, evidence.path / 'PROVENANCE.json')
        evidence.save()
        install_env = dict(env, IPFS_API=fixture['metadata'].endpoint)
        try:
            evidence.run('install', [str(fixture['binary']), 'install', *fixture['arguments']],
                         root, install_env, 120)
            evidence.report['installation'] = verify_guarded_install(home, fixture)
            expected = [('/api/v0/cat', f'/ipfs/{fixture["root"]}/{name}')
                        for name in ('CHECKSUMS.txt', 'CHECKSUMS.txt.sig', 'RELEASE-DESCRIPTOR')]
            expected += [('/api/v0/block/get', fixture['descriptor']),
                         ('/api/v0/block/get', fixture['root'])]
            assert fixture['metadata'].requests == expected, fixture['metadata'].requests
        finally:
            evidence.report['metadata_requests'] = fixture['metadata'].requests
            evidence.save()
        return fixture['binary'], fixture['payload']


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


def main(argv=None):
    args = parse_args(argv)
    binary, fhs = args.binary.resolve(strict=True), args.fhs.resolve(strict=True)
    ipfs = shutil.which('ipfs')
    if not ipfs:
        raise RuntimeError('required Kubo executable ipfs is unavailable')
    evidence = GuardedEvidence(args.evidence) if args.guarded else None
    # Keep the admitted GNU ARM64 readiness address within the existing limit.
    prefix = '' if args.guarded else 'ww-installed-boot-'
    with (evidence or nullcontext()), tempfile.TemporaryDirectory(prefix=prefix) as root:
        root = Path(root).resolve()
        home = root / ('h' if args.guarded else 'home')
        if args.guarded:
            longest_socket = home / '.ww/bin/.ww-host-upgrade' / ('r-4294967295-' + '0' * 32 + '.sock')
            assert len(os.fsencode(longest_socket)) < 104, 'guarded fixture HOME exceeds native socket path budget'
        home.mkdir()
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(('WW_', 'IPFS_'))}
        env.update(HOME=str(home), IPFS_PATH=str(root / 'ipfs'), NO_COLOR='1')
        if args.guarded:
            env = guarded_environment(env)
        with isolated_kubo() as kubo:
            # Explicit runtime endpoints remain separate from offline installation.
            assert urllib.parse.urlparse(kubo['IPFS_API']).port != 5001, kubo
            api = urllib.parse.urlparse(kubo['IPFS_API'])
            kubo_api = f'/ip4/{api.hostname}/tcp/{api.port}'
            env.update(kubo)
            if args.guarded:
                binary, fhs = install_guarded(args, root, home, env, evidence)
            else:
                bootstrap = bootstrap_arguments(root / 'bootstrap', binary, fhs)
                subprocess.run([str(binary), 'install', *bootstrap],
                               env=env, check=True, timeout=120)
            installed = home / '.ww/bin/ww'
            assert hashlib.sha256(installed.read_bytes()).digest() == hashlib.sha256(binary.read_bytes()).digest()
            identity = (home / '.ww/identity').read_bytes()
            services = list(home.rglob('*.plist')) + list(home.rglob('*.service'))
            assert services == [], services
            assert not (home / '.ww/etc/ns/ww').exists()
            argv = [str(installed), 'run', '--identity', str(home / '.ww/identity'),
                    '--namespace-root', str(home / '.ww'),
                    '--ipfs-url', kubo['IPFS_API'],
                    '--ipns-routing-url', kubo['IPFS_ROUTING_API'],
                    '--listen', '/ip4/127.0.0.1/tcp/0', '--http-listen', '127.0.0.1:0']
            # Start explicitly with isolated listeners and dependency endpoints.
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
            for mode in ('configured', 'bare', 'local-target', 'immutable-target', 'explicit-root'):

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
                if args.guarded:
                    admit_guarded(command, cwd, host_env, args.admission_timeout_secs, evidence, mode)
                log = (evidence.path if args.guarded else root) / f'host-{mode}.log'
                if args.guarded:
                    host = BoundedProcess(command, cwd, host_env, log, None,
                                          HOST_OUTER_TIMEOUT, MAX_COMMAND_OUTPUT)
                else:
                    with log.open('w') as output:
                        host = subprocess.Popen(command, cwd=cwd, env=host_env,
                                                stdout=output, stderr=subprocess.STDOUT)
                try:
                    ready = json.loads(wait_http(f'http://127.0.0.1:{admin}/readyz', host, log))
                    if args.guarded:
                        evidence.report['modes'][mode]['ready'] = ready
                        evidence.save()
                    assert ready['ready'] is True and ready['phase'] == 'ready', ready
                    status = json.loads(wait_http(f'http://127.0.0.1:{http}/status', host, log))
                    if args.guarded:
                        evidence.report['modes'][mode]['http_status'] = status
                        evidence.save()
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
                    if args.guarded:
                        evidence.report['modes'][mode]['version'] = version
                        evidence.save()
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
                    if args.guarded:
                        host.check()
                        services = list(home.rglob('*.plist')) + list(home.rglob('*.service'))
                        assert services == [], services
                        evidence.report['modes'][mode].update(
                            status='completed',
                            identity_sha256=hashlib.sha256(identity).hexdigest(),
                            kernel_sha256=hashlib.sha256(selected).hexdigest(),
                            private_paths_absent=True, service_files=[])
                        evidence.save()
                    print(f'PASS: installed {mode}, /readyz, /status, root={effective}, kernel={version["kernel_cid"]}, peer={peer_id}')
                finally:
                    if args.guarded:
                        try:
                            host.stop()
                        finally:
                            item = evidence.report['modes'][mode]
                            item.update(host_returncode=host.returncode, host_output_bytes=host.bytes,
                                        host_output_limit_bytes=MAX_COMMAND_OUTPUT)
                            if host.error is not None:
                                item.update(status='failed', host_error=str(host.error))
                            evidence.save()
                    else:
                        stop(host)


if __name__ == '__main__':
    main()
