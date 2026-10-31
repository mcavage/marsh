# Pi Kit: the DHI Node.js runtime base plus the Kit VM floor (Docker Engine,
# which runs this Kit's jobs from this same image), the POSIX tools, the build
# toolchain a project's gate needs (make, gcc, python3) and ripgrep, assembled
# from Debian packages in a -dev build stage (image-base/assemble-packages.sh).
# npm runs only in the Node -dev stage.
FROM dhi.io/node:26-debian13@sha256:38c8bbf0ccffd6603b32f602c08b4f407a2a83b010f7371854deaa6e0c2576c4 AS base

FROM dhi.io/debian-base:trixie-dev@sha256:7995f6739e5ad75cbd57d9356f7d7c83d43dd957682b6a000f32ff95b441a9b6 AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm @userland @devtools ripgrep

# Pi, its MCP Gateway extension, and (for ACP mode, `pi --acp`) the community
# pi-acp adapter, all pinned by package-lock.json.
FROM dhi.io/node:26-debian13-dev@sha256:6d1c8db117086399179ccc021bb73dd11c027760d5625befe75e2ed5f8105372 AS npm
ARG PI_VERSION
WORKDIR /opt/marsh/pi-gateway
COPY package.json package-lock.json ./
COPY patch-pi-dependencies.mjs ./
RUN test "$PI_VERSION" = "$(node -p 'require("./package.json").dependencies["@earendil-works/pi-coding-agent"]')" \
 && npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
 && node patch-pi-dependencies.mjs \
 && test -f node_modules/pi-acp/dist/index.js \
 && node_modules/.bin/pi --version
COPY --chmod=0644 mcp-gateway.mjs ./mcp-gateway.mjs

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
RUN --mount=type=bind,source=image-base/smoke.sh,target=/tmp/smoke.sh sh /tmp/smoke.sh node rg make gcc python3 python pi

FROM image
RUN --mount=type=bind,from=smoke,source=/smoke.ok,target=/tmp/smoke.ok test -f /tmp/smoke.ok
