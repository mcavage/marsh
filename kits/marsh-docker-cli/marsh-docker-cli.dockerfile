# Docker's official static CLI (download.docker.com) and Compose's release
# binary (github.com/docker/compose), each pinned to a version and a SHA-256.
# To move a pin, change the version and both digests together; the build
# fails on a mismatch. COMPOSE_VERSION follows the services engine's
# docker-compose-plugin pin (tests/release/test_compose_pins.py), and its
# digests are upstream's (tests/release/compose-release-digests.txt).
FROM debian:trixie-slim AS build
ARG TARGETARCH
ARG DOCKER_VERSION=29.9.3
ARG COMPOSE_VERSION=5.7.1
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates
RUN set -eu; \
    case "$TARGETARCH" in \
      arm64) arch=aarch64; \
             docker_sha=1b27fd739378ef938401e81ce5afb73b7ad7432d4f598814a9ff5f826bb464d2; \
             compose_sha=f2f49f903240895fa764c0dca3a14548b754ad9426f6b609ccaf55ec5a3aa2a5 ;; \
      amd64) arch=x86_64; \
             docker_sha=3fb5d1f13014c18cc54945bc177c58a9056a482dada031e17d12cf2e6922c973; \
             compose_sha=af426722a8af5de812fcee30cc1c39f0639f5f723cbadefb25e5d4db054064cf ;; \
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
