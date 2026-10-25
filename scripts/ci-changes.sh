#!/bin/sh
# Classify the files a pull request changes (one path per line on stdin) for
# .github/workflows/ci.yml. Prints two lines for $GITHUB_OUTPUT:
#
#   code=false  only documentation and repository metadata changed, so the
#               Linux build and tests have nothing to check. docs/model (TLA+)
#               and docs/upstream (patch queue) are code.
#   host=false  nothing the macOS job tests changed: crates, Cargo files, the
#               toolchain, vendor, the Makefile, tests, or this workflow.
#
# An empty list (a re-run, a force-push to the base) is treated as everything.
set -eu

files=$(cat)
if [ -z "$files" ]; then
  echo code=true
  echo host=true
  exit 0
fi

light='^(docs/|[A-Za-z_-]+\.md$|\.github/(renovate\.json5|renovate-global\.js|CODEOWNERS|PULL_REQUEST_TEMPLATE\.md|ISSUE_TEMPLATE/))'
heavy_docs='^docs/(model|upstream)/'
host='^(crates/|Cargo\.(toml|lock)$|rust-toolchain\.toml$|vendor/|Makefile$|tests/|\.github/workflows/ci\.yml$|scripts/ci-changes\.sh$)'

if printf '%s\n' "$files" | grep -Ev "$light" | grep -q . ||
   printf '%s\n' "$files" | grep -Eq "$heavy_docs"; then
  echo code=true
else
  echo code=false
fi

if printf '%s\n' "$files" | grep -Eq "$host"; then
  echo host=true
else
  echo host=false
fi
