#!/bin/sh
# Attack loop for SVC-SEC-04, run from the project root $P with the flipper
# running and DOCKER_HOST selecting the endpoint under test:
#
#   attack.sh RUN_ID [N]        N attempts per variant (default 300)
#
# The harness writes /etc/nfx-vmroot-RUN_ID into the services VM root first.
# Each attempt binds $P/race/x (variants -v, --mount) or $P/race/p/ok (-v) and
# reads the sentinel through it. Prints one line per variant:
#   VARIANT escaped=N ok=N refused=N other=N
# escaped = the sentinel was read (the root was bound); ok = ok.txt was read;
# refused = docker exited 125 with a "marsh:" message; other = anything else.
set -u
run=${1:?RUN_ID}
n=${2:-300}
P=$(pwd)
IMG=busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec
attempt() {  # variant, bind args...
  variant=$1; shift
  escaped=0; ok=0; refused=0; other=0
  i=0
  while [ "$i" -lt "$n" ]; do
    out=$(docker run --rm "$@" $IMG sh -c 'cat /h/etc/nfx-vmroot-'"$run"' 2>/dev/null && echo ESCAPED; cat /h/ok.txt' 2>&1); rc=$?
    case "$out" in
      *ESCAPED*) escaped=$((escaped + 1)) ;;
      *marsh:*) refused=$((refused + 1)) ;;
      ok) ok=$((ok + 1)) ;;
      *) other=$((other + 1)) ;;
    esac
    i=$((i + 1))
  done
  echo "$variant escaped=$escaped ok=$ok refused=$refused other=$other"
}
attempt v-x      -v "$P/race/x:/h"
attempt mount-x  --mount "type=bind,src=$P/race/x,dst=/h"
attempt v-p      -v "$P/race/p/ok:/h"
