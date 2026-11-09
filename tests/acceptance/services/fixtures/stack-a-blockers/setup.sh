#!/bin/sh
# Pre-create the external volume nfx-ext with a sentinel file. Uses whatever
# engine $DOCKER_HOST selects. Idempotent.
set -eu
docker volume create nfx-ext >/dev/null
docker run --rm -v nfx-ext:/e busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec sh -c 'echo nfx-ext-sentinel > /e/sentinel'
