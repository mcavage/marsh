#!/bin/sh
# A container that prints `tick N` every 0.5 s: `docker logs -f ticker` is the
# long-lived stream the daemon-restart and cancel scenarios watch.
#   start.sh [NAME]   (default: ticker)   stop.sh [NAME]
set -eu
name=${1:-ticker}
exec docker run -d --name "$name" busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec \
  sh -c 'n=0; while :; do n=$((n + 1)); echo "tick $n"; sleep 0.5; done'
