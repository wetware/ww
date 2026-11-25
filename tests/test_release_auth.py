#!/usr/bin/env python3
"""Exercise real shell authentication with ephemeral keys and isolated transport."""
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from release_auth_fixture import (cid_bytes, cid_text, generate_key, manifest_root,
                                  pb_field, pin_fixture_verifiers, R1_ROOT, sign_manifest, varint)

REPO = Path(__file__).resolve().parents[1]
INSTALLER = REPO / 'scripts/install.sh'
R1_SPKI_SHA256 = 'c15cb2b5a66ab54bba27e39c79da23a3d927b52032b91df3456c8d4146f5b348'
RELEASE_NAME = 'k51qzi5uqu5dgbw6eyddak3ozm2p2zwet8skgn8qzbwzb90d5obsnnwimvp14i'
PLATFORMS = [('Linux', 'x86_64', 'linux/x86_64'),
             ('Linux', 'aarch64', 'linux/aarch64'),
             ('Darwin', 'x86_64', 'macos/x86_64'),
             ('Darwin', 'arm64', 'macos/aarch64')]


class ReleaseAuthentication(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='ww-release-auth-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.private, self.keys = generate_key(self.root / 'keys')
        self.other_private, self.other_keys = generate_key(self.root / 'other-keys')
        self.release = self.root / 'release'
        self.release.mkdir()
        self.manifest = self.release / 'CHECKSUMS.txt'
        self.signature = self.release / 'CHECKSUMS.txt.sig'
        self.home = self.root / 'home'
        self.home.mkdir()
        (self.home / 'sentinel').write_text('existing installation state')
        self.calls = self.root / 'executions'
        self.transport = self.root / 'transport-calls'
        self.probes = self.root / 'platform-probes'
        self.stubs = self.root / 'bin'
        self.stubs.mkdir()
        self.script('uname', '''printf '%s\n' "$*" >> "$TEST_PLATFORM_PROBES"
case "$1" in
-s) echo "$TEST_OS" ;;
-m) echo "$TEST_ARCH" ;;
*) exit 90 ;;
esac
''')
        self.script('ipfs', '''printf '%s\n' "$*" >> "$TEST_TRANSPORT"
case "$1" in
id) exit "${TEST_ID_STATUS:-0}" ;;
name)
  [ "$*" = "name get $TEST_RELEASE_NAME" ] || exit 91
  cat "$TEST_DISCOVERY"
  exit "${TEST_RESOLVE_STATUS:-0}" ;;
--offline)
  [ -n "$IPFS_PATH" ] || exit 91
  shift
  case "$*" in
    "name inspect --verify=$TEST_RELEASE_NAME --dump=false --enc=json") cat ;;
    "init --profile=test") mkdir "$IPFS_PATH" ;;
    "block put --cid-codec=dag-cbor")
      cmp -s - "$TEST_DESCRIPTOR_BLOCK" || exit 91
      cat "$TEST_RELEASE/RELEASE-DESCRIPTOR" ;;
    "dag get $TEST_DESCRIPTOR_CID") cat "$TEST_DESCRIPTOR_JSON" ;;
    *) exit 91 ;;
  esac ;;
block)
  [ "$2" = get ] || exit 91
  if [ "$3" = "${TEST_DESCRIPTOR_CID:-}" ]; then cat "$TEST_DESCRIPTOR_BLOCK"; exit; fi
  [ "$3" = "$TEST_CID" ] || exit 91
  [ "${FAIL_FETCH:-}" != root ] || exit 1
  cat "$TEST_ROOT_BLOCK" ;;
cat)
  case "$2" in
    "/ipfs/$TEST_VERIFIER_ROOT/"*) cat "$TEST_VERIFIERS/${2#"/ipfs/$TEST_VERIFIER_ROOT/"}"; exit ;;
  esac
  prefix=/ipfs/${TEST_CID:-test-release}/
  case "$2" in "$prefix"*) ;; *) exit 91 ;; esac
  path=${2#"$prefix"}
  if [ "${FAIL_FETCH:-}" = "$path" ]; then printf truncated; exit 1; fi
  cat "$TEST_RELEASE/$path" ;;
*) exit 92 ;;
esac
''')
        # The frozen legacy installer still invokes perform. This executable
        # fixture also records the first execution of the installed Host.
        binary = b'''#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$TEST_EXECUTIONS"
case "$*" in
'upgrade --recover-only '*) exit "${TEST_RECOVERY_EXIT:-0}" ;;
*--check*) exit 0 ;;
'install '*|'perform install '*) mkdir -p "$HOME/.ww/bin"; cp "$0" "$HOME/.ww/bin/ww" ;;
esac
'''
        verifiers = {}
        verifier_root = self.root / 'verifiers'
        for _, _, path in PLATFORMS:
            verifier = verifier_root / 'bin/ww' / path / 'ww'
            verifier.parent.mkdir(parents=True)
            verifier.write_text(f'''#!/bin/sh
set -eu
printf 'verifier %s\n' "$*" >> "$TEST_VERIFIER_CALLS"
[ "$1" = install ] || exit 91
if [ -n "${{TEST_CAPTURE_KEY:-}}" ]; then
  while [ "$1" != --trusted-release-keys ]; do shift; done
  cp -p "$2" "$TEST_CAPTURE_KEY"
fi
mkdir -p "$HOME/.ww/bin"
cp "$TEST_RELEASE/bin/ww/{path}/ww" "$HOME/.ww/bin/ww"
chmod +x "$HOME/.ww/bin/ww"
''')
            verifiers[path] = verifier
            target = self.release / 'bin/ww' / path / 'ww'
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(binary)
        for name in ('fhs/boot/kernel.wasm', 'fhs/bin/status.wasm'):
            target = self.release / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(b'fixture WASM')
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(('WW_', 'IPFS_', 'GITHUB_'))}
        if os.environ.get('WW_RELEASE_PACKAGE'):
            self.env['WW_RELEASE_PACKAGE'] = os.environ['WW_RELEASE_PACKAGE']
        self.env.update(HOME=str(self.home), PATH=str(self.stubs) + os.pathsep + os.environ['PATH'],
                        TEST_OS='Linux', TEST_ARCH='x86_64', TEST_RELEASE=str(self.release),
                        TEST_EXECUTIONS=str(self.calls), TEST_TRANSPORT=str(self.transport),
                        TEST_PLATFORM_PROBES=str(self.probes), TEST_ROOT_BLOCK=str(self.root / 'root.block'))
        self.verifier_calls = self.root / 'verifier-calls'
        self.env.update(TEST_VERIFIER_ROOT=R1_ROOT, TEST_VERIFIERS=str(verifier_root),
                        TEST_VERIFIER_CALLS=str(self.verifier_calls))
        self.installer = pin_fixture_verifiers(INSTALLER, self.root / 'fixture-install.sh', verifiers)
        self.write_manifest()

    def script(self, name, body):
        path = self.stubs / name
        path.write_text('#!/bin/sh\nset -eu\n' + body)
        path.chmod(0o755)

    def write_manifest(self):
        records = []
        for path in sorted(self.release.rglob('*')):
            if path.is_file() and path.name not in ('CHECKSUMS.txt', 'CHECKSUMS.txt.sig'):
                records.append(hashlib.sha256(path.read_bytes()).hexdigest() + '  '
                               + path.relative_to(self.release).as_posix() + '\n')
        self.manifest.write_text('# sha256\n' + ''.join(records))
        self.sign()

    def sign(self, private=None):
        sign_manifest(private or self.private, self.manifest)
        self.set_root(manifest_root(self.manifest.read_bytes()))

    def set_root(self, data):
        (self.root / 'root.block').write_bytes(data)
        self.release_cid = cid_text(data)
        self.env['TEST_CID'] = self.release_cid

    def run_installer(self, verify=False, success=False, keys=None, legacy=False):
        installer = REPO / 'tests/fixtures/release-auth/legacy-install.sh' if legacy else self.installer
        args = ['sh', str(installer)]
        if not legacy:
            args += ['--trusted-release-keys', str(keys or self.keys)]
        if verify:
            args += ['--verify-manifest', str(self.manifest), '--signature', str(self.signature)]
        else:
            args += ['--version', 'test-release'] if legacy else ['--release', self.release_cid]
        before = {str(path.relative_to(self.home)): path.read_bytes()
                  for path in self.home.rglob('*') if path.is_file()}
        previous_calls = len(self.calls.read_text().splitlines()) if self.calls.exists() else 0
        env = dict(self.env, TEST_CID='test-release') if legacy else self.env
        result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=30)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            if not verify and not legacy:
                calls = self.calls.read_text().splitlines()[previous_calls:]
                self.assertEqual(calls, ['--version'])
                verifier_call = self.verifier_calls.read_text().splitlines()[-1]
                self.assertIn(f'install --release {self.release_cid} ', verifier_call)
                self.assertIn('--trusted-release-keys ', verifier_call)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse(self.calls.exists(), 'unauthenticated candidate executed')
            after = {str(path.relative_to(self.home)): path.read_bytes()
                     for path in self.home.rglob('*') if path.is_file()}
            self.assertEqual(after, before)
        if verify:
            self.assertFalse(self.transport.exists(), 'verify-only mode contacted IPFS')
        return result

    def test_valid_release_all_platforms(self):
        for os_name, arch, _ in PLATFORMS:
            with self.subTest(os=os_name, arch=arch):
                self.env.update(TEST_OS=os_name, TEST_ARCH=arch)
                self.run_installer(success=True)
                self.assertEqual(self.calls.read_text().splitlines(), ['--version'])
                self.calls.unlink()

    def substituted_release(self, original_root):
        # A and B are different releases signed by the SAME trusted publisher.
        # Authentication of either manifest succeeds independently.
        self.run_installer(verify=True, success=True)
        requested = self.release_cid
        root_a = (self.root / 'root.block').read_bytes()
        for _, _, path in PLATFORMS:
            binary = self.release / 'bin/ww' / path / 'ww'
            binary.write_bytes(binary.read_bytes() + b'\n# distinct valid release B\n')
        self.write_manifest()
        self.run_installer(verify=True, success=True)
        self.assertNotEqual(requested, self.release_cid)
        self.release_cid = self.env['TEST_CID'] = requested
        if original_root:
            (self.root / 'root.block').write_bytes(root_a)
        for mode in ('install', 'explicit-recovery', 'locator-recovery'):
            with self.subTest(mode=mode):
                if mode == 'install':
                    result = self.run_installer()
                else:
                    self.prepare_recovery(b'{"version":1,"release":"'
                                          + requested.encode() + b'"}')
                    flags = ['--release', requested] if mode == 'explicit-recovery' else []
                    result = self.run_recovery(flags, expected_exit=1)
                    self.assertFalse(self.calls.exists(), 'substituted candidate executed')
                self.assertIn('manifest binding' if original_root else 'root CID mismatch',
                              result.stderr)
                self.assertNotIn('/bin/ww/', self.transport.read_text(),
                                 'binding failure reached candidate download')

    def test_other_valid_release_root_never_executes(self):
        self.substituted_release(original_root=False)

    def test_other_valid_manifest_under_requested_root_never_executes(self):
        self.substituted_release(original_root=True)

    def test_substituted_bootstrap_verifier_never_executes(self):
        verifier = Path(self.env['TEST_VERIFIERS']) / 'bin/ww/linux/x86_64/ww'
        verifier.write_bytes(verifier.read_bytes() + b'\n# corrupted verifier\n')
        self.run_installer()
        self.assertFalse(self.verifier_calls.exists())

    def test_root_bytes_must_match_requested_cid_before_candidate_fetch(self):
        root = self.root / 'root.block'
        root.write_bytes(root.read_bytes() + b'\x00')
        result = self.run_installer()
        self.assertIn('root CID mismatch', result.stderr)
        self.assertNotIn('/bin/ww/', self.transport.read_text())
        self.prepare_recovery()
        result = self.run_recovery(['--release', self.release_cid], expected_exit=1)
        self.assertIn('root CID mismatch', result.stderr)
        self.assertFalse(self.calls.exists())

    def test_unsupported_root_cid_profiles_and_noncanonical_padding(self):
        for cid in (cid_text(b'raw root', 0x55), cid_text(b'cbor root', 0x71),
                    'Qm' + 'a' * 44, self.release_cid.upper()):
            with self.subTest(cid=cid):
                self.refuse_before_transport(['--release', cid])
        alphabet = 'abcdefghijklmnopqrstuvwxyz234567'
        # A 36-byte CID has two unused low bits in its last base32 digit.
        index = alphabet.index(self.release_cid[-1])
        self.assertEqual(index % 4, 0)
        self.release_cid = self.env['TEST_CID'] = self.release_cid[:-1] + alphabet[index + 1]
        result = self.run_installer()
        self.assertIn('root CID mismatch', result.stderr)
        self.assertFalse(self.calls.exists())

    def test_malformed_root_membership_never_executes(self):
        manifest = self.manifest.read_bytes()
        raw_hash = cid_bytes(manifest)
        data = pb_field(10, b'\x08\x01')

        def link(name=b'CHECKSUMS.txt', cid=raw_hash, size=None):
            return (pb_field(10, cid) + pb_field(18, name)
                    + b'\x18' + varint(len(manifest) if size is None else size))

        valid_link = link()
        wrapped_link = pb_field(18, valid_link)
        cases = {
            'empty': b'',
            'missing-manifest': pb_field(18, link(b'OTHER')) + data,
            'wrong-manifest-digest': pb_field(18, link(cid=cid_bytes(b'other'))) + data,
            'unixfs-manifest-wrapper': pb_field(18, link(cid=cid_bytes(manifest, 0x70))) + data,
            'wrong-cid-version': pb_field(18, link(cid=b'\x02' + raw_hash[1:])) + data,
            'wrong-multihash': pb_field(18, link(cid=raw_hash[:2] + b'\x13' + raw_hash[3:])) + data,
            'truncated-cid': pb_field(18, link(cid=raw_hash[:-1])) + data,
            'wrong-manifest-size': pb_field(18, link(size=len(manifest) + 1)) + data,
            'duplicate-link': wrapped_link * 2 + data,
            'empty-name': pb_field(18, link(b'')) + data,
            'dot-name': pb_field(18, link(b'.')) + wrapped_link + data,
            'path-name': pb_field(18, link(b'../CHECKSUMS.txt')) + data,
            'nul-name': pb_field(18, link(b'CHECKSUMS.txt\x00')) + data,
            'non-ascii-name': pb_field(18, link(b'\xff')) + wrapped_link + data,
            'unsorted-links': pb_field(18, link(b'Z')) + wrapped_link + data,
            'missing-directory-data': wrapped_link,
            'wrong-directory-data': wrapped_link + pb_field(10, b'\x08\x02'),
            'duplicate-directory-data': wrapped_link + data * 2,
            'interleaved-data': wrapped_link + data + pb_field(18, link(b'Z')),
            'unknown-node-field': wrapped_link + data + b'\x18\x00',
            'unknown-link-field': pb_field(18, valid_link + b'\x20\x00') + data,
            'duplicate-hash': pb_field(18, pb_field(10, raw_hash) + valid_link) + data,
            'duplicate-name': pb_field(18, valid_link + pb_field(18, b'OTHER')) + data,
            'missing-size': pb_field(18, valid_link[:-len(varint(len(manifest))) - 1]) + data,
            'overlong-size': pb_field(18, link(b'0', size=0)[:-1] + b'\x80\x00') + wrapped_link + data,
            'overflow-size': pb_field(18, link(b'0', size=0)[:-1] + b'\xff' * 9 + b'\x02') + wrapped_link + data,
            'nonminimal-length': b'\x12' + bytes([len(valid_link) | 128, 0]) + valid_link + data,
            'overflow-length': b'\x12' + b'\xff' * 10 + b'\x00' + valid_link + data,
            'truncated-link': wrapped_link[:-1],
            'trailing-byte': wrapped_link + data + b'\x00',
            'nested-decoy-link': pb_field(10, wrapped_link) + data,
            'too-many-links': b''.join(pb_field(18, link(f'{i:04d}'.encode()))
                                     for i in range(1024)) + wrapped_link + data,
            'oversized-root': b'\x00' * (2097152 + 1),
        }
        self.prepare_recovery()
        for name, block in cases.items():
            with self.subTest(case=name):
                # The requested CID is genuine for each malformed root. Root
                # hashing alone must not make an ambiguous membership valid.
                self.set_root(block)
                result = self.run_installer()
                self.assertIn('Release root exceeds' if name == 'oversized-root'
                              else 'manifest binding', result.stderr)
                self.run_recovery(['--release', self.release_cid], expected_exit=1)
                self.assertFalse(self.calls.exists())
                self.assertNotIn('/bin/ww/', self.transport.read_text())

    def test_directory_orders_and_opaque_u64_artifact_sizes(self):
        root = manifest_root(self.manifest.read_bytes())
        # Native consumers also accept Data before all Links.
        self.set_root(root[-4:] + root[:-4])
        self.run_installer(success=True)
        self.calls.unlink()
        # The parser must not round or reject canonical unrelated u64 sizes.
        link = (pb_field(10, cid_bytes(b'other')) + pb_field(18, b'Z')
                + b'\x18' + varint(2**64 - 1))
        self.set_root(root[:-4] + pb_field(18, link) + root[-4:])
        self.run_installer(success=True)

    def test_maximum_manifest_remains_a_raw_block(self):
        manifest = self.manifest.read_bytes()
        # Fill with unique short entries, then one entry to reach the bound.
        records = []
        remaining = 1048576 - len(manifest)
        index = 0
        while remaining > 200:
            record = b'0' * 64 + f'  unused/{index}\n'.encode()
            records.append(record)
            remaining -= len(record)
            index += 1
        records.append(b'0' * 64 + b'  ' + b'z' * (remaining - 67) + b'\n')
        self.manifest.write_bytes(manifest + b''.join(records))
        self.assertEqual(self.manifest.stat().st_size, 1048576)
        self.sign()
        self.run_installer(success=True)

    def test_recovery_modified_executable_never_executes(self):
        self.prepare_recovery()
        (self.release / 'bin/ww/linux/x86_64/ww').write_bytes(b'tampered Host')
        result = self.run_recovery(['--release', self.release_cid], expected_exit=1)
        self.assertIn('Checksum mismatch', result.stderr)
        self.assertFalse(self.calls.exists())

    def refuse_before_transport(self, flags):
        before = {str(path.relative_to(self.home)): path.read_bytes()
                  for path in self.home.rglob('*') if path.is_file()}
        result = subprocess.run(['sh', str(INSTALLER), *flags,
                                 '--trusted-release-keys', str(self.keys)],
                                env=self.env, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(self.transport.exists(), 'refusal contacted IPFS')
        self.assertFalse(self.probes.exists(), 'refusal probed the platform')
        self.assertFalse(self.calls.exists(), 'refusal executed a candidate')
        after = {str(path.relative_to(self.home)): path.read_bytes()
                 for path in self.home.rglob('*') if path.is_file()}
        self.assertEqual(after, before)
        return result

    def automatic_descriptor(self, channel):
        # Real Map/DAG-CBOR envelope [1, {"channel": NAME}]. This transport
        # fixture isolates channel binding. test_install.py uses a full descriptor.
        text = channel.encode()
        assert 23 < len(text) < 256
        block = bytes.fromhex('8201a1676368616e6e656c78') + bytes([len(text)]) + text
        cid = cid_text(block, 0x71)
        descriptor = self.root / 'descriptor.cbor'
        descriptor.write_bytes(block)
        dag_json = self.root / 'descriptor.json'
        dag_json.write_text(json.dumps([1, {'channel': channel}]))
        (self.release / 'RELEASE-DESCRIPTOR').write_text(cid + '\n')
        self.env.update(TEST_DESCRIPTOR_CID=cid, TEST_DESCRIPTOR_JSON=str(dag_json),
                        TEST_DESCRIPTOR_BLOCK=str(descriptor))
        self.write_manifest()
        self.set_root(manifest_root(self.manifest.read_bytes(), block))

    def automatic_fixture(self, channel=RELEASE_NAME):
        self.automatic_descriptor(channel)
        # Provision ephemeral test trust in a private installer copy.
        content = self.installer.read_text()
        r1_key = (REPO / 'doc/release-keys').read_text().strip()
        self.assertEqual(hashlib.sha256(base64.b64decode(r1_key, validate=True)).hexdigest(),
                         R1_SPKI_SHA256)
        for production, fixture in [(r1_key, self.keys.read_text().strip()),
                                    ('/etc/wetware/release-keys', str(self.root / 'system-keys'))]:
            self.assertIn(production, content)
            content = content.replace(production, fixture)
        installer = self.root / 'install.sh'
        installer.write_text(content)
        self.discovery = self.root / 'resolved'
        self.inspection = {"Entry": {"Value": f"/ipfs/{self.release_cid}"},
                           "Validation": {"Valid": True, "Name": RELEASE_NAME}}
        self.discovery.write_text(json.dumps(self.inspection))
        self.env['TEST_RELEASE_NAME'] = RELEASE_NAME
        self.env['TEST_DISCOVERY'] = str(self.discovery)
        return installer

    def auto_install(self, installer, *args, success=False):
        result = subprocess.run(['sh', str(installer), *args], env=self.env,
                                capture_output=True, text=True, timeout=30)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse(self.calls.exists(), 'rejected candidate executed')
        return result

    def test_automatic_install_uses_discovered_root_and_embedded_key_all_platforms(self):
        installer = self.automatic_fixture()
        for os_name, arch, path in PLATFORMS:
            with self.subTest(os=os_name, arch=arch):
                self.env.update(TEST_OS=os_name, TEST_ARCH=arch)
                self.auto_install(installer, success=True)
                transport = self.transport.read_text().splitlines()
                self.assertIn(f'name get {RELEASE_NAME}', transport)
                self.assertIn(f'cat /ipfs/{self.release_cid}/bin/ww/{path}/ww --progress=false', transport)
                calls = self.calls.read_text().splitlines()
                self.assertEqual(calls, ['--version'])
                self.assertIn(f'cat /ipfs/{R1_ROOT}/bin/ww/{path}/ww --progress=false', transport)
                self.assertIn(f'install --release {self.release_cid}', self.verifier_calls.read_text())
                self.calls.unlink()
                self.transport.unlink()

    def test_embedded_key_permissions_with_group_writable_umask(self):
        installer = self.automatic_fixture()
        observed = self.root / 'observed-key'
        result = subprocess.run(['sh', str(installer)], umask=0o002,
                                env=dict(self.env, TEST_CAPTURE_KEY=str(observed)),
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(observed.stat().st_mode & 0o777, 0o600)

    def test_invalid_ipns_and_malformed_targets_never_fetch_release_content(self):
        installer = self.automatic_fixture()
        valid = self.discovery.read_bytes()
        malformed = [b'', b'{', b'x' * 10241]
        for changes in ({"Valid": False}, {"Valid": 1}, {"Name": "wrong signer"}):
            verdict = dict(self.inspection["Validation"], **changes)
            malformed.append(json.dumps(dict(self.inspection, Validation=verdict)).encode())
        for path in ("/ipns/" + RELEASE_NAME, "/ipfs/" + self.release_cid + "/child",
                     "/ipfs/" + self.release_cid + "\n", "/ipfs/" + self.release_cid[:-1] + "b"):
            malformed.append(json.dumps(dict(self.inspection, Entry={"Value": path})).encode())
        for index, data in enumerate(malformed):
            with self.subTest(case=index):
                self.discovery.write_bytes(data)
                self.auto_install(installer)
                self.assertFalse(any(op.startswith(('cat ', 'block '))
                                     for op in self.transport.read_text().splitlines()))
                self.transport.unlink()
        self.discovery.write_bytes(valid)
        self.env['TEST_RESOLVE_STATUS'] = '1'
        self.auto_install(installer)
        self.assertNotIn('cat ', self.transport.read_text())

    def test_signed_release_for_another_channel_cannot_start_verifier(self):
        installer = self.automatic_fixture(channel='k' * 62)
        self.auto_install(installer)
        self.assertFalse(self.verifier_calls.exists())

    def test_large_existing_transport_log_does_not_limit_metadata_fetch(self):
        installer = self.automatic_fixture()
        self.transport.write_bytes(b'x' * (1024 * 1024 + 1))
        self.auto_install(installer, success=True)

    def test_oversized_descriptor_binding_refused_before_verifier(self):
        installer = self.automatic_fixture()
        (self.release / 'RELEASE-DESCRIPTOR').write_bytes(b'x' * 129)
        result = self.auto_install(installer)
        self.assertIn('exceeds metadata size limit', result.stderr)
        self.assertFalse(self.verifier_calls.exists())

    def test_changed_descriptor_cannot_start_verifier(self):
        installer = self.automatic_fixture()
        block = Path(self.env['TEST_DESCRIPTOR_BLOCK'])
        block.write_bytes(bytes(len(block.read_bytes())))
        self.auto_install(installer)
        self.assertFalse(self.verifier_calls.exists())

    def test_explicit_install_and_verify_require_external_keyring(self):
        installer = self.automatic_fixture()
        self.auto_install(installer, '--release', self.release_cid)
        self.assertNotIn('name get', self.transport.read_text())
        self.auto_install(installer, '--verify-manifest', str(self.manifest),
                          '--signature', str(self.signature))

    def test_automatic_discovery_requires_kubo_and_supported_platform(self):
        installer = self.automatic_fixture()
        self.env['TEST_ID_STATUS'] = '1'
        self.auto_install(installer)
        self.assertEqual(['id'], self.transport.read_text().splitlines())
        self.transport.unlink()
        self.env.update(TEST_ID_STATUS='0', TEST_ARCH='riscv64')
        result = self.auto_install(installer)
        self.assertIn('Unsupported architecture', result.stderr)
        self.assertFalse(self.transport.exists())

    def test_automatic_discovery_refuses_existing_consumer_state(self):
        installer = self.automatic_fixture()
        state = self.home / '.ww/release-consumer'
        state.parent.mkdir()
        state.mkdir()
        self.auto_install(installer)
        state.rmdir()
        state.symlink_to('missing')
        self.auto_install(installer)
        self.assertTrue(state.is_symlink())

    def test_configured_keyring_excludes_embedded_authority(self):
        installer = self.automatic_fixture()
        system_keys = self.root / 'system-keys'
        system_keys.write_bytes(self.other_keys.read_bytes())
        self.auto_install(installer)
        self.auto_install(installer, '--trusted-release-keys', str(self.keys), success=True)
        self.calls.unlink()
        system_keys.unlink()
        system_keys.symlink_to('missing')
        self.auto_install(installer)
        system_keys.unlink()
        self.auto_install(installer, '--trusted-release-keys', str(system_keys))

    def test_automatic_install_rejects_missing_signature_and_substituted_bytes(self):
        installer = self.automatic_fixture()
        signature = self.signature.read_bytes()
        self.signature.unlink()
        self.auto_install(installer)
        sign_manifest(self.other_private, self.manifest)
        (self.release / 'release-keys').write_bytes(self.other_keys.read_bytes())
        self.auto_install(installer)
        self.signature.write_bytes(signature)
        (self.release / 'bin/ww/linux/x86_64/ww').write_bytes(b'changed executable')
        result = self.auto_install(installer)
        self.assertIn('Checksum mismatch', result.stderr)
        self.assertNotIn('/release-keys', self.transport.read_text())

    def test_release_rejects_paths_and_malformed_cids_before_transport(self):
        for value in ('test-release', '/ipfs/' + self.release_cid, '/ipns/untrusted',
                      self.release_cid + '/other', self.release_cid + '?query',
                      self.release_cid[:-1], self.release_cid + 'a', self.release_cid[:-1] + '0',
                      self.release_cid + '\n' + self.release_cid, ' ', '--recover'):
            with self.subTest(value=value):
                self.refuse_before_transport(['--release', value])

    def test_retired_version_option_has_no_alias(self):
        self.refuse_before_transport(['--version', 'test-release'])

    def test_recovery_requires_pending_journal_before_transport(self):
        for flags in (['--recover', '--timeout-secs', '15'], ['--recover', '--timeout-secs', '15', '--release', 'test-release']):
            with self.subTest(flags=flags):
                result = self.refuse_before_transport(flags)
                self.assertIn('pending Host upgrade', result.stderr)

    def test_recovery_requires_positive_timeout_before_transport(self):
        self.prepare_recovery()
        for flags in ([], ['--timeout-secs', '0'], ['--timeout-secs', '-1'],
                      ['--timeout-secs', 'NaN'], ['--timeout-secs', '1.5'],
                      ['--timeout-secs', '4294967296'],
                      ['--timeout-secs', '18446744073709551616'],
                      ['--timeout-secs']):
            with self.subTest(flags=flags):
                result = self.refuse_before_transport(['--recover', '--release', self.release_cid, *flags])
                self.assertIn('--timeout-secs', result.stderr)

    def test_install_forwards_configured_ipfs_endpoint(self):
        self.env['IPFS_API'] = 'http://127.0.0.1:41003'
        self.run_installer(success=True)
        for call in self.verifier_calls.read_text().splitlines():
            self.assertIn('--ipfs-url http://127.0.0.1:41003', call)

    def prepare_recovery(self, pointer=None):
        root = self.home / '.ww'
        root.mkdir(exist_ok=True)
        (root / 'host-upgrade.json').write_bytes(b'pending generation sentinel')
        if pointer is not None:
            (root / 'installed-release.json').write_bytes(pointer)

    def test_recovery_requires_canonical_local_locator_or_explicit_release(self):
        self.prepare_recovery()
        cases = [None, b'', b'{}', b'{"version":1,"release":""}',
                 b'{"version":2,"release":"' + self.release_cid.encode() + b'"}',
                 b'{"version":1,"release":"' + self.release_cid.encode() + b'"}\n',
                 b'{"release":"' + self.release_cid.encode() + b'","version":1}',
                 b'{"version":1,"release":"/ipns/untrusted"}',
                 b'{"version":1,"release":"' + self.release_cid.encode() + b'","extra":true}',
                 b'X' * 1025, b'{"version":1,"release":"' + self.release_cid.encode() + b'"}\x00']
        pointer = self.home / '.ww/installed-release.json'
        for contents in cases:
            with self.subTest(contents=contents):
                if contents is not None:
                    pointer.write_bytes(contents)
                result = self.refuse_before_transport(['--recover', '--timeout-secs', '15'])
                self.assertIn('--release', result.stderr)
        pointer.unlink()
        pointer.symlink_to(self.manifest)
        self.refuse_before_transport(['--recover', '--timeout-secs', '15'])

    def test_recovery_rejects_symlinked_journal_before_transport(self):
        self.prepare_recovery()
        journal = self.home / '.ww/host-upgrade.json'
        journal.unlink()
        journal.symlink_to(self.manifest)
        self.refuse_before_transport(['--recover', '--timeout-secs', '15', '--release', self.release_cid])

    def run_recovery(self, flags=(), expected_exit=0):
        self.env['TEST_CID'] = self.release_cid
        before = {str(path.relative_to(self.home)): path.read_bytes()
                  for path in self.home.rglob('*') if path.is_file()}
        result = subprocess.run(['sh', str(INSTALLER), '--recover', '--timeout-secs', '15', *flags,
                                 '--trusted-release-keys', str(self.keys)],
                                env=self.env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, expected_exit, result.stdout + result.stderr)
        after = {str(path.relative_to(self.home)): path.read_bytes()
                 for path in self.home.rglob('*') if path.is_file()}
        self.assertEqual(after, before)
        self.assertFalse((self.home / '.ww/bin').exists(), 'recovery invoked install')
        return result

    def test_recovery_handoff_fetches_only_authenticated_host_all_platforms(self):
        self.prepare_recovery(b'{"version":1,"release":"' + self.release_cid.encode() + b'"}')
        for os_name, arch, path in PLATFORMS:
            with self.subTest(os=os_name, arch=arch):
                self.env.update(TEST_OS=os_name, TEST_ARCH=arch)
                self.run_recovery()
                self.assertEqual(self.calls.read_text().splitlines(), [
                    f'upgrade --recover-only --release {self.release_cid} '
                    f'--ipfs-url http://localhost:5001 --timeout-secs 15 --trusted-release-keys {self.keys}'])
                self.assertEqual(self.transport.read_text().splitlines(), [
                    'id', f'cat /ipfs/{self.release_cid}/CHECKSUMS.txt --progress=false',
                    f'cat /ipfs/{self.release_cid}/CHECKSUMS.txt.sig --progress=false',
                    f'block get {self.release_cid}',
                    f'cat /ipfs/{self.release_cid}/bin/ww/{path}/ww --progress=false'])
                self.calls.unlink()
                self.transport.unlink()

    def test_recovery_explicit_release_propagates_native_refusal_and_endpoint(self):
        self.prepare_recovery(b'invalid pointer bypassed by explicit selection')
        self.env['TEST_RECOVERY_EXIT'] = '43'
        self.run_recovery(['--release', self.release_cid, '--ipfs-url', 'http://127.0.0.1:41001'],
                          expected_exit=43)
        self.assertIn('--ipfs-url http://127.0.0.1:41001', self.calls.read_text())
        self.assertEqual(len(self.calls.read_text().splitlines()), 1)

    def test_recovery_explicit_release_uses_ipfs_api_without_local_pointer(self):
        self.prepare_recovery()
        self.env['IPFS_API'] = 'http://127.0.0.1:41002'
        self.run_recovery(['--release', self.release_cid])
        self.assertIn('--ipfs-url http://127.0.0.1:41002', self.calls.read_text())

    def test_recovery_authentication_failure_does_not_execute(self):
        self.prepare_recovery()
        self.signature.write_bytes(b'\x01' * 64)
        self.run_recovery(['--release', self.release_cid], expected_exit=1)
        self.assertFalse(self.calls.exists())

    def test_recovery_conflicts_with_manifest_verification(self):
        result = self.refuse_before_transport(['--recover', '--verify-manifest', str(self.manifest),
                                              '--signature', str(self.signature)])
        self.assertIn('cannot combine', result.stderr)

    def test_inherited_temporary_directory_is_never_removed(self):
        directory = self.root / 'unrelated-state'
        directory.mkdir()
        sentinel = directory / 'sentinel'
        sentinel.write_bytes(b'private unrelated state')
        self.env['WW_TMPDIR'] = str(directory)
        self.refuse_before_transport(['--recover', '--timeout-secs', '15'])
        self.assertTrue(directory.exists(), 'cleanup removed inherited WW_TMPDIR')
        self.assertEqual(sentinel.read_bytes(), b'private unrelated state')

    def test_legacy_consumer_accepts_signed_tree(self):
        # Frozen pre-authentication installer ignores the new detached signature.
        # This proves producer-first compatibility, not legacy consumer security.
        self.run_installer(success=True, legacy=True)
        self.assertEqual(len(self.calls.read_text().splitlines()), 3)

    def test_small_order_keys_and_signature_points_fail(self):
        ys = [
            '00' * 32,
            '01' + '00' * 31,
            '26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05',
            'c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a',
            'ec' + 'ff' * 30 + '7f',
            'ed' + 'ff' * 30 + '7f',
            'ee' + 'ff' * 30 + '7f',
        ]
        original_key = self.keys.read_bytes()
        original_signature = self.signature.read_bytes()
        # R=identity, S=0 is a universal forgery for an identity public key
        # under OpenSSL alone. The installer must reject that provisioned key.
        forgery = bytes.fromhex('01' + '00' * 63)
        for y in ys:
            for sign_bit in (0, 128):
                point = bytes.fromhex(y)
                point = point[:-1] + bytes([point[-1] | sign_bit])
                with self.subTest(point=point.hex()):
                    self.keys.write_bytes(base64.b64encode(
                        bytes.fromhex('302a300506032b6570032100') + point) + b'\n')
                    # A valid R isolates rejection of the keyring, including a
                    # weak second key after an otherwise successful signature.
                    self.signature.write_bytes(original_signature)
                    result = self.run_installer(verify=True)
                    self.assertIn('Weak Ed25519 release point', result.stderr)
                    self.keys.write_bytes(original_key + self.keys.read_bytes())
                    result = self.run_installer(verify=True)
                    self.assertIn('Weak Ed25519 release point', result.stderr)
                    self.keys.write_bytes(original_key)
                    self.signature.write_bytes(point + original_signature[32:])
                    result = self.run_installer(verify=True)
                    self.assertIn('Weak Ed25519 release point', result.stderr)
        self.keys.write_bytes(base64.b64encode(
            bytes.fromhex('302a300506032b6570032100') + bytes.fromhex('01' + '00' * 31)) + b'\n')
        self.signature.write_bytes(forgery)
        self.run_installer(verify=True)

    def test_verify_only_authenticates_without_transport(self):
        self.run_installer(verify=True, success=True)
        self.assertFalse(self.calls.exists())

    def test_manifest_modification(self):
        self.manifest.write_bytes(self.manifest.read_bytes().replace(b'fhs', b'FHS'))
        self.run_installer()

    def test_modified_artifact(self):
        (self.release / 'bin/ww/linux/x86_64/ww').write_bytes(b'evil')
        self.run_installer()

    def test_attacker_generated_checksums(self):
        (self.release / 'bin/ww/linux/x86_64/ww').write_bytes(b'evil')
        self.write_manifest()
        self.sign(self.other_private)
        self.run_installer()

    def test_wrong_trusted_key(self):
        self.run_installer(keys=self.other_keys)

    def test_fetched_public_key_is_not_a_trust_anchor(self):
        (self.release / 'release-keys').write_bytes(self.other_keys.read_bytes())
        self.sign(self.other_private)
        self.run_installer()
        calls = self.transport.read_text() if self.transport.exists() else ''
        self.assertNotIn('/release-keys', calls)

    def test_missing_manifest_and_signature(self):
        for path in (self.manifest, self.signature):
            with self.subTest(path=path.name):
                data = path.read_bytes()
                path.unlink()
                self.run_installer()
                path.write_bytes(data)

    def test_malformed_signature(self):
        original = self.signature.read_bytes()
        for data in (b'', original[:-1], original + b'\n', b'X' * 64,
                     base64.b64encode(original), b'\0' * 64):
            with self.subTest(length=len(data)):
                self.signature.write_bytes(data)
                self.run_installer()
        self.signature.write_bytes(original)

    def test_missing_platform_entry_and_payload(self):
        original = self.manifest.read_bytes()
        self.manifest.write_bytes(b''.join(line for line in original.splitlines(keepends=True)
                                          if b'bin/ww/linux/x86_64/ww' not in line))
        self.sign()
        self.run_installer()
        self.manifest.write_bytes(original)
        self.sign()
        (self.release / 'bin/ww/linux/x86_64/ww').unlink()
        self.run_installer()

    def test_signed_digest_mismatch_and_malformed_manifests(self):
        original = self.manifest.read_bytes()
        record = original.splitlines(keepends=True)[1]
        digest = b'0' * 64
        cases = [original.replace(b'# sha256', b'# blake3'),
                 original.replace(b'# sha256', b'# SHA256'),
                 original[9:], original + record, original[:-1],
                 original.replace(b'\n', b'\r\n'), b'# sha256\n',
                 b'# sha256\n' + digest + b'  ../escape\n',
                 b'# sha256\n' + digest + b'  /absolute\n',
                 b'# sha256\n' + digest + b'  fhs//name\n',
                 b'# sha256\n' + digest + b'  fhs/./name\n',
                 b'# sha256\n' + digest + b'  fhs/../name\n',
                 b'# sha256\n' + digest + b'  fhs/name with space\n',
                 b'# sha256\n' + digest + b'  fhs/name\x00\n',
                 b'# sha256\n' + b'A' * 64 + b'  fhs/name\n',
                 b'# sha256\n' + digest + b' *fhs/name\n',
                 b'# sha256\n' + b'x' * (1024 * 1024)]
        for index, data in enumerate(cases):
            with self.subTest(case=index):
                self.manifest.write_bytes(data)
                self.sign()
                self.run_installer(verify=True)

    def test_rotation_overlap_and_retirement(self):
        overlap = self.root / 'overlap'
        overlap.write_bytes(self.keys.read_bytes() + self.other_keys.read_bytes())
        self.run_installer(verify=True, success=True, keys=overlap)
        self.sign(self.other_private)
        self.run_installer(verify=True, success=True, keys=overlap)
        self.run_installer(verify=True, success=True, keys=self.other_keys)
        self.run_installer(verify=True, keys=self.keys)
        self.sign()
        self.run_installer(verify=True, keys=self.other_keys)

    def test_malformed_keyrings(self):
        original = self.keys.read_bytes()
        wrong_der = base64.b64encode(b'X' * 44) + b'\n'
        for data in (b'', original[:-1], original + original,
                     original + self.other_keys.read_bytes() + original,
                     b'not base64\n', wrong_der, original + b'\n',
                     original.replace(b'\n', b'\r\n'), b' ' + original):
            with self.subTest(keyring=data):
                self.keys.write_bytes(data)
                self.run_installer(verify=True)
        self.keys.unlink()
        self.run_installer(verify=True)

    def test_failed_transport_never_executes(self):
        for path in ('CHECKSUMS.txt', 'CHECKSUMS.txt.sig', 'root', 'fhs/bin/status.wasm'):
            with self.subTest(path=path):
                self.env['FAIL_FETCH'] = path
                self.run_installer()



if __name__ == '__main__':
    unittest.main(verbosity=2)
