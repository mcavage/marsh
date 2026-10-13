# marsh shell Kit: the DHI Debian runtime base plus the Kit VM floor (Docker
# Engine, which runs this Kit's jobs from this same image), the POSIX text
# tools and python3, assembled from Debian packages in a -dev build stage
# (image-base/assemble-packages.sh). The runtime image has no package manager.
FROM dhi.io/debian-base:trixie@sha256:20079b51710f0397da5e056bfc7156b6aafc7ff3aa4ef4cbcb0b0f1df99cd8c4 AS base

FROM dhi.io/debian-base:trixie-dev@sha256:c6fc0de84b65bc20346cee5f071fd976ccf383431515db2a4d9721ca936feb9f AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm @userland python3

FROM base AS image
COPY --from=packages /rootfs/ /
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-shell
LABEL com.docker.sandboxes.start-docker=true
USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["/usr/local/bin/marsh-shell"]

# The runtime image has no Python: smoke-test it in a throwaway stage, then
# verify it, read-only, from the build stage.
FROM image AS smoke
USER root
RUN --mount=type=bind,source=image-base/smoke.sh,target=/tmp/smoke.sh sh /tmp/smoke.sh

FROM packages AS verify
RUN sh /tmp/marsh-image-base/verifier.sh
RUN --mount=type=bind,from=smoke,source=/smoke.ok,target=/smoke.ok \
    --mount=type=bind,from=image,target=/image \
    python3 -I -S /image/usr/local/share/licenses/marsh-dhi/verify.py --root /image --receipt-dir /receipt

FROM image
COPY --from=verify /receipt/ /usr/local/share/licenses/marsh-dhi/
