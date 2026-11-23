# The DHI Node.js runtime base plus the Kit VM floor (Docker Engine, which
# runs this Kit's jobs from this same image), assembled from Debian packages
# in a -dev build stage (packaging/image-base/assemble-packages.sh).
FROM dhi.io/node:26-debian13@sha256:85120617b61b168bce65bcac3a1564ec55a10c6e43a1bc92239fe5e4e03224ae AS base

FROM dhi.io/debian-base:trixie-dev@sha256:3a0f849e7ecad5433a8a517e4606794dc65941b275177c7f44002c3fc322d7a8 AS packages
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
