#!/bin/sh
# Fail if git tracks a dependency or build directory. Fixtures install what
# they need on demand (see tests/acceptance/services/fixtures/stage.py), so a
# committed node_modules only bloats history and trips secret scanning on the
# test keys inside third-party packages.
set -eu

bad=$(git ls-files | grep -E '(^|/)(node_modules|\.venv|__pycache__|\.next|site-packages|target)/' || true)
if [ -n "$bad" ]; then
	count=$(printf '%s\n' "$bad" | wc -l | tr -d ' ')
	{
		echo "check-no-vendored-deps: $count tracked path(s) are inside a dependency or build directory"
		echo "(node_modules, .venv, __pycache__, .next, site-packages, target). Do not commit them:"
		printf '%s\n' "$bad" | sed -n '1,10p;11s/.*/  .../p' | sed '1,10s/^/  /'
		echo "Untrack with: git rm -r --cached <dir>; add it to .gitignore; install it on demand"
		echo "(npm ci, pip install) from the step that needs it."
	} >&2
	exit 1
fi
echo "check-no-vendored-deps: ok"
