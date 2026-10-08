# v2's sandbox.image, as content. The template carries the platform floor
# and a codex install; the build re-pins codex to the release this kit
# publishes, so the provide cannot claim a version the image does not ship.
# codex-code-mode-host rides along: codex spawns it as a sibling executable
# for Code Mode and fails that feature closed when it is missing.
FROM dhi.io/debian-base:trixie-dev@sha256:f3bd649e61fc5357c3b2f207a0b047961d5562f0dc9905b85adbf4fa19580292 AS build
ARG CODEX_VERSION
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates
COPY release-checksums.txt /tmp/marsh-codex-checksums.txt
WORKDIR /tmp/marsh-codex-download
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && mkdir -p /out/usr/local/bin "${CODEX_VERSION}" \
 && for bin in codex codex-code-mode-host; do \
      archive="${CODEX_VERSION}/${bin}-${target}.tar.gz"; \
      awk -v file="$archive" \
        '$2 == file { print; count++ } END { if (count != 1) exit 1 }' \
        /tmp/marsh-codex-checksums.txt > /tmp/marsh-codex-selected.txt \
      && curl -fsSL "https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/${bin}-${target}.tar.gz" \
        -o "$archive" \
      && sha256sum --check --strict /tmp/marsh-codex-selected.txt || exit 1; \
    done \
 && for bin in codex codex-code-mode-host; do \
      tar -xOzf "${CODEX_VERSION}/${bin}-${target}.tar.gz" -- "${bin}-${target}" \
        > "/out/usr/local/bin/${bin}" \
      && chmod 0755 "/out/usr/local/bin/${bin}" || exit 1; \
    done \
 && /out/usr/local/bin/codex --version

FROM dhi.io/sbx-templates:codex-docker@sha256:0529fcace182dc3de0967bdb235ed3f8ca715f8eafc9f987a53a258ae3bf83f3
ARG CODEX_VERSION
USER root
COPY image-repair /tmp/marsh-image-repair
RUN python3 -I -S /tmp/marsh-image-repair/repair.py \
 && rm -rf /tmp/marsh-image-repair
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
RUN python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --stage base --receipt
COPY --from=build /out/usr/local/bin/ /usr/local/bin/
# ACP mode (`codex --acp`, packaging/agents.json codex-session): the official
# codex-acp adapter, pinned by package-lock.json, with the Codex build it
# depends on kept inside its own node_modules (never on PATH).
WORKDIR /opt/marsh/codex-acp
COPY package.json package-lock.json ./
COPY collect-notices.mjs ./
COPY notices ./notices
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
 && test -f node_modules/@agentclientprotocol/codex-acp/dist/index.js \
 && node_modules/.bin/codex --version \
 && node collect-notices.mjs node_modules /usr/local/share/licenses/marsh-codex-acp
COPY --chmod=0644 mcp-gateway-proxy.mjs /opt/marsh/codex-acp/mcp-gateway-proxy.mjs
COPY --chmod=0755 acp-entrypoint.sh /opt/marsh/codex-acp/marsh-acp.sh
COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-codex
# Public command lookup and the absolute entrypoint selection must agree.
ENV PATH=/usr/local/bin:$PATH
# This executes only the pinned CLI version query during the root-owned build.
RUN test "$(command -v codex)" = /usr/local/bin/codex \
 && test "$(codex --version)" = "codex-cli ${CODEX_VERSION}" \
 && node --version
# Provider legal texts are in the canonical marsh-dhi bundle above.
# Final receipt binds the retained npm 0.159.2, the selected native overlay,
# and the ACP adapter's selected native npm payload/notices.
RUN python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --profile codex-kit --version "$CODEX_VERSION" --adapter codex-acp --receipt
# v2's environment.variables, in the slot OCI already owns for static env.
ENV IS_SANDBOX=1 BROWSER=xdg-open CODEX_HOME=/home/agent/.codex GIT_TERMINAL_PROMPT=0
USER agent
WORKDIR /home/agent/workspace
# v2's sandbox.entrypoint.
ENTRYPOINT ["/usr/local/bin/marsh-codex"]
