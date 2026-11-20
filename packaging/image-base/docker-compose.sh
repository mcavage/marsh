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
# copy follow it. To move the pin, change the version and both digests together,
# and append the release's two digests to tests/release/compose-release-digests.txt.
set -eu
COMPOSE_VERSION=5.8.0
case "${TARGETARCH:?TARGETARCH is not set}" in
  arm64) arch=aarch64; sha=1a9c918b2fb49b9c87f0018c64b5abd0d30715155e98dc939779fbaee6c414d3 ;;
  amd64) arch=x86_64; sha=b8840f5008c447f41fce0a57ae3adeab42c89b1eb6e6012524bcabbfb4ffbaf6 ;;
  *) echo "docker-compose.sh: unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;;
esac
plugins="$1/lib/docker/cli-plugins"
mkdir -p "$plugins"
curl -fsSL "https://github.com/docker/compose/releases/download/v${COMPOSE_VERSION}/docker-compose-linux-${arch}" \
  -o "$plugins/docker-compose"
echo "${sha}  $plugins/docker-compose" | sha256sum -c -
chmod 0755 "$plugins/docker-compose"
