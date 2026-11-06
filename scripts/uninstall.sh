#!/bin/sh
# Wetware uninstaller
# Removes ~/.ww and prints PATH cleanup guidance.
set -eu

WW_HOME="${HOME}/.ww"

if [ ! -d "$WW_HOME" ]; then
  echo "Nothing to uninstall: ${WW_HOME} does not exist."
  exit 0
fi

echo "This will remove ${WW_HOME} and all its contents:"
echo "  - ${WW_HOME}/bin/ww        (binary)"
echo "  - ${WW_HOME}/lib/std/      (standard library)"
echo "  - ${WW_HOME}/etc/          (config templates)"
echo ""
printf "Continue? [y/N] "
read -r REPLY
case "$REPLY" in
  y|Y|yes|YES) ;;
  *)
    echo "Aborted."
    exit 0
    ;;
esac

# Hold the same kernel lock as native installation/upgrade until rm exits.
# Python is required here because macOS does not ship a flock command. Isolated
# mode excludes user Python startup configuration; failures leave .ww intact.
python3 -I -S - "$HOME" <<'PY'
import fcntl
import os
import stat
import sys

try:
    home = sys.argv[1]
    if not os.path.isabs(home) or not stat.S_ISDIR(os.lstat(home).st_mode):
        raise ValueError("installation HOME must be an absolute directory without symlink traversal")
    fd = os.open(os.path.join(home, ".ww-installation.lock"),
                 os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    metadata = os.fstat(fd)
    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1
            or metadata.st_uid != os.geteuid() or metadata.st_mode & 0o022):
        raise ValueError("installation lock must be a private regular file owned by this user")
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        raise ValueError("installation mutation already in progress; retry after it completes") from None
    directory = os.path.join(home, ".ww")
    if not stat.S_ISDIR(os.lstat(directory).st_mode):
        raise ValueError("installation directory is not a directory")
    try:
        os.lstat(os.path.join(directory, "host-upgrade.json"))
    except FileNotFoundError:
        pass
    else:
        raise ValueError("pending Host upgrade; recover the recorded transaction before uninstalling")
    # The deleting process owns the lock, even if its parent shell is killed.
    os.set_inheritable(fd, True)
    os.execv("/bin/rm", ["rm", "-rf", "--", directory])
except (OSError, ValueError) as error:
    sys.exit(f"Uninstall refused: {error}")
PY
echo "Removed ${WW_HOME}"

echo ""
echo "Clean up your PATH by removing the ww entry from your shell config:"
echo ""
echo "  # bash: edit ~/.bashrc and remove:"
echo "  export PATH=\"${WW_HOME}/bin:\$PATH\""
echo ""
echo "  # zsh: edit ~/.zshrc and remove:"
echo "  export PATH=\"${WW_HOME}/bin:\$PATH\""
echo ""
echo "  # fish: run:"
echo "  set -e fish_user_paths[(contains -i ${WW_HOME}/bin \$fish_user_paths)]"
echo ""
echo "Wetware has been uninstalled."
