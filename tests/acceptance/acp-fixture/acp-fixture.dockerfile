# The DHI Node.js runtime base plus the Kit VM floor (Docker Engine, which
# runs this Kit's jobs from this same image), assembled from Debian packages
# in a -dev build stage (packaging/image-base/assemble-packages.sh).
FROM dhi.io/node:26-debian13@sha256:dc87c509f7279f734d04036f696bf9aad1b8d66f34750f32c30c5b85e033187f AS base

FROM dhi.io/debian-base:trixie-dev@sha256:cc4e05ff415ee2d83918f2af45c737983209924ef7362c53f00cfcdaa1c94326 AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm

FROM base
COPY --from=packages /rootfs/ /
COPY --chmod=0755 agent.mjs /usr/local/bin/marsh-acp-fixture.mjs
LABEL com.docker.sandboxes.start-docker=true
USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["node", "/usr/local/bin/marsh-acp-fixture.mjs"]
