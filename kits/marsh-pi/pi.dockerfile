# Pi Kit: the DHI Node.js runtime base plus the Kit VM floor (Docker Engine,
# which runs this Kit's jobs from this same image), the POSIX tools, the build
# toolchain a project's gate needs (make, gcc, python3) and ripgrep, assembled
# from Debian packages in a -dev build stage (image-base/assemble-packages.sh).
# npm runs only in the Node -dev stage.
FROM dhi.io/node:24-debian13@sha256:5eb921faefd07559999aad0571d9c26eccfc1c7aef8d386dc66481ed8f989cac AS base

FROM dhi.io/debian-base:trixie-dev@sha256:cc4e05ff415ee2d83918f2af45c737983209924ef7362c53f00cfcdaa1c94326 AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
# The notice index names each package's exact version: when it changes (a DHI
# package update), the cached package layer must not be reused.
COPY dhi-notices/installed-package-notices.json /tmp/marsh-notice-index.json
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm @userland @devtools ripgrep

# Pi, its MCP Gateway extension, and (for ACP mode, `pi --acp`) the community
# pi-acp adapter, all pinned by package-lock.json.
FROM dhi.io/node:24-debian13-dev@sha256:0fbf99f9c6874344b8c9a2bc80ea63c9f0058f01814f1ea23bf7ddad1b130a95 AS npm
ARG PI_VERSION
WORKDIR /opt/marsh/pi-gateway
COPY package.json package-lock.json ./
COPY patch-pi-dependencies.mjs ./
COPY collect-notices.mjs ./
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
COPY notices ./notices
RUN test "$PI_VERSION" = "$(node -p 'require("./package.json").dependencies["@earendil-works/pi-coding-agent"]')" \
 && npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
 && node patch-pi-dependencies.mjs \
 && test -f node_modules/pi-acp/dist/index.js \
 && node_modules/.bin/pi --version \
 && node collect-notices.mjs node_modules /usr/local/share/licenses/marsh-pi
COPY --chmod=0644 mcp-gateway.mjs ./mcp-gateway.mjs

FROM base AS image
COPY --from=packages /rootfs/ /
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
COPY --from=npm /opt/marsh/pi-gateway /opt/marsh/pi-gateway
COPY --from=npm /usr/local/share/licenses/marsh-pi /usr/local/share/licenses/marsh-pi
COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-pi
LABEL com.docker.sandboxes.start-docker=true
# The marsh Kit contract this image implements (marsh_contracts::kit_source):
# `marsh kit upgrade` moves a packaged command only to a supported one.
LABEL org.marsh.kit.contract=1
ENV IS_SANDBOX=1 PATH=/opt/marsh/pi-gateway/node_modules/.bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["/usr/local/bin/marsh-pi"]

# The runtime image has no Python: smoke-test it in a throwaway stage, then
# verify it, read-only, from the build stage. Scope is the image plus installed
# npm metadata/retained notices, not all npm code bytes.
FROM image AS smoke
USER root
RUN --mount=type=bind,source=image-base/smoke.sh,target=/tmp/smoke.sh sh /tmp/smoke.sh node rg make gcc python3 python pi

FROM packages AS verify
RUN sh /tmp/marsh-image-base/verifier.sh
RUN --mount=type=bind,from=smoke,source=/smoke.ok,target=/smoke.ok \
    --mount=type=bind,from=image,target=/image \
    python3 -I -S /image/usr/local/share/licenses/marsh-dhi/verify.py --root /image --adapter pi --receipt-dir /receipt

FROM image
COPY --from=verify /receipt/ /usr/local/share/licenses/marsh-dhi/
