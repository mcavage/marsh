# Docker's official static CLI (download.docker.com) and Compose's release
# binary (github.com/docker/compose), each pinned to a version and a SHA-256.
# To move a pin, change the version and both digests together; the build
# fails on a mismatch.
FROM debian:trixie-slim AS build
ARG TARGETARCH
ARG DOCKER_VERSION=29.9.3
ARG COMPOSE_VERSION=5.7.0
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates
RUN set -eu; \
    case "$TARGETARCH" in \
      arm64) arch=aarch64; \
             docker_sha=1b27fd739378ef938401e81ce5afb73b7ad7432d4f598814a9ff5f826bb464d2; \
             compose_sha=15861535d14e26555736b49376c95e14bfd94285366c8446bfac7156a972e76f ;; \
      amd64) arch=x86_64; \
             docker_sha=3fb5d1f13014c18cc54945bc177c58a9056a482dada031e17d12cf2e6922c973; \
             compose_sha=7758048ad38e580f22559603312874fec8bb638d021a9e64182385fc17cc12a2 ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    plugins=/out/usr/local/lib/docker/cli-plugins; \
    mkdir -p /out/usr/local/bin "$plugins" /tmp/docker; \
    curl -fsSL "https://download.docker.com/linux/static/stable/${arch}/docker-${DOCKER_VERSION}.tgz" -o /tmp/docker.tgz; \
    echo "${docker_sha}  /tmp/docker.tgz" | sha256sum -c -; \
    tar -xzf /tmp/docker.tgz -C /tmp/docker docker/docker; \
    install -m 0755 /tmp/docker/docker/docker /out/usr/local/bin/docker; \
    curl -fsSL "https://github.com/docker/compose/releases/download/v${COMPOSE_VERSION}/docker-compose-linux-${arch}" -o "$plugins/docker-compose"; \
    echo "${compose_sha}  $plugins/docker-compose" | sha256sum -c -; \
    chmod 0755 "$plugins/docker-compose"

FROM scratch
LABEL org.marsh.kit.contract=1
COPY --from=build /out/ /
