# The services E2E fixture Kit image: the published acceptance fixture image
# (tests/acceptance/fixture, DHI Debian trixie runtime + Kit VM floor +
# marsh-fixture) plus a client toolbox: docker CLI plugins (compose, buildx),
# psql 17 + pgbench, redis-cli, curl, jq, python3, Node 22 and Supabase CLI 3.x.
# Debian packages are assembled onto the runtime base the way the fixture's own
# image is (packaging/image-base/assemble-packages.sh, staged here as
# image-base/). Every image is pinned by digest. No credentials anywhere.
FROM docker.io/mcavage/marsh-fixture@sha256:f4a162a732ec6a16c99b1dfdf9e925f9f15b1651b6ca6201aecb0fce8dffe7d1 AS fixture

FROM docker:29.9.2-cli@sha256:5e6baf67325a79f64ed3bed6863954d5e05d0f3d4e57cbdb14809346814bbfdb AS docker-plugins
FROM postgres:17@sha256:748a18a54d3842d43bc156f0840d78029b8d349e0feca98c12b9c4ce3404a6b3 AS pg
FROM node:22-trixie-slim@sha256:be1e2a3dc2c4fd68cdfadad2a696a280e0d2749098bee509230174d72fb768c6 AS node

FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2 AS packages
COPY --from=fixture /var/lib/dpkg /base/dpkg
COPY --from=fixture /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
# Tool packages are pinned to exact versions (a version Debian has since dropped
# fails the build loudly; the base is pinned by digest). libstdc++6: Node's runtime; bsdutils: script (PTY); iproute2: ss; netcat-openbsd:
# nc; sqlite3: the ignored-state scenarios. psql comes from postgresql-client-17 (Debian trixie
# ships 17); pgbench is not in a client package, so it is copied from the pinned
# postgres:17 image below. The client tools are linked straight to the real
# binaries: Debian's pg_wrapper is a Perl script that warns about the locale.
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs \
      postgresql-client-17=17.11-0+deb13u1 redis-tools=5:8.0.2-3+deb13u3 curl=8.14.1-2+deb13u7 \
      jq=1.7.1-6+deb13u4 python3=3.13.5-1 libstdc++6=14.2.0-19 bsdutils=1:2.41.5-0+deb13u1 \
      iproute2=6.15.0-1 netcat-openbsd=1.229-1 sqlite3=3.46.1-7+deb13u2 \
 && for tool in psql pg_isready pg_dump pg_dumpall pg_restore createdb dropdb; do \
      ln -sfn /usr/lib/postgresql/17/bin/$tool /rootfs/usr/bin/$tool; done \
 && ln -sfn nc.openbsd /rootfs/usr/bin/nc \
 && mkdir -p /rootfs/usr/local/bin \
 && ln -s ../lib/node_modules/npm/bin/npm-cli.js /rootfs/usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /rootfs/usr/local/bin/npx

FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2 AS supabase
ARG SUPABASE_VERSION=3.0.0
ARG SUPABASE_SHA256=243474fb5b491a0df04682fc1941cc5519559a1178ae01c225c633eda2e8acb8
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends ca-certificates curl >/dev/null \
 && curl -fsSL "https://github.com/supabase/cli/releases/download/v${SUPABASE_VERSION}/supabase_${SUPABASE_VERSION}_linux_arm64.tar.gz" -o /tmp/supabase.tgz \
 && echo "${SUPABASE_SHA256}  /tmp/supabase.tgz" | sha256sum -c - \
 && tar -xzf /tmp/supabase.tgz -C /usr/local/bin supabase

FROM fixture AS image
COPY --from=packages /rootfs/ /
COPY --from=docker-plugins /usr/local/libexec/docker/cli-plugins/docker-compose /usr/local/libexec/docker/cli-plugins/docker-buildx /usr/libexec/docker/cli-plugins/
COPY --from=pg /usr/lib/postgresql/17/bin/pgbench /usr/local/bin/pgbench
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
COPY --from=supabase /usr/local/bin/supabase /usr/local/bin/supabase
COPY --chmod=0755 agent.mjs /usr/local/bin/marsh-acp-fixture.mjs
# SEC-17's positive control: a plain `curl https://httpbin.org/headers` in a Kit job
# sends the (synthetic) placeholder header that a Kit-scoped sbx secret substitutes
# (a wrapper, because jobs run as the Mac user with that user's HOME).
COPY curlrc /etc/nfx-curlrc
COPY --chmod=0755 curl-wrapper /usr/local/bin/curl
ENV FIXTURE_IMAGE_ENV=image-config
USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["/usr/local/bin/marsh-fixture"]

# Run every tool the services scenarios call, so a missing library fails the
# build rather than a scenario.
FROM image AS smoke
RUN docker --version && docker compose version && docker buildx version \
 && psql --version && pgbench --version && redis-cli --version && curl --version >/dev/null \
 && jq --version && python3 --version && node --version && npm --version && supabase --version \
 && script --version && ss --version && nc -h 2>&1 | grep -q OpenBSD && sqlite3 --version \
 && test "$(node --version | cut -d. -f1)" = v22 \
 && touch /tmp/smoke.ok

FROM image
RUN --mount=type=bind,from=smoke,source=/tmp/smoke.ok,target=/tmp/smoke.ok test -f /tmp/smoke.ok
