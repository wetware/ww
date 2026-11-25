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


def piped_terminal(environment, answer=None, interactive_caller=True, timeout=5, columns=100, caller='/bin/bash'):
    """Pipe INSTALL_TEST into sh with an independent controlling terminal."""
    pid, master = pty.fork()
    if pid == 0:
        fcntl.ioctl(1, termios.TIOCSWINSZ, struct.pack('HHHH', 24, columns, 0, 0))
        flag = '-ic' if interactive_caller else '-c'
        options = ['--noprofile', '--norc'] if caller == '/bin/bash' else ['-f']
        os.execve(caller, [caller, *options, flag, 'cat "$INSTALL_TEST" | /bin/sh'], environment)
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

    def activation(self, output, configured_shell=None):
        if configured_shell is None:
            heading = 'To use ww in this terminal:'
        else:
            self.assertNotIn('To use ww in this terminal:', output)
            self.assertNotIn('  ww run', output)
            self.assertIn('~/.ww/bin/ww run', output)
            heading = f'In {configured_shell}, run:'
        self.assertIn(heading, output)
        return output.split(heading + '\n  ', 1)[1].splitlines()[0]

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
        repeated = self.install(answer=b'yes\n')
        self.assertIn('~/.ww/bin is already configured in ~/.zshrc', repeated)
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
            ('bash', bash_config, ['--noprofile', '--norc', '-ic'], {}),
            ('zsh', "dot $a;'!/.zshrc", ['-fic'], {'ZDOTDIR': str(self.home / "dot $a;'!")}),
            ('fish', "config $a;'!/fish/conf.d/ww-path.fish", ['--no-config', '-ic'],
             {'XDG_CONFIG_HOME': str(self.home / "config $a;'!")}),
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
                activation = self.activation(output, None if shell == 'bash' else shell)
                self.assertTrue(activation.startswith('source ~/'), activation)
                self.assert_ww_runs(shell, [*args, activation + '\nww'], **environment)
                if shell != 'bash':
                    self.assert_ww_runs(shell, ['-ic', 'ww'], **environment)

    def test_csh_startup_precedence(self):
        tcsh = Path('/bin/tcsh')
        shells = [shell for shell in (Path('/bin/csh'), tcsh, Path('/bin/bsd-csh'))
                  if shell.is_file()]
        if not shells:
            self.skipTest('Csh and Tcsh unavailable')
        cshrc = self.home / '.cshrc'
        tcshrc = self.home / '.tcshrc'
        original = b'# existing settings\n'
        for shell in shells:
            for existing in [(cshrc, tcshrc), (cshrc,), (tcshrc,), ()]:
                with self.subTest(shell=shell, existing=[p.name for p in existing]):
                    for config in (cshrc, tcshrc):
                        config.unlink(missing_ok=True)
                        if config in existing:
                            config.write_bytes(original)
                            config.chmod(0o600)
                    uses_tcsh = tcsh.is_file() and shell.samefile(tcsh)
                    selected = tcshrc if uses_tcsh and tcshrc in existing else cshrc
                    other = cshrc if selected == tcshrc else tcshrc
                    output = self.install(SHELL=str(shell), answer=b'y\n')
                    self.assertIn('✓ Added ~/.ww/bin to ~/' + selected.name, output)
                    self.assertIn(b'# Wetware PATH', selected.read_bytes())
                    if other in existing:
                        self.assertEqual(other.read_bytes(), original)
                    else:
                        self.assertFalse(other.exists())

                    activation = self.activation(output, shell.name)
                    self.assertEqual(activation, 'source ~/' + selected.name)
                    self.assert_ww_runs(str(shell), ['-fc', activation + '\nww'])

                    # First resolve ww through normal interactive startup. Read
                    # the selected file a second time, then check PATH and ww.
                    command = f'ww; source "$HOME/{selected.name}"; printenv PATH; ww'
                    result = subprocess.run([shell, '-ic', command], env=dict(self.env, SHELL=str(shell)),
                                            text=True, capture_output=True, timeout=5)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    lines = result.stdout.splitlines()
                    self.assertEqual(lines.count('ww-from-test-home'), 2, result.stderr)
                    self.assertEqual(lines[-2].split(':').count(str(self.bin)), 1)

    def test_login_shell_path_with_misleading_name(self):
        shell = self.home / 'tcsh'
        shell.symlink_to('/bin/bash')
        output = self.install(SHELL=str(shell), answer=b'y\n')
        self.assertIn('✓ Added ~/.ww/bin to ~/.cshrc', output)
        self.assertEqual(self.activation(output, 'tcsh'), 'source ~/.cshrc')

    def test_unknown_csh_with_tcshrc_uses_manual_guidance(self):
        shell = self.home / 'csh'
        shell.write_text('#!/bin/sh\ntouch "$HOME/unexpected-execution"\n')
        shell.chmod(0o700)
        for name in ('.tcshrc', '.cshrc'):
            (self.home / name).write_text('# existing settings')
        output = self.install(SHELL=str(shell), answer=b'y\n')
        self.assertNotIn(PROMPT.decode(), output)
        self.assertIn('set path =', output)
        self.assertNotIn('✓ Added', output)
        self.assertFalse((self.home / 'unexpected-execution').exists())
        for name in ('.tcshrc', '.cshrc'):
            self.assertEqual((self.home / name).read_text(), '# existing settings')

    def test_ksh_existing_env(self):
        home = self.home.rename(self.home.parent / 'ksh-home')
        self.home = home
        self.env.update(HOME=str(home), SHELL='/bin/ksh', ENV=str(home / '.kshrc'))
        config = home / '.kshrc'
        self.assertNotIn(PROMPT.decode(), self.install())
        config.touch(mode=0o600)
        output = self.install(answer=b'y\n')
        self.assertIn(PROMPT.decode(), output)
        activation = self.activation(output, 'ksh')
        self.assertEqual(activation, '. ~/.kshrc')
        self.assert_ww_runs('ksh', ['-c', activation + '\nww'])
        self.assertIn('.ww/bin', config.read_text())
        self.assert_ww_runs('ksh', ['-ic', 'ww'])

    def test_nonprintable_startup_path_uses_new_terminal_fallback(self):
        directory = self.home / 'line\nbreak'
        output = self.install(answer=b'y\n', ZDOTDIR=str(directory))
        self.assertIn('✓ Added', output)
        self.assertIn('Open a new terminal', output)
        self.assertNotIn('To use ww in this terminal:', output)
        self.assertNotIn('  ww run', output)
        self.assertIn('~/.ww/bin/ww run', output)
        self.assertIn('# Wetware PATH', (directory / '.zshrc').read_text())

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
        self.assertNotIn('✓ Added', output)
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
