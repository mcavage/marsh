# marsh shell Kit: the DHI Debian runtime base plus the Kit VM floor (Docker
# Engine, which runs this Kit's jobs from this same image), the POSIX text
# tools and python3 (also as `python`), assembled from Debian packages in a -dev build stage
# (image-base/assemble-packages.sh). The runtime image has no package manager.
FROM dhi.io/debian-base:trixie@sha256:2701bf4a0eb97d2233a94b031043342d77595152567793db9fc52b591d97773a AS base

FROM dhi.io/debian-base:trixie-dev@sha256:de916b98b4cf1dfb1a82a9aaa466106257d17c7a9cd4fe83a007778d11917c3f AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm @userland python3 python-is-python3

# Docker Compose plugin, pinned with a SHA-256 (image-base/docker-compose.sh).
# The image already has the docker CLI (docker-ce-cli from kit-vm.packages).
FROM dhi.io/debian-base:trixie-dev@sha256:de916b98b4cf1dfb1a82a9aaa466106257d17c7a9cd4fe83a007778d11917c3f AS compose
ARG TARGETARCH
COPY image-base/docker-compose.sh /tmp/docker-compose.sh
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
 && sh /tmp/docker-compose.sh /out

FROM base AS image
COPY --from=packages /rootfs/ /
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
COPY --from=compose /out/ /usr/local/
COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-shell
LABEL com.docker.sandboxes.start-docker=true
# The marsh Kit contract this image implements (marsh_contracts::kit_source):
# `marsh kit upgrade` moves a packaged command only to a supported one.
LABEL org.marsh.kit.contract=1
USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["/usr/local/bin/marsh-shell"]

# Exercise the installed runtime and commands before publishing.
FROM image AS smoke
USER root
RUN --mount=type=bind,source=image-base/smoke.sh,target=/tmp/smoke.sh sh /tmp/smoke.sh python \
 && docker --version && docker compose version

FROM image
RUN --mount=type=bind,from=smoke,source=/smoke.ok,target=/tmp/smoke.ok test -f /tmp/smoke.ok
