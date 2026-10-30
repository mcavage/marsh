ARG SHELL_IMAGE
FROM rust:1.99-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 AS build

WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked \
 && install -D -m 0755 target/release/marsh-acceptance-fixture /out/usr/local/bin/marsh-fixture \
 && ln -s marsh-fixture /out/usr/local/bin/fixture

# The DHI Debian runtime base plus the Kit VM floor (Docker Engine, which runs
# this Kit's jobs from this same image), the POSIX tools and python3 (the
# acceptance probes run in the fixture's VM), assembled from Debian packages in
# a -dev build stage (image-base/assemble-packages.sh).
FROM ${SHELL_IMAGE} AS base

FROM dhi.io/debian-base:trixie-dev@sha256:addf9d8e974b044e4dfc7b4186756d3e0786b573f6a21a53697d03b133f70a77 AS packages
COPY --from=base /var/lib/dpkg /base/dpkg
COPY --from=base /etc/passwd /etc/group /base/etc/
COPY image-base /tmp/marsh-image-base
RUN sh /tmp/marsh-image-base/assemble-packages.sh /base /rootfs @kit-vm @userland python3

FROM base AS image
COPY --from=packages /rootfs/ /
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
# The image ships its own `fixture` on PATH (processes acceptance: the entry
# case execs it; a job of another registered name keeps it local), and one
# image-config variable that must never be forwarded to a spawned child.
COPY --from=build /out/usr/local/bin/ /usr/local/bin/
ENV FIXTURE_IMAGE_ENV=image-config
LABEL com.docker.sandboxes.start-docker=true
USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["/usr/local/bin/marsh-fixture"]

# Exercise the installed runtime and commands before publishing.
FROM image AS smoke
USER root
RUN --mount=type=bind,source=image-base/smoke.sh,target=/tmp/smoke.sh sh /tmp/smoke.sh && test -x /usr/local/bin/marsh-fixture

FROM image
RUN --mount=type=bind,from=smoke,source=/smoke.ok,target=/tmp/smoke.ok test -f /tmp/smoke.ok
