# syntax=docker/dockerfile:1
ARG RUST_IMAGE=rust:1.99-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8
FROM ${RUST_IMAGE} AS build

WORKDIR /source
ARG BUILD_CACHE_ID=marsh-default
# Release builds stay serial; `make dev` raises this for its persistent cache.
ARG CARGO_JOBS=1
# Extra cargo features for every build below (`make dev DEV_FEATURES=...`); empty for releases.
ARG CARGO_FEATURES=
# 1 for release packaging (CI): marsh-contracts refuses the test-breaks feature then.
ARG RELEASE_PACKAGING=
RUN rustup target add --toolchain 1.99.1 aarch64-unknown-linux-musl
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY vendor ./vendor
COPY scripts/third-party-notices.py ./scripts/third-party-notices.py
COPY scripts/verify-static-elf.py ./scripts/verify-static-elf.py
COPY scripts/provenance.py ./scripts/provenance.py
COPY packaging/notice-overrides ./packaging/notice-overrides
COPY LICENSE ./LICENSE
ENV CARGO_INCREMENTAL=0 CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_BUILD_JOBS=${CARGO_JOBS} MARSH_RELEASE_PACKAGING=${RELEASE_PACKAGING}
# The build records what cargo reports it compiled and the hash of each
# executable (scripts/provenance.py); `make dist` verifies it. "$@" carries the
# feature arguments.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=${BUILD_CACHE_ID},target=/source/target \
    set -eu; \
    if [ -n "${CARGO_FEATURES}" ]; then set -- --features "${CARGO_FEATURES}"; else set --; fi; \
    python3 scripts/provenance.py build --manifest /out-provenance.json --reset \
      --rename marsh=marsh-linux-arm64 --rename marsh-relay=marsh-relay-linux-arm64 --rename marshd=marshd-linux-arm64 -- \
      cargo build --locked --release "$@" \
      -p marsh --bin marsh \
      -p marsh-daemon --bin marsh-relay \
      -p marsh-backend --bin marshd; \
    cp /source/target/release/marsh /out-marsh; \
    cp /source/target/release/marsh-relay /out-relay; \
    cp /source/target/release/marshd /out-daemon; \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$(rustc --print sysroot)/lib/rustlib/aarch64-unknown-linux-gnu/bin/rust-lld" \
    RUSTFLAGS='-C target-feature=+crt-static' \
    python3 scripts/provenance.py build --manifest /out-provenance.json \
      --rename marsh-worker=marsh-worker-linux-arm64 --rename marsh-local=marsh-local-linux-arm64 --rename marsh-byte-exec=marsh-byte-exec-linux-arm64 -- \
      cargo build --locked --release --target aarch64-unknown-linux-musl "$@" \
      -p marsh-worker --bin marsh-worker \
      -p marsh-runtime --bin marsh-byte-exec \
      -p marsh --bin marsh-local; \
    cp /source/target/aarch64-unknown-linux-musl/release/marsh-worker /out-worker; \
    cp /source/target/aarch64-unknown-linux-musl/release/marsh-local /out-local-shell; \
    python3 scripts/verify-static-elf.py /out-local-shell --architecture arm64; \
    cp /source/target/aarch64-unknown-linux-musl/release/marsh-byte-exec /out-byte-exec; \
    python3 scripts/verify-static-elf.py /out-worker --architecture arm64; \
    python3 scripts/verify-static-elf.py /out-byte-exec --architecture arm64; \
    python3 scripts/third-party-notices.py --output /out-notices; \
    cp LICENSE /out-notices/LICENSE

FROM scratch AS export
COPY --from=build /out-marsh /marsh-linux-arm64
COPY --from=build /out-local-shell /marsh-local-linux-arm64
COPY --from=build /out-worker /marsh-worker-linux-arm64
COPY --from=build /out-relay /marsh-relay-linux-arm64
COPY --from=build /out-daemon /marshd-linux-arm64
COPY --from=build /out-byte-exec /marsh-byte-exec-linux-arm64
COPY --from=build /out-provenance.json /provenance.json
COPY --from=build /out-notices /licenses/marsh
