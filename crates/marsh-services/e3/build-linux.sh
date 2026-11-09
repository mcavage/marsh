#!/usr/bin/env bash
# Build the E3 dev harness for linux/arm64 (static musl) in a container.
# Output: $OUT/e3-harness (default /tmp/e3-out).
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
OUT=${OUT:-/private/var/folders/zr/1nbpv_812d17xnm1kdwwvq1m0000gn/T/opencode/e3-out}
FEATURES=${FEATURES:-dev-harness,test-breaks}
mkdir -p "$OUT"
docker volume create e3-target >/dev/null
docker run --rm \
  -v "$REPO":/src:ro -v e3-target:/target -v "$OUT":/out \
  -v "$HOME/.cargo/registry":/usr/local/cargo/registry \
  -v "$HOME/.cargo/config.toml":/usr/local/cargo/config.toml:ro \
  -w /src -e CARGO_TARGET_DIR=/target -e CARGO_INCREMENTAL=0 \
  rust:1.99-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 \
  sh -c '
    rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$(rustc --print sysroot)/lib/rustlib/aarch64-unknown-linux-gnu/bin/rust-lld" \
    RUSTFLAGS="-C target-feature=+crt-static" \
    cargo build --release --locked --target aarch64-unknown-linux-musl -p marsh-services --features '"$FEATURES"' --bin e3-harness 2>&1 | tail -20
    cp /target/aarch64-unknown-linux-musl/release/e3-harness /out/e3-harness
  '
ls -la "$OUT/e3-harness"
