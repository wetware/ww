"""Exercise the signed installer's post-install step without fetching a release."""
import errno
import fcntl
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import struct
import subprocess
import tempfile
import sys
import termios
import time
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / 'scripts/install.sh'
PROMPT = b'Add Wetware to your shell PATH? [y/N]'


def piped_terminal(environment, answer=None, interactive_caller=True, timeout=5):
    """Pipe INSTALL_TEST into sh with an independent 100-column controlling terminal."""
    pid, master = pty.fork()
    if pid == 0:
        fcntl.ioctl(1, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 100, 0, 0))
        flag = '-ic' if interactive_caller else '-c'
        os.execve('/bin/bash', ['bash', '--noprofile', '--norc', flag,
                                'cat "$INSTALL_TEST" | /bin/sh'], environment)
    output = bytearray()
    deadline = time.monotonic() + timeout
    try:
        while True:
            wait = deadline - time.monotonic()
            if wait <= 0 or not select.select([master], [], [], wait)[0]:
                raise AssertionError('installer blocked: ' + output.decode(errors='replace'))
            try:
                data = os.read(master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
            if not data:
                break
            output.extend(data)
            if answer is not None and PROMPT in output:
                os.write(master, answer)
                answer = None
    finally:
        os.close(master)
        if time.monotonic() >= deadline:
            os.kill(pid, signal.SIGKILL)
        _, status = os.waitpid(pid, 0)
    if os.waitstatus_to_exitcode(status) != 0:
        raise AssertionError(output)
    return output.decode(errors='replace').replace('\r\n', '\n')


class InstallerPath(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        installer = INSTALLER.read_text()
        start = installer.index('configure_path() {')
        end = installer.index('\n}', start) + 2
        cls.script = 'set -eu\nWW_HOME="$HOME/.ww"\n' + installer[start:end] + '\nconfigure_path\n'

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name).resolve()
        self.home = root / """home ' " space"""
        self.home.mkdir(mode=0o700)
        self.bin = self.home / '.ww/bin'
        self.bin.mkdir(parents=True)
        (self.bin / 'ww').write_text('#!/bin/sh\necho ww-from-test-home\n')
        (self.bin / 'ww').chmod(0o700)
        self.script_path = root / 'post-install.sh'
        self.script_path.write_text(self.script)
        self.env = dict(PATH=os.environ['PATH'], HOME=str(self.home),
                        SHELL='/bin/zsh', TERM='xterm', INSTALL_TEST=str(self.script_path))

    def install(self, *, answer=None, tty=True, interactive_caller=True, timeout=5, **env):
        environment = dict(self.env, **env)
        if not tty:
            result = subprocess.run(['/bin/sh'], input=self.script, text=True,
                                    capture_output=True, env=environment,
                                    start_new_session=True, timeout=4)
            self.assertEqual(result.returncode, 0, result.stderr)
            return result.stdout + result.stderr

        return piped_terminal(environment, answer, interactive_caller, timeout)

    def assert_ww_runs(self, shell, args, **env):
        binary = shutil.which(shell)
        if binary:
            run = subprocess.run([binary, *args], env=dict(self.env, **env),
                                 text=True, capture_output=True, timeout=5,
                                 cwd=self.home)
            self.assertEqual(run.returncode, 0, run.stderr)
            self.assertIn('ww-from-test-home', run.stdout)
            if shell == 'tcsh':
                path = run.stdout.splitlines()[-2]
                self.assertEqual(path.split(':').count(str(self.bin)), 1)

    def test_piped_zsh_opt_in_preserves_bytes_mode_and_repeated_install(self):
        config = self.home / '.zshrc'
        original = b'# existing: \xff\r\n# last line'
        config.write_bytes(original)
        config.chmod(0o640)
        output = self.install(answer=b'y\n')
        self.assertIn(PROMPT.decode(), output)
        edited = config.read_bytes()
        self.assertTrue(edited.startswith(original))
        self.assertGreater(len(edited), len(original))
        self.assertEqual(config.stat().st_mode & 0o777, 0o640)
        self.install(answer=b'yes\n')
        self.assertEqual(config.read_bytes(), edited)
        self.assert_ww_runs('zsh', ['-ic', 'ww'])

    def test_decline_default_and_terminal_eof(self):
        for reply in [b'n\n', b'\n', b'\x04']:
            with self.subTest(reply=reply):
                output = self.install(answer=reply)
                self.assertIn(PROMPT.decode(), output)
                self.assertIn('.ww/bin', output)
                self.assertEqual(list(self.home.iterdir()), [self.home / '.ww'])

    def test_prompt_times_out(self):
        output = self.install(timeout=20)
        self.assertIn(PROMPT.decode(), output)
        self.assertIn('.ww/bin', output)
        self.assertEqual(list(self.home.iterdir()), [self.home / '.ww'])

    def test_busybox_ash_interpreter(self):
        busybox = shutil.which('busybox')
        if not busybox:
            self.skipTest('BusyBox unavailable')
        run = subprocess.run([busybox, 'ash'], env=dict(self.env, SHELL='/bin/ash'),
                             input=self.script, text=True, capture_output=True,
                             timeout=5, start_new_session=True)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn('export PATH', run.stdout)
        self.assertNotIn(PROMPT.decode(), run.stdout)
        self.assertEqual(list(self.home.iterdir()), [self.home / '.ww'])

    def test_unattended(self):
        for options in [dict(tty=False), dict(CI='true'), dict(interactive_caller=False),
                        dict(NONINTERACTIVE='1')]:
            with self.subTest(options=options):
                output = self.install(**options)
                self.assertNotIn(PROMPT.decode(), output)
                self.assertIn('.ww/bin', output)
                self.assertEqual(list(self.home.iterdir()), [self.home / '.ww'])

    def test_already_on_path(self):
        output = self.install(PATH=str(self.bin) + ':' + self.env['PATH'])
        self.assertNotIn(PROMPT.decode(), output)
        self.assertEqual(list(self.home.iterdir()), [self.home / '.ww'])

    def test_shadowed_installed_command(self):
        earlier = self.home / 'bin'
        earlier.mkdir()
        shadow = earlier / 'ww'
        shadow.write_text('#!/bin/sh\nexit 1\n')
        shadow.chmod(0o700)
        path = os.pathsep.join((str(earlier), str(self.bin), self.env['PATH']))
        output = self.install(PATH=path)
        self.assertNotIn(PROMPT.decode(), output)
        self.assertIn('~/.ww/bin/ww run', output)
        self.assertNotIn('  ww run', output)

    def test_shell_specific_startup_files(self):
        bash_config = '.bash_profile' if sys.platform == 'darwin' else '.bashrc'
        for shell, config, args, environment in [
            ('bash', bash_config, ['--noprofile', '--norc', '-c',
                                   '. "$HOME/' + bash_config + '"; ww'], {}),
            ('zsh', 'dot/.zshrc', ['-ic', 'ww'], {'ZDOTDIR': str(self.home / 'dot')}),
            ('fish', 'config/fish/conf.d/ww-path.fish', ['-ic', 'ww'],
             {'XDG_CONFIG_HOME': str(self.home / 'config')}),
            ('csh', '.cshrc', ['-ic', 'ww'], {}),
            ('tcsh', '.tcshrc', ['-ic', 'source "$HOME/.tcshrc"; printenv PATH; ww'], {}),
        ]:
            with self.subTest(shell=shell):
                dest = self.home / config
                dest.parent.mkdir(exist_ok=True, parents=True)
                for parent in dest.parents:
                    if parent == self.home:
                        break
                    parent.chmod(0o700)
                if shell != 'fish':
                    dest.touch(mode=0o600)
                output = self.install(SHELL='/bin/' + shell, answer=b'yes\n', **environment)
                self.assertIn(PROMPT.decode(), output)
                self.assertIn('.ww/bin', dest.read_text(), output)
                self.assert_ww_runs(shell, args, **environment)

    def test_ksh_existing_env(self):
        home = self.home.rename(self.home.parent / 'ksh-home')
        self.home = home
        self.env.update(HOME=str(home), SHELL='/bin/ksh', ENV=str(home / '.kshrc'))
        config = home / '.kshrc'
        self.assertNotIn(PROMPT.decode(), self.install())
        config.touch(mode=0o600)
        self.assertIn(PROMPT.decode(), self.install(answer=b'y\n'))
        self.assertIn('.ww/bin', config.read_text())
        self.assert_ww_runs('ksh', ['-ic', 'ww'])

    def test_manual_shells(self):
        for shell in ['sh', 'dash', 'ash', 'nu']:
            with self.subTest(shell=shell):
                output = self.install(SHELL='/bin/' + shell)
                self.assertNotIn(PROMPT.decode(), output)
                self.assertEqual(list(self.home.iterdir()), [self.home / '.ww'])
                if shell == 'nu':
                    self.assertIn('$env.PATH', output)
                else:
                    self.assertIn('export PATH', output)
                    command = next(line.strip() for line in output.splitlines()
                                   if 'export PATH' in line)
                    self.assert_ww_runs(shell, ['-c', command + '\nww'])
                    if shell == 'ash':
                        self.assert_ww_runs('busybox', ['ash', '-c', command + '\nww'])

    def test_rejects_external_and_symlinked_startup_directories(self):
        external = self.home.parent / 'external'
        external.mkdir()
        symlink = self.home / 'dotfiles'
        symlink.symlink_to(external)
        for directory in [external, symlink]:
            with self.subTest(directory=directory):
                output = self.install(answer=b'y\n', ZDOTDIR=str(directory))
                self.assertIn('.ww/bin', output)
                self.assertEqual(list(external.iterdir()), [])

    def test_short_write_restores_existing_config(self):
        config = self.home / '.zshrc'
        original = b'# Existing settings\n'
        config.write_bytes(original)
        config.chmod(0o600)
        # A real short write leaves five bytes on the descriptor. The installer
        # must truncate those bytes without touching the pre-existing content.
        short_write = 'write = os.write\nos.write = lambda fd, data: write(fd, data[:5])'
        self.script_path.write_text(self.script.replace('\nconfigure()\n',
                                                        '\n' + short_write + '\nconfigure()\n'))
        output = self.install(answer=b'y\n')
        self.assertIn('incomplete startup file write', output)
        self.assertEqual(config.read_bytes(), original)
        self.assertNotIn('✓ Configured', output)
        self.assertIn('export PATH', output)
        self.assertIn('~/.ww/bin/ww run', output)

    def test_refuses_symlink_and_unwritable_file(self):
        config = self.home / '.zshrc'
        other = self.home.parent / 'other'
        other.write_bytes(b'# other\n')
        config.symlink_to(other)
        self.assertIn('.ww/bin', self.install(answer=b'y\n'))
        self.assertTrue(config.is_symlink())
        self.assertEqual(other.read_bytes(), b'# other\n')
        config.unlink()
        os.link(other, config)
        self.assertIn('.ww/bin', self.install(answer=b'y\n'))
        self.assertTrue(config.samefile(other))
        self.assertEqual(other.read_bytes(), b'# other\n')
        config.unlink()
        config.write_text('# read only')
        config.chmod(0o400)
        self.assertIn('.ww/bin', self.install(answer=b'y\n'))
        self.assertEqual(config.read_text(), '# read only')
        self.assertEqual(config.stat().st_mode & 0o777, 0o400)


if __name__ == '__main__':
    unittest.main()
