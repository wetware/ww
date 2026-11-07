#!/bin/sh
# Remove managed software through the native ownership and recovery checks.
# Identity, namespaces, application data, logs, and signed state are preserved.
set -eu

WW_HOME="${HOME}/.ww"
if [ ! -e "$WW_HOME" ] && [ ! -L "$WW_HOME" ]; then
  echo "Nothing to uninstall: ${WW_HOME} does not exist."
  exit 0
fi

if [ -L "$WW_HOME" ] || [ -L "$WW_HOME/bin" ] || [ -L "$WW_HOME/bin/ww" ] \
    || [ ! -f "$WW_HOME/bin/ww" ] || [ ! -x "$WW_HOME/bin/ww" ]; then
  echo "Uninstall refused: no regular managed Host executable at ${WW_HOME}/bin/ww. Preserve user state and restore the recorded Host before retrying." >&2
  exit 1
fi

exec "$WW_HOME/bin/ww" uninstall "$@"
