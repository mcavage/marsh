# syntax=docker/dockerfile:1
# The one shell VM template: the repaired DHI shell (gcc, make, git, python3,
# node, npm, docker) plus the pinned Rust 1.95 toolchain with the aarch64 gnu
# and musl targets `make dev-inner` needs, plus the Pi coding agent 1.0.0
# (`pi-dev`, locked by pi/package-lock.json). `marsh --dev` uses this same image;
# it only adds the dev broker. The caller supplies the exact reviewed upstream
# DHI digest. The exported reference names this repaired image, never the
# upstream input. No agent image, host profile or credential is a build input.
ARG SHELL_BASE_IMAGE
FROM rust:1.95-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS rust
RUN rustup component add --toolchain 1.95.0 rustfmt clippy \
 && rustup target add --toolchain 1.95.0 aarch64-unknown-linux-musl aarch64-unknown-linux-gnu \
 && rustc --version && cargo fmt --version && cargo clippy --version

FROM ${SHELL_BASE_IMAGE}
USER root
COPY image-repair /tmp/marsh-image-repair
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
RUN python3 -I -S /tmp/marsh-image-repair/repair.py \
 && python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --receipt \
 && rm -rf /tmp/marsh-image-repair \
 && command -v sudo \
 && command -v docker \
 && command -v dockerd \
 && command -v gcc && command -v node && command -v npm && command -v make
# The stock template prints an ASCII banner from /etc/profile.d/01-dhi.sh on
# interactive shells (also via /etc/bash.bashrc). marsh is quiet by default:
# the banner shows only when MARSH_BANNER=1 is set in the shell VM.
RUN test -f /etc/profile.d/01-dhi.sh \
 && sed -i '1a [ "${MARSH_BANNER:-0}" = 1 ] || { return 0 2>/dev/null || exit 0; }' /etc/profile.d/01-dhi.sh \
 && sh -c '. /etc/profile.d/01-dhi.sh' | wc -c | grep -qx 0
# zsh for `marsh --shell zsh` (docs/shells.md), from the base image's Debian
# trixie apt sources; recommended extras are not installed.
RUN apt-get update \
 && apt-get install -y --no-install-recommends zsh \
 && rm -rf /var/lib/apt/lists/* \
 && /usr/bin/zsh -fc 'echo $ZSH_VERSION' \
 && test -x /usr/bin/bash
COPY --from=rust /usr/local/cargo/ /usr/local/cargo/
COPY --from=rust /usr/local/rustup/ /usr/local/rustup/
# Any shell UID (the Mac user's) may use the shared registry cache.
RUN chmod -R a+rwX /usr/local/cargo /usr/local/rustup \
 && RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo \
    /usr/local/cargo/bin/cargo --version
WORKDIR /opt/marsh/pi
COPY pi/package.json pi/package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
 && test "$(node_modules/.bin/pi --version)" = "$(node -p 'require("./package.json").dependencies["@earendil-works/pi-coding-agent"]')"
COPY --chmod=0755 pi/pi-dev /usr/local/bin/pi-dev
WORKDIR /
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_TOOLCHAIN=1.95.0 \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
USER agent
