#!/usr/bin/env bash
# The in-process witnesses (e3/witness.sh) in the guest toolchain, where the
# Linux-only tests (POLLHUP semantics, pins) run.
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
docker run --rm -v "$REPO":/src:ro -v e3-target:/target \
  -v "$HOME/.cargo/registry":/usr/local/cargo/registry -v "$HOME/.cargo/config.toml":/usr/local/cargo/config.toml:ro \
  -w /src -e CARGO_TARGET_DIR=/target-test -e CARGO_NET_OFFLINE=true \
  rust:1.99-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 \
  bash crates/marsh-services/e3/witness.sh
