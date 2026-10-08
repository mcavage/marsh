# v2's sandbox.image, as content. The template carries the platform floor
# (bash, agent user, git, CA store) and a claude install; the build
# re-pins claude to the release this kit publishes, so the provide cannot
# claim a version the image does not ship.
#
# Anthropic's install.sh downloads this same binary and then runs
# `claude install` to wire the symlink. That second step is a Bun runtime,
# which aborts under QEMU when cross-building arm64, so the kit fetches
# the platform binary directly into the layout install.sh would have left.
FROM dhi.io/sbx-templates:claude-code-docker@sha256:13d7dbd08041284dbe5df67ec730a26f7b03dbd6fa994e143672979440426010
ARG CLAUDE_VERSION
ARG TARGETARCH
USER root
COPY image-repair /tmp/marsh-image-repair
RUN python3 -I -S /tmp/marsh-image-repair/repair.py \
 && rm -rf /tmp/marsh-image-repair
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
RUN python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --stage base --receipt
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
 && mkdir -p "${CLAUDE_VERSION}/${platform}" /home/agent/.local/share/claude/versions /home/agent/.local/bin \
 && curl -fsSL "https://downloads.claude.ai/claude-code-releases/${CLAUDE_VERSION}/${platform}/claude" \
      -o "${CLAUDE_VERSION}/${platform}/claude" \
 && sha256sum --check --strict /tmp/marsh-claude-selected.txt \
 && install -m 0755 "${CLAUDE_VERSION}/${platform}/claude" "/home/agent/.local/share/claude/versions/${CLAUDE_VERSION}" \
 && ln -sfn "/home/agent/.local/share/claude/versions/${CLAUDE_VERSION}" /home/agent/.local/bin/claude \
 && chown -R agent:agent /home/agent/.local \
 && node --version \
 && rm -rf /tmp/marsh-claude-download /tmp/marsh-claude-checksums.txt /tmp/marsh-claude-selected.txt
# ACP mode (`claude --acp`, packaging/agents.json claude-session): the official
# Claude ACP adapter, pinned by package-lock.json. Its Agent SDK carries the
# matching native Claude Code build, which the adapter drives through
# claude-cli.sh so the container, not bubblewrap, stays the sandbox.
WORKDIR /opt/marsh/claude-acp
COPY package.json package-lock.json ./
COPY collect-notices.mjs ./
COPY notices ./notices
RUN node -e 'if (Number(process.versions.node.split(".")[0]) < 22) process.exit(1)' \
 && npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
 && test -f node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js \
 && node collect-notices.mjs node_modules /usr/local/share/licenses/marsh-claude-acp
COPY --chmod=0644 mcp-gateway-proxy.mjs /opt/marsh/claude-acp/mcp-gateway-proxy.mjs
COPY --chmod=0755 claude-cli.sh /opt/marsh/claude-acp/claude-cli.sh
COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-claude
# One final scoped receipt: the selected native CLI version and exact allowed
# base symlink mutation, plus the adapter's selected native SDK payload/notices.
RUN python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --profile claude-kit --version "$CLAUDE_VERSION" --adapter claude-acp --receipt
# Provider legal texts are in the canonical marsh-dhi bundle above.
# v2's environment.variables, in the slot OCI already owns for static env.
ENV IS_SANDBOX=1
USER agent
WORKDIR /home/agent/workspace
# v2's sandbox.entrypoint.
ENTRYPOINT ["/usr/local/bin/marsh-claude"]
