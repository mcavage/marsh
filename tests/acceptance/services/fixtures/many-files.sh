#!/bin/sh
# Fill a named volume with N small files (default 300000) for freeze-budget
# tests, in directories of 1000. Uses whatever engine $DOCKER_HOST selects.
#
#   many-files.sh [VOLUME [N]]        defaults: VOLUME=many N=300000
set -eu
volume=${1:-many}
count=${2:-300000}
case "$count" in ''|*[!0-9]*) echo "many-files: N must be a number" >&2; exit 2 ;; esac
docker volume create "$volume" >/dev/null
exec docker run --rm -v "$volume:/v" -e N="$count" busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec sh -c '
  set -eu
  cd /v
  d=0
  made=0
  while [ "$made" -lt "$N" ]; do
    mkdir -p "d$d"
    n=1000
    [ $(($N - made)) -lt 1000 ] && n=$(($N - made))
    i=0
    while [ "$i" -lt "$n" ]; do echo x > "d$d/f$i"; i=$((i + 1)); done
    made=$((made + n))
    d=$((d + 1))
  done
  echo "many-files: $made files in $d directories"
'
