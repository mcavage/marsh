# v2's sandbox.image, as content: the DHI Node.js runtime base plus the Kit VM
# floor (Docker Engine, which runs this Kit's jobs from this same image), the
# POSIX tools, the build toolchain a project's gate needs (make, gcc, python3)
# and ripgrep, assembled from Debian packages in a -dev build stage
# (image-base/assemble-packages.sh), plus the Claude Code release this Kit
# publishes, so the provide cannot claim a version the image does not ship.
#
# Anthropic's install.sh downloads this same binary and then runs
# `claude install` to wire the symlink. That second step is a Bun runtime,
# which aborts under QEMU when cross-building arm64, so the kit fetches
# the platform binary directly into the layout install.sh would have left.
FROM dhi.io/node:24-debian13@sha256:32a2fa6af5209698a9354cef0b19ada3dbfac3090b1010b70afa9f1a298ba2d9 AS base

FROM dhi.io/debian-base:trixie-dev@sha256:9b025768946856e44f9775005eb199f2e1820484ea27bb5c8bcbb187b80464dc AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm @userland @devtools ripgrep

FROM dhi.io/debian-base:trixie-dev@sha256:9b025768946856e44f9775005eb199f2e1820484ea27bb5c8bcbb187b80464dc AS download
ARG CLAUDE_VERSION
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates
COPY release-checksums.txt /tmp/marsh-claude-checksums.txt
WORKDIR /tmp/marsh-claude-download
RUN case "$TARGETARCH" in \
      amd64) platform=linux-x64 ;; \
      arm64) platform=linux-arm64 ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && awk -v file="${CLAUDE_VERSION}/${platform}/claude" \
      '$2 == file { print; count++ } END { if (count != 1) exit 1 }' \
      /tmp/marsh-claude-checksums.txt > /tmp/marsh-claude-selected.txt \
 && mkdir -p "${CLAUDE_VERSION}/${platform}" /out/home/agent/.local/share/claude/versions /out/home/agent/.local/bin \
 && curl -fsSL "https://downloads.claude.ai/claude-code-releases/${CLAUDE_VERSION}/${platform}/claude" \
      -o "${CLAUDE_VERSION}/${platform}/claude" \
 && sha256sum --check --strict /tmp/marsh-claude-selected.txt \
 && install -m 0755 "${CLAUDE_VERSION}/${platform}/claude" "/out/home/agent/.local/share/claude/versions/${CLAUDE_VERSION}" \
 && ln -sfn "/home/agent/.local/share/claude/versions/${CLAUDE_VERSION}" /out/home/agent/.local/bin/claude

# ACP mode (`claude --acp`, packaging/agents.json claude-session): the official
# Claude ACP adapter, pinned by package-lock.json. Its Agent SDK carries the
# matching native Claude Code build, which the adapter drives through
# claude-cli.sh so the container, not bubblewrap, stays the sandbox.
FROM dhi.io/node:24-debian13-dev@sha256:20cf36e12660c68b1e28887d1e71a7f2494bcea9d034709368e0afce77728eb0 AS npm
WORKDIR /opt/marsh/claude-acp
COPY package.json package-lock.json ./
COPY collect-notices.mjs ./
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
COPY notices ./notices
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
 && test -f node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js \
 && node collect-notices.mjs node_modules /usr/local/share/licenses/marsh-claude-acp
COPY --chmod=0644 mcp-gateway-proxy.mjs /opt/marsh/claude-acp/mcp-gateway-proxy.mjs
COPY --chmod=0755 claude-cli.sh /opt/marsh/claude-acp/claude-cli.sh

FROM base AS image
COPY --from=packages /rootfs/ /
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
COPY --from=download --chown=1000:1000 /out/home/agent/.local /home/agent/.local
COPY --from=npm /opt/marsh/claude-acp /opt/marsh/claude-acp
COPY --from=npm /usr/local/share/licenses/marsh-claude-acp /usr/local/share/licenses/marsh-claude-acp
COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-claude
LABEL com.docker.sandboxes.start-docker=true
# The marsh Kit contract this image implements (marsh_contracts::kit_source):
# `marsh kit upgrade` moves a packaged command only to a supported one.
LABEL org.marsh.kit.contract=1
# v2's environment.variables, in the slot OCI already owns for static env.
# The executable is a reviewed Kit input, not a per-container installation.
ENV IS_SANDBOX=1 DISABLE_AUTOUPDATER=1 DISABLE_UPDATES=1 \
    PATH=/home/agent/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
USER agent
WORKDIR /home/agent/workspace
# v2's sandbox.entrypoint.
ENTRYPOINT ["/usr/local/bin/marsh-claude"]

# The runtime image has no Python: smoke-test it in a throwaway stage, then
# verify it, read-only, from the build stage. One final scoped receipt: the selected native CLI version, plus the
# adapter's selected native SDK payload/notices. Provider legal texts are in
# the canonical marsh-dhi bundle.
FROM image AS smoke
USER root
RUN --mount=type=bind,source=image-base/smoke.sh,target=/tmp/smoke.sh sh /tmp/smoke.sh node rg make gcc python3 python

FROM packages AS verify
ARG CLAUDE_VERSION
RUN sh /tmp/marsh-image-base/verifier.sh
RUN --mount=type=bind,from=smoke,source=/smoke.ok,target=/smoke.ok \
    --mount=type=bind,from=image,target=/image \
    python3 -I -S /image/usr/local/share/licenses/marsh-dhi/verify.py --root /image \
      --profile claude-kit --version "$CLAUDE_VERSION" --adapter claude-acp --receipt-dir /receipt

FROM image
COPY --from=verify /receipt/ /usr/local/share/licenses/marsh-dhi/
