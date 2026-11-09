#!/bin/sh
# Build the local image (nfx/local-app:1) with whatever engine $DOCKER_HOST selects.
set -eu
cd "$(dirname "$0")"
docker build -q -t nfx/local-app:1 local >/dev/null
