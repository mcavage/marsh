#!/bin/sh
# Classify changed paths for CI and per-commit image publication. Empty input
# runs every lane; manual CI dispatches also run everything.
set -eu
files=$(cat)
if [ -z "$files" ]; then
  printf '%s\n' code=true host=true tools=true model=true images=true renovate=true
  exit 0
fi

ci='^(\.github/workflows/ci\.yml|scripts/ci-changes\.sh)$'
# Kit and shell image inputs: CI builds the images, and a passing main run
# publishes them (release.yml).
images="^(kits/marsh-(claude|codex|pi|shell)/|packaging/(shell|shell-image|svc-engine/|commands\\.json|agents\\.json|image-base|dhi-notices|notice-overrides)|tests/acceptance/(fixture/|acp-fixture/(acp-fixture[.](dockerfile|yaml)|agent[.]mjs)$)|scripts/(publish-kits|prepare-shell-image|prepare-svc-engine-image|prepare-kit-inputs|build_inputs|image_observations|stage-kits|owned_process|stock_sdk)\\.py|scripts/requirements-publish\\.txt\$|Makefile\$)|$ci"
# Rust tests, real binary callers, and the shell differential harness, plus
# the files the Rust tests read: every Kit (marsh-contracts), the packaged
# command tables and the ACP fixture (tests/release/test_ci_changes.py
# derives that list from the sources). Image inputs select them too: a
# per-commit run moves a Kit's stable tag only when its CI run passed the Rust
# lanes on that commit.
code="^(crates/|Cargo\\.(toml|lock)\$|rust-toolchain\\.toml\$|vendor/|docs/upstream/|Makefile\$|\\.config/nextest\\.toml\$|tests/mcp/|kits/|packaging/(commands|drive-commands)\\.json\$|tests/acceptance/(onboarding\\.py\$|acp-fixture/|fixture/commands\\.json\$|bash_suites\\.py\$|bash_divergences\\.json\$))|$images"
host=$code
# Tooling tests exercise publisher, acceptance and release scripts without Rust.
tools="^(scripts/|tests/|evals/|kits/|packaging/|Makefile\$|\\.github/)|$code"
# Markdown in docs/model is documentation, not an input to the model checker.
model="^(docs/model/.*\\.(tla|cfg)\$|docs/model/check\\.sh\$|Makefile\$|\\.github/workflows/model\\.yml\$)|$ci"
renovate="^(\\.github/renovate|tests/release/renovate-policy\\.mjs\$)|$ci"

has() { printf '%s\n' "$files" | grep -Eq "$1"; }
for lane in code host tools model images renovate; do
  eval "pattern=\$$lane"
  if has "$pattern"; then echo "$lane=true"; else echo "$lane=false"; fi
done
