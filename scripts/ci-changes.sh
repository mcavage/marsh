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
# Rust tests, real binary callers, and the shell differential harness.
code="^(crates/|Cargo\\.(toml|lock)\$|rust-toolchain\\.toml\$|vendor/|docs/upstream/|Makefile\$|\\.config/nextest\\.toml\$|tests/mcp/|tests/acceptance/(onboarding\\.py|acp-fixture/typed_host_caller\\.py|bash_suites\\.py|bash_divergences\\.json)\$)|$ci"
host=$code
# Tooling tests exercise publisher, acceptance and release scripts without Rust.
tools="^(scripts/|tests/|evals/|kits/|packaging/|Makefile\$|\\.github/)|$code"
# Markdown in docs/model is documentation, not an input to the model checker.
model="^(docs/model/.*\\.(tla|cfg)\$|docs/model/check\\.sh\$|Makefile\$|\\.github/workflows/model\\.yml\$)|$ci"
images="^(kits/marsh-(claude|codex|pi|shell)/|packaging/(shell|shell-image|svc-engine/|commands\\.json|agents\\.json|image-base|dhi-notices|notice-overrides)|tests/acceptance/(fixture/|acp-fixture/(acp-fixture[.](dockerfile|yaml)|agent[.]mjs)$)|scripts/(publish-kits|prepare-shell-image|prepare-svc-engine-image|prepare-kit-inputs|build_inputs|image_observations|stage-kits|owned_process|stock_sdk)\\.py|scripts/requirements-publish\\.txt\$|Makefile\$)|$ci"
renovate="^(\\.github/renovate|tests/release/renovate-policy\\.mjs\$)|$ci"

has() { printf '%s\n' "$files" | grep -Eq "$1"; }
for lane in code host tools model images renovate; do
  eval "pattern=\$$lane"
  if has "$pattern"; then echo "$lane=true"; else echo "$lane=false"; fi
done
