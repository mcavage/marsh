#!/usr/bin/env bash
# clippy for the Linux-only code paths (pins), in the guest toolchain.
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
docker run --rm -v "$REPO":/src:ro -v e3-target:/target \
  -v "$HOME/.cargo/registry":/usr/local/cargo/registry -v "$HOME/.cargo/config.toml":/usr/local/cargo/config.toml:ro \
  -w /src -e CARGO_TARGET_DIR=/target-clippy \
  rust:1.99-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 \
  sh -c 'rustup component add clippy >/dev/null 2>&1; cargo clippy --locked -p marsh-services --all-targets --features dev-harness,test-breaks -- -D warnings 2>&1 | tail -30; cargo test --locked -p marsh-services --lib 2>&1 | grep "test result"'
