# marsh shell Kit: the DHI Debian runtime base plus the Kit VM floor (Docker
# Engine, which runs this Kit's jobs from this same image), the POSIX text
# tools and python3 (also as `python`), assembled from Debian packages in a -dev build stage
# (image-base/assemble-packages.sh). The runtime image has no package manager.
FROM dhi.io/debian-base:trixie@sha256:5c9d8c2c90a0a4672f44a42a485c99e1b95f4bfad5c8ef49934f34b7ffb6e23b AS base

FROM dhi.io/debian-base:trixie-dev@sha256:94571f3dc07fbc7166f6f1cbf1fe068a7647198ee2554e6b90d59a68cc8b053c AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm @userland python3 python-is-python3

FROM base AS image
COPY --from=packages /rootfs/ /
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
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
RUN --mount=type=bind,source=image-base/smoke.sh,target=/tmp/smoke.sh sh /tmp/smoke.sh python

FROM image
RUN --mount=type=bind,from=smoke,source=/smoke.ok,target=/tmp/smoke.ok test -f /tmp/smoke.ok
