#!/bin/sh
# The out-of-grant files the hostile variants point at: ../nfx-outside next to
# the staged project (cwd = the project). The secret marker must appear in no
# fork container's env, files or logs (SEC-13 c3); it is also a redaction pattern.
set -eu
mkdir -p ../nfx-outside
printf 'NFX-CANARY-SECRET-7f3a91c2\n' > ../nfx-outside/secret.txt
printf 'NFX_SECRET=NFX-CANARY-SECRET-7f3a91c2\n' > ../nfx-outside/env
cat > ../nfx-outside/include.yaml <<'YAML'
services:
  included:
    image: busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec
    command: ["sleep", "1d"]
    environment:
      NFX_SECRET: NFX-CANARY-SECRET-7f3a91c2
YAML
cat > ../nfx-outside/extends.yaml <<'YAML'
services:
  base:
    image: busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec
    command: ["sleep", "1d"]
    environment:
      NFX_SECRET: NFX-CANARY-SECRET-7f3a91c2
YAML
