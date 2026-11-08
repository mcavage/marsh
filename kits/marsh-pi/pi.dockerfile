# Pi Kit: the DHI Node.js runtime base plus the Kit VM floor (Docker Engine,
# which runs this Kit's jobs from this same image), the POSIX tools, the build
# toolchain a project's gate needs (make, gcc, python3), ripgrep, and
# poppler-utils (pdftoppm, pdftotext: the agent reads PDFs), assembled
# from Debian packages in a -dev build stage (image-base/assemble-packages.sh).
# npm runs only in the Node -dev stage.
FROM dhi.io/node:26-debian13@sha256:d505be133e87522ebee07e8e2693955817b4ad4787d65fb99946124cc1e8f0a4 AS base

FROM dhi.io/debian-base:trixie-dev@sha256:61025cea33d5910165ced8b33edd3f6b6bfe161e129174887936a1b9a1442b20 AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm @userland @devtools ripgrep poppler-utils

# Pi, its MCP Gateway and task-tool extensions, and (for ACP mode, `pi --acp`) the community
# pi-acp adapter, all pinned by package-lock.json.
FROM dhi.io/node:26-debian13-dev@sha256:6c7e2d41ac2cad140e61f765740708f5ab19ac3e734b3d56e029923cb11be4fd AS npm
ARG PI_VERSION
WORKDIR /opt/marsh/pi-gateway
COPY package.json package-lock.json ./
COPY patch-pi-acp.mjs ./
RUN test "$PI_VERSION" = "$(node -p 'require("./package.json").dependencies["@earendil-works/pi-coding-agent"]')" \
 && npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
 && node patch-pi-acp.mjs \
 && test -f node_modules/pi-acp/dist/index.js \
 && node_modules/.bin/pi --version
COPY --chmod=0644 mcp-gateway.mjs ./mcp-gateway.mjs
COPY --chmod=0644 marsh-task.mjs ./marsh-task.mjs

FROM base AS image
COPY --from=packages /rootfs/ /
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
COPY --from=npm /opt/marsh/pi-gateway /opt/marsh/pi-gateway
COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-pi
LABEL com.docker.sandboxes.start-docker=true
# The marsh Kit contract this image implements (marsh_contracts::kit_source):
# `marsh kit upgrade` moves a packaged command only to a supported one.
LABEL org.marsh.kit.contract=1
ENV IS_SANDBOX=1 PATH=/opt/marsh/pi-gateway/node_modules/.bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["/usr/local/bin/marsh-pi"]

# Exercise the installed runtime and commands before publishing.
FROM image AS smoke
USER root
RUN --mount=type=bind,source=image-base/smoke.sh,target=/tmp/smoke.sh sh /tmp/smoke.sh node rg make gcc python3 python pi \
 && pdftoppm -v 2>/dev/null && pdftotext -v 2>/dev/null

FROM image
RUN --mount=type=bind,from=smoke,source=/smoke.ok,target=/tmp/smoke.ok test -f /tmp/smoke.ok
