#!/bin/sh
# Classify the files a change touches (one path per line on stdin) for
# .github/workflows/ci.yml. Prints one line per lane for $GITHUB_OUTPUT:
#
#   code=false      only documentation and repository metadata changed, so the
#                   Linux build and tests have nothing to check. docs/model
#                   (TLA+) and docs/upstream (patch queue) are code.
#   host=false      nothing the macOS jobs test changed: crates, Cargo files,
#                   the toolchain, vendor, the Makefile, tests, or CI itself.
#   model=false     no TLA+ input changed: the specs and configs, their runner,
#                   the Makefile (the pinned tools) or CI itself.
#   images=false    no input of the shell template or the Kit images changed.
#   renovate=false  the Renovate configuration and its policy test are unchanged.
#
# An empty list (a re-run, a force-push to the base) is treated as everything.
set -eu

files=$(cat)
if [ -z "$files" ]; then
  printf '%s\n' code=true host=true model=true images=true renovate=true
  exit 0
fi

light='^(docs/|[A-Za-z_-]+\.md$|\.github/(renovate\.json5|renovate-global\.js|CODEOWNERS|PULL_REQUEST_TEMPLATE\.md|ISSUE_TEMPLATE/))'
heavy_docs='^docs/(model|upstream)/'
ci='^(\.github/workflows/ci\.yml|scripts/ci-changes\.sh)$'
host="^(crates/|Cargo\\.(toml|lock)\$|rust-toolchain\\.toml\$|vendor/|Makefile\$|tests/|\\.config/nextest\\.toml\$)|$ci"
model="^(docs/model/|Makefile\$)|$ci"
images="^(kits/marsh-(claude|codex|pi|shell)/|packaging/(shell|shell-image|commands\\.json|agents\\.json|image-base|image-repair|dhi-notices|notice-overrides)|tests/acceptance/[a-z-]*fixture/|scripts/(publish-kits|prepare-shell-image|prepare-kit-inputs|build_inputs|image_observations|stage-kits|owned_process|stock_sdk)\\.py|scripts/requirements-publish\\.txt)|$ci"
renovate="^(\\.github/renovate|tests/release/renovate-policy\\.mjs\$)|$ci"

has() { printf '%s\n' "$files" | grep -Eq "$1"; }

if printf '%s\n' "$files" | grep -Ev "$light" | grep -q . || has "$heavy_docs"; then
  echo code=true
else
  echo code=false
fi
for lane in host model images renovate; do
  eval "pattern=\$$lane"
  if has "$pattern"; then echo "$lane=true"; else echo "$lane=false"; fi
done
