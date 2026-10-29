# syntax=docker/dockerfile:1
# The one shell VM template: the DHI Debian base (its -dev variant: the shell
# user has sudo and apt for their own tools) with Docker Engine, the build
# tools `make dev` needs (gcc, make, git, python3, node, npm), zsh, Vim and
# terminal-only Emacs, the pinned Rust 1.99 toolchain with the aarch64 gnu and
# musl targets `make dev-inner`
# needs, and the Pi coding agent (`pi-dev`, locked by
# pi/package-lock.json). `marsh --dev` uses this same image; it only adds the
# dev broker. The caller supplies the exact reviewed upstream DHI digest. The
# exported reference names this image, never the upstream input. No agent
# image, host profile or credential is a build input.
ARG SHELL_BASE_IMAGE
FROM rust:1.99-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 AS rust
RUN rustup component add --toolchain 1.99.1 rustfmt clippy \
 && rustup target add --toolchain 1.99.1 aarch64-unknown-linux-musl aarch64-unknown-linux-gnu \
 && rustc --version && cargo fmt --version && cargo clippy --version

FROM ${SHELL_BASE_IMAGE} AS emacs
USER root
# Native compilation requires a GCC JIT package DHI does not supply; build
# terminal Emacs without it, preserving DHI's compiler and runtime libraries.
COPY image-base /tmp/marsh-image-base
RUN apt-get update \
 && sh /tmp/marsh-image-base/align-perl.sh \
 && apt-get install -y --no-install-recommends \
      gcc libc6-dev make autoconf automake dpkg-dev pkg-config texinfo \
      ca-certificates curl xz-utils \
      libncurses-dev libgnutls28-dev libxml2-dev libsqlite3-dev \
      libtree-sitter-dev libgmp-dev libacl1-dev zlib1g-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /build/emacs
COPY editors/patches ./patches
COPY editors/SHA256SUMS editors/REBUILD.md ./
COPY Dockerfile ./Dockerfile
RUN curl -fSL --retry 3 --max-time 120 -o emacs_30.1+1-6+deb13u1.dsc \
      'https://deb.debian.org/debian/pool/main/e/emacs/emacs_30.1%2b1-6%2bdeb13u1.dsc' \
 && curl -fSL --retry 3 --max-time 120 -o emacs_30.1+1.orig.tar.xz \
      'https://deb.debian.org/debian/pool/main/e/emacs/emacs_30.1%2b1.orig.tar.xz' \
 && curl -fSL --retry 3 --max-time 120 -o emacs_30.1+1-6+deb13u1.debian.tar.xz \
      'https://deb.debian.org/debian/pool/main/e/emacs/emacs_30.1%2b1-6%2bdeb13u1.debian.tar.xz' \
 && sha256sum -c SHA256SUMS \
 && dpkg-source --no-check -x emacs_30.1+1-6+deb13u1.dsc source \
 && cd source \
 && for patch in ../patches/*.patch; do \
      patch -p1 --fuzz=0 --batch --forward --reject-file=- -i "$patch" || exit 1; \
    done \
 && ./autogen.sh \
 && ./configure --prefix=/opt/emacs --without-x --without-native-compilation \
      --without-sound --with-gnutls --with-xml2 --with-sqlite3 \
      --with-tree-sitter --with-modules \
 && make -j8 && make install \
 && mkdir -p /opt/emacs/share/doc/emacs/source \
 && cp COPYING debian/copyright /opt/emacs/share/doc/emacs/ \
 && cp ../*.dsc ../*.tar.xz ../SHA256SUMS ../Dockerfile ../REBUILD.md \
      /opt/emacs/share/doc/emacs/source/ \
 && cp -r ../patches /opt/emacs/share/doc/emacs/source/

FROM ${SHELL_BASE_IMAGE} AS image
USER root
# The DHI-built Engine, Node.js and Python packages carry no Debian copyright
# file. Their versions and the Python bootstrap wheels are pinned here.
# Original upstream notices are retained with the image. zsh is for `marsh --shell zsh` (docs/shells.md). Recommended
# extras are not installed. Ordinary Debian tools are unpinned and retain their
# package copyright files; Emacs is installed from source above.
COPY image-base /tmp/marsh-image-base
RUN apt-get update \
 && sh /tmp/marsh-image-base/align-perl.sh \
 && apt-get install -y --no-install-recommends \
      docker-ce=5:29.9.2-1 docker-ce-cli=5:29.9.2-2 containerd.io=2.4.2-3 \
      docker-buildx-plugin=0.38.1-1 \
      nodejs-24=24.22.2-0 npm=11.21.1-1 \
      python-3.14=3.14.10-0 python3-pip=26.2.2-0 python3-pip-whl=26.2.2-0 \
      python3-setuptools-whl=84.2.1-0 python3-wheel=0.48.0-0 \
      sudo passwd ca-certificates curl git openssh-client less procps jq \
      gcc libc6-dev make zsh vim \
      libacl1 libattr1 libgmp10 libgnutls30t64 libsqlite3-0 libtinfo6 \
      libtree-sitter0.22 libxml2 zlib1g \
 && rm -rf /var/lib/apt/lists/* \
 && rm -rf /tmp/marsh-image-base \
 && /usr/bin/zsh -fc 'echo $ZSH_VERSION' \
 && test -x /usr/bin/bash
COPY --from=emacs /opt/emacs /opt/emacs
RUN mkdir -p /usr/local/bin && ln -s /opt/emacs/bin/emacs /usr/local/bin/emacs
# The sandbox account Docker Sandboxes expects (uid 1000 `agent`), in the
# docker and sudo groups, and the persistent environment file it writes at VM
# start, sourced by every shell (bash.bashrc and profile below) and named by
# BASH_ENV.
RUN { getent group docker >/dev/null || groupadd -r docker; } \
 && { getent group sudo >/dev/null || groupadd -g 27 sudo; } \
 && groupadd -g 1000 agent \
 && useradd -u 1000 -g 1000 -G docker,sudo -d /home/agent -m -s /bin/bash agent \
 && install -d -o agent -g agent /home/agent/workspace \
 && printf 'agent ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/agent \
 && chmod 0440 /etc/sudoers.d/agent && visudo -cf /etc/sudoers.d/agent \
 && touch /etc/sandbox-persistent.sh
COPY --chmod=0644 <<'EOF' /etc/bash.bashrc
if [ -f /etc/sandbox-persistent.sh ]; then
    . /etc/sandbox-persistent.sh
fi
export BASH_ENV=/etc/sandbox-persistent.sh
EOF
COPY --chmod=0644 <<'EOF' /etc/profile.d/sandbox-persistent.sh
if [ -f /etc/sandbox-persistent.sh ]; then
    . /etc/sandbox-persistent.sh
fi
export BASH_ENV=/etc/sandbox-persistent.sh
EOF
COPY --chmod=0644 <<'EOF' /etc/profile
if [ -d /etc/profile.d ]; then
    for _f in /etc/profile.d/*.sh; do
        [ -r "$_f" ] && . "$_f"
    done
    unset _f
fi
EOF
COPY image-repair /tmp/marsh-image-repair
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
RUN python3 -I -S /tmp/marsh-image-repair/repair.py \
 && rm -rf /tmp/marsh-image-repair \
 && command -v sudo && command -v visudo && command -v useradd && command -v getent \
 && command -v docker && command -v dockerd \
 && command -v gcc && command -v node && command -v npm && command -v make && command -v python3
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

USER agent
RUN HOME=/home/agent vim --version \
 && HOME=/home/agent emacs --batch --eval '(princ emacs-version)'
USER root

FROM image
WORKDIR /
LABEL com.docker.sandboxes.start-docker=true
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_TOOLCHAIN=1.99.1 \
    HOME=/home/agent \
    BASH_ENV=/etc/sandbox-persistent.sh \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
USER agent
