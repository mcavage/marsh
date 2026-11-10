# v2's sandbox.image, as content: the DHI Node.js runtime base plus the Kit VM
# floor (Docker Engine, which runs this Kit's jobs from this same image), the
# POSIX tools, the build toolchain a project's gate needs (make, gcc, python3),
# ripgrep, and poppler-utils (pdftoppm, pdftotext: the agent reads PDFs),
# assembled from Debian packages in a -dev build stage
# (image-base/assemble-packages.sh), plus the Codex release this Kit
# publishes, so the provide cannot claim a version the image does not ship.
# codex-code-mode-host rides along: codex spawns it as a sibling executable
# for Code Mode and fails that feature closed when it is missing.
FROM dhi.io/node:26-debian13@sha256:d505be133e87522ebee07e8e2693955817b4ad4787d65fb99946124cc1e8f0a4 AS base

FROM dhi.io/debian-base:trixie-dev@sha256:94571f3dc07fbc7166f6f1cbf1fe068a7647198ee2554e6b90d59a68cc8b053c AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm @userland @devtools ripgrep poppler-utils

FROM dhi.io/debian-base:trixie-dev@sha256:94571f3dc07fbc7166f6f1cbf1fe068a7647198ee2554e6b90d59a68cc8b053c AS build
ARG CODEX_VERSION
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates
WORKDIR /tmp/marsh-codex-download
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && mkdir -p /out/usr/local/bin "${CODEX_VERSION}" \
 && for bin in codex codex-code-mode-host; do \
      archive="${CODEX_VERSION}/${bin}-${target}.tar.gz"; \
      curl -fsSL "https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/${bin}-${target}.tar.gz" \
        -o "$archive" || exit 1; \
    done \
 && for bin in codex codex-code-mode-host; do \
      tar -xOzf "${CODEX_VERSION}/${bin}-${target}.tar.gz" -- "${bin}-${target}" \
        > "/out/usr/local/bin/${bin}" \
      && chmod 0755 "/out/usr/local/bin/${bin}" || exit 1; \
    done \
 && /out/usr/local/bin/codex --version \
 && mkdir -p /out/usr/local/share/licenses/codex \
 && for file in LICENSE NOTICE; do \
      curl -fsSL "https://raw.githubusercontent.com/openai/codex/rust-v${CODEX_VERSION}/$file" \
        -o "/out/usr/local/share/licenses/codex/$file" || exit 1; \
    done

# ACP mode (`--acp`, the Kit's acp@1 adapter): the official
# codex-acp adapter, pinned by package-lock.json, with the Codex build it
# depends on kept inside its own node_modules (never on PATH).
FROM dhi.io/node:26-debian13-dev@sha256:3aa5efd22383b1e1c5969ed5c1876cd524a2c62a51982f06d1e8fa0a4d1d71b7 AS npm
WORKDIR /opt/marsh/codex-acp
COPY package.json package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
 && test -f node_modules/@agentclientprotocol/codex-acp/dist/index.js \
 && node_modules/.bin/codex --version
COPY --chmod=0644 mcp-gateway-proxy.mjs /opt/marsh/codex-acp/mcp-gateway-proxy.mjs
COPY --chmod=0755 acp-entrypoint.sh /opt/marsh/codex-acp/marsh-acp.sh

# Docker Compose plugin, pinned with a SHA-256 (image-base/docker-compose.sh).
# The image already has the docker CLI (docker-ce-cli from kit-vm.packages).
FROM dhi.io/debian-base:trixie-dev@sha256:94571f3dc07fbc7166f6f1cbf1fe068a7647198ee2554e6b90d59a68cc8b053c AS compose
ARG TARGETARCH
COPY image-base/docker-compose.sh /tmp/docker-compose.sh
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
 && sh /tmp/docker-compose.sh /out

FROM base AS image
COPY --from=packages /rootfs/ /
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
COPY --from=compose /out/ /usr/local/
COPY --from=build /out/usr/local/bin/ /usr/local/bin/
COPY --from=build /out/usr/local/share/licenses/ /usr/local/share/licenses/
COPY --from=npm /opt/marsh/codex-acp /opt/marsh/codex-acp
COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-codex
LABEL com.docker.sandboxes.start-docker=true
# The marsh Kit contract this image implements (marsh_contracts::kit_source):
# `marsh kit upgrade` moves a packaged command only to a supported one.
LABEL org.marsh.kit.contract=1
# v2's environment.variables, in the slot OCI already owns for static env.
# Public command lookup and the absolute entrypoint selection must agree.
ENV IS_SANDBOX=1 CODEX_HOME=/home/agent/.codex GIT_TERMINAL_PROMPT=0 \
    PATH=/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin
USER agent
WORKDIR /home/agent/workspace
# v2's sandbox.entrypoint.
ENTRYPOINT ["/usr/local/bin/marsh-codex"]

# Exercise the installed runtime and commands before publishing.
FROM image AS smoke
ARG CODEX_VERSION
USER root
RUN --mount=type=bind,source=image-base/smoke.sh,target=/tmp/smoke.sh sh /tmp/smoke.sh node rg make gcc python3 python \
 && docker --version && docker compose version \
 && pdftoppm -v 2>/dev/null && pdftotext -v 2>/dev/null \
 && test "$(command -v codex)" = /usr/local/bin/codex \
 && test "$(codex --version 2>/dev/null)" = "codex-cli ${CODEX_VERSION}"

FROM image
RUN --mount=type=bind,from=smoke,source=/smoke.ok,target=/tmp/smoke.ok test -f /tmp/smoke.ok
