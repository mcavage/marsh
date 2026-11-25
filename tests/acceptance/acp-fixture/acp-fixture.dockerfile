# The DHI Node.js runtime base plus the Kit VM floor (Docker Engine, which
# runs this Kit's jobs from this same image), assembled from Debian packages
# in a -dev build stage (packaging/image-base/assemble-packages.sh).
FROM dhi.io/node:26-debian13@sha256:fa9dc99f7b68bdc38306b03ee76d9318937ce648a1c73d217ff6a603e7a89da1 AS base

FROM dhi.io/debian-base:trixie-dev@sha256:5ff4ba110ee6df53768d87f200ddf33ead88452b9fb73448c70c89362364c44f AS packages
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
