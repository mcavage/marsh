# v2's sandbox.image, as content: the DHI Node.js runtime base plus the Kit VM
# floor (Docker Engine, which runs this Kit's jobs from this same image), the
# POSIX tools, the build toolchain a project's gate needs (make, gcc, python3)
# and ripgrep, assembled from Debian packages in a -dev build stage
# (image-base/assemble-packages.sh), plus the Codex release this Kit
# publishes, so the provide cannot claim a version the image does not ship.
# codex-code-mode-host rides along: codex spawns it as a sibling executable
# for Code Mode and fails that feature closed when it is missing.
FROM dhi.io/node:24-debian13@sha256:f3575cf1e68ffe163703b691c465af833fd2d94cccc1e825d052c943018f2aa3 AS base

FROM dhi.io/debian-base:trixie-dev@sha256:04a23b0daf5f06a4985703cce7942ea96be5448946f3e2ea2ef15ff9db0ae1e9 AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm @userland @devtools ripgrep

FROM dhi.io/debian-base:trixie-dev@sha256:04a23b0daf5f06a4985703cce7942ea96be5448946f3e2ea2ef15ff9db0ae1e9 AS build
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

# ACP mode (`codex --acp`, packaging/agents.json codex-session): the official
# codex-acp adapter, pinned by package-lock.json, with the Codex build it
# depends on kept inside its own node_modules (never on PATH).
FROM dhi.io/node:24-debian13-dev@sha256:81ee43ecd5e1741d4fbb1d66f9637e393c02b1ab0a056b10dbac400cbed2f2bc AS npm
WORKDIR /opt/marsh/codex-acp
COPY package.json package-lock.json ./
COPY collect-notices.mjs ./
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
COPY notices ./notices
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
 && test -f node_modules/@agentclientprotocol/codex-acp/dist/index.js \
 && node_modules/.bin/codex --version \
 && node collect-notices.mjs node_modules /usr/local/share/licenses/marsh-codex-acp
COPY --chmod=0644 mcp-gateway-proxy.mjs /opt/marsh/codex-acp/mcp-gateway-proxy.mjs
COPY --chmod=0755 acp-entrypoint.sh /opt/marsh/codex-acp/marsh-acp.sh

FROM base AS image
COPY --from=packages /rootfs/ /
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
COPY --from=build /out/usr/local/bin/ /usr/local/bin/
COPY --from=npm /opt/marsh/codex-acp /opt/marsh/codex-acp
COPY --from=npm /usr/local/share/licenses/marsh-codex-acp /usr/local/share/licenses/marsh-codex-acp
COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-codex
LABEL com.docker.sandboxes.start-docker=true
# v2's environment.variables, in the slot OCI already owns for static env.
# Public command lookup and the absolute entrypoint selection must agree.
ENV IS_SANDBOX=1 CODEX_HOME=/home/agent/.codex GIT_TERMINAL_PROMPT=0 \
    PATH=/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin
USER agent
WORKDIR /home/agent/workspace
# v2's sandbox.entrypoint.
ENTRYPOINT ["/usr/local/bin/marsh-codex"]

# The runtime image has no Python: smoke-test it in a throwaway stage, then
# verify it, read-only, from the build stage. This executes only the pinned CLI version query. The final receipt binds the
# selected native release and the ACP adapter's selected native npm
# payload/notices; provider legal texts are in the canonical marsh-dhi bundle.
FROM image AS smoke
ARG CODEX_VERSION
USER root
RUN --mount=type=bind,source=image-base/smoke.sh,target=/tmp/smoke.sh sh /tmp/smoke.sh node rg make gcc python3 \
 && test "$(command -v codex)" = /usr/local/bin/codex \
 && test "$(codex --version 2>/dev/null)" = "codex-cli ${CODEX_VERSION}"

FROM packages AS verify
ARG CODEX_VERSION
RUN sh /tmp/marsh-image-base/verifier.sh
RUN --mount=type=bind,from=smoke,source=/smoke.ok,target=/smoke.ok \
    --mount=type=bind,from=image,target=/image \
    python3 -I -S /image/usr/local/share/licenses/marsh-dhi/verify.py --root /image \
      --profile codex-kit --version "$CODEX_VERSION" --adapter codex-acp --receipt-dir /receipt

FROM image
COPY --from=verify /receipt/ /usr/local/share/licenses/marsh-dhi/
