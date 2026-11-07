# The DHI Node.js runtime base plus the Kit VM floor (Docker Engine, which
# runs this Kit's jobs from this same image), assembled from Debian packages
# in a -dev build stage (packaging/image-base/assemble-packages.sh).
FROM dhi.io/node:26-debian13@sha256:dea81b92cdabf3879e8edaad05c8aa4460603b6b0351fcf6c481eb12b24c8bd5 AS base

FROM dhi.io/debian-base:trixie-dev@sha256:61025cea33d5910165ced8b33edd3f6b6bfe161e129174887936a1b9a1442b20 AS packages
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
