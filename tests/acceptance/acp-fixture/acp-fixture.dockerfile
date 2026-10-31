# The DHI Node.js runtime base plus the Kit VM floor (Docker Engine, which
# runs this Kit's jobs from this same image), assembled from Debian packages
# in a -dev build stage (packaging/image-base/assemble-packages.sh).
FROM dhi.io/node:26-debian13@sha256:38c8bbf0ccffd6603b32f602c08b4f407a2a83b010f7371854deaa6e0c2576c4 AS base

FROM dhi.io/debian-base:trixie-dev@sha256:7995f6739e5ad75cbd57d9356f7d7c83d43dd957682b6a000f32ff95b441a9b6 AS packages
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
