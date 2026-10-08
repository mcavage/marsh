#!/bin/sh
set -eu

root=$(mktemp -d "${TMPDIR:-/tmp}/marsh-kit-shell.XXXXXX")
trap 'rm -rf "$root"' EXIT HUP INT TERM
adapter=$(pwd)/kits/marsh-shell/marsh-entrypoint.sh
home=$root/home
mkdir -p "$home"

# A bare non-PTY invocation treats stdin as a POSIX shell program.
output=$(printf 'printf "stdin:%%s" "$HOME"' | \
  MARSH_SELECTED_HOME="$home" sh "$adapter")
test "$output" = "stdin:$home"

# Explicit shell argv retain their normal order and status behavior.
output=$(MARSH_SELECTED_HOME="$home" sh "$adapter" -c \
  'printf "%s:%s:%s" "$0" "$1" "$2"' command-name one 'two words')
test "$output" = 'command-name:one:two words'
if MARSH_SELECTED_HOME="$home" sh "$adapter" -c 'exit 37'; then
  echo 'shell adapter lost the requested exit status' >&2
  exit 1
else
  status=$?
fi
test "$status" -eq 37

# With a terminal and no argv, /bin/sh enters its interactive mode. Exercise
# the same adapter with a real PTY and verify byte output plus exit status.
MARSH_SELECTED_HOME="$home" python3 - "$adapter" <<'PY'
import errno
import os
import pty
import subprocess
import sys

master, slave = pty.openpty()
try:
    process = subprocess.Popen(
        ["/bin/sh", sys.argv[1]],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=os.environ,
        close_fds=True,
    )
finally:
    os.close(slave)

os.write(master, b"printf shell-pty-ready; exit 23\n")
captured = bytearray()
while True:
    try:
        chunk = os.read(master, 4096)
    except OSError as error:
        if error.errno == errno.EIO:
            break
        raise
    if not chunk:
        break
    captured.extend(chunk)
os.close(master)
status = process.wait(timeout=10)
if status != 23 or b"shell-pty-ready" not in captured:
    raise SystemExit(
        f"interactive shell adapter failed: status={status}, output={captured!r}"
    )
PY

if MARSH_SELECTED_HOME=relative sh "$adapter" -c true >/dev/null 2>&1; then
  echo 'relative selected home unexpectedly accepted by shell kit' >&2
  exit 1
fi

python3 - <<'PY'
import json

with open("packaging/commands.json", encoding="utf-8") as source:
    commands = json.load(source)
if commands.get("shell") != "kits/marsh-shell":
    raise SystemExit("packaged shell command mapping is missing")
PY
