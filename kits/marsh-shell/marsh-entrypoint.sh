#!/bin/sh
set -eu

selected=${MARSH_SELECTED_HOME:-${HOME:-/home/agent}}
case "$selected" in
  /*) ;;
  *) echo "shell kit: MARSH_SELECTED_HOME must be absolute" >&2; exit 125 ;;
esac
export HOME=$selected

# With no arguments, /bin/sh reads a nonterminal stdin as a script and becomes
# interactive when attached to a terminal. Caller arguments are passed through
# unchanged, so `shell -c ...` has ordinary POSIX sh behavior.
exec /bin/sh "$@"
