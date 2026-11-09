#!/bin/sh
# Fetch the Docker Compose CLI plugin into OUT/lib/docker/cli-plugins, pinned
# to a release and a SHA-256 per architecture (a mismatch fails the build).
# Every Kit image already has the docker CLI (docker-ce-cli, kit-vm.packages);
# this adds `docker compose`, so an agent in a project with Docker on can use
# the project's endpoint. The same pins are in kits/marsh-docker-cli.
#
#   TARGETARCH=arm64 docker-compose.sh OUT      (needs curl and ca-certificates)
#
# To move the pin, change the version and both digests together.
set -eu
COMPOSE_VERSION=5.7.0
case "${TARGETARCH:?TARGETARCH is not set}" in
  arm64) arch=aarch64; sha=15861535d14e26555736b49376c95e14bfd94285366c8446bfac7156a972e76f ;;
  amd64) arch=x86_64; sha=7758048ad38e580f22559603312874fec8bb638d021a9e64182385fc17cc12a2 ;;
  *) echo "docker-compose.sh: unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;;
esac
plugins="$1/lib/docker/cli-plugins"
mkdir -p "$plugins"
curl -fsSL "https://github.com/docker/compose/releases/download/v${COMPOSE_VERSION}/docker-compose-linux-${arch}" \
  -o "$plugins/docker-compose"
echo "${sha}  $plugins/docker-compose" | sha256sum -c -
chmod 0755 "$plugins/docker-compose"
