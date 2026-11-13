#!/bin/sh
# Fetch the Docker Compose CLI plugin into OUT/lib/docker/cli-plugins, pinned
# to a release and a SHA-256 per architecture (a mismatch fails the build).
# Every Kit image already has the docker CLI (docker-ce-cli, kit-vm.packages);
# this adds `docker compose`, so an agent in a project with Docker on can use
# the project's endpoint. The same pins are in kits/marsh-docker-cli.
#
#   TARGETARCH=arm64 docker-compose.sh OUT      (needs curl and ca-certificates)
#
# The version is the upstream version of the services engine's
# docker-compose-plugin pin (packaging/svc-engine/Dockerfile), which Renovate
# moves; tests/release/test_compose_pins.py fails until this and every other
# copy follow it. To move the pin, change the version and both digests together.
set -eu
COMPOSE_VERSION=5.7.1
case "${TARGETARCH:?TARGETARCH is not set}" in
  arm64) arch=aarch64; sha=f2f49f903240895fa764c0dca3a14548b754ad9426f6b609ccaf55ec5a3aa2a5 ;;
  amd64) arch=x86_64; sha=af426722a8af5de812fcee30cc1c39f0639f5f723cbadefb25e5d4db054064cf ;;
  *) echo "docker-compose.sh: unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;;
esac
plugins="$1/lib/docker/cli-plugins"
mkdir -p "$plugins"
curl -fsSL "https://github.com/docker/compose/releases/download/v${COMPOSE_VERSION}/docker-compose-linux-${arch}" \
  -o "$plugins/docker-compose"
echo "${sha}  $plugins/docker-compose" | sha256sum -c -
chmod 0755 "$plugins/docker-compose"
