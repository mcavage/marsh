FROM dhi.io/sbx-templates:claude-code-docker@sha256:13d7dbd08041284dbe5df67ec730a26f7b03dbd6fa994e143672979440426010
ARG PI_VERSION
USER root
COPY image-repair /tmp/marsh-image-repair
RUN python3 -I -S /tmp/marsh-image-repair/repair.py \
 && rm -rf /tmp/marsh-image-repair
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
RUN python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --stage base --receipt
# Pi, its MCP Gateway extension, and (for ACP mode, `pi --acp`) the community
# pi-acp adapter, all pinned by package-lock.json.
WORKDIR /opt/marsh/pi-gateway
COPY package.json package-lock.json ./
COPY patch-pi-dependencies.mjs ./
COPY collect-notices.mjs ./
COPY notices ./notices
RUN test "$PI_VERSION" = "$(node -p 'require("./package.json").dependencies["@earendil-works/pi-coding-agent"]')" \
 && npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
 && node patch-pi-dependencies.mjs \
 && test -f node_modules/pi-acp/dist/index.js \
 && node_modules/.bin/pi --version \
 && node collect-notices.mjs node_modules /usr/local/share/licenses/marsh-pi
COPY --chmod=0644 mcp-gateway.mjs ./mcp-gateway.mjs
COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-pi
ENV IS_SANDBOX=1 PATH=/opt/marsh/pi-gateway/node_modules/.bin:$PATH
# Scope is base plus installed npm metadata/retained notices, not all npm code bytes.
RUN python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --adapter pi --stage final --receipt
USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["/usr/local/bin/marsh-pi"]
