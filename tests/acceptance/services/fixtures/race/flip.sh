#!/bin/sh
# Symlink flipper, run in a container with the race directory mounted at /r:
#   docker run -d --name flip -v $P/race:/r busybox sh /r/flip.sh
# Atomically (rename over) swaps /r/x between `ok` and `/`, and /r/p between
# `good` and `evil` (whose ok is a symlink to /), as fast as it can, for at most
# FLIP_SECONDS (default 600). A bind source of $P/race/x or $P/race/p/ok then
# names the project in one moment and the VM root in the next.
set -u
cd /r
end=$(( $(date +%s) + ${FLIP_SECONDS:-600} ))
swap() {  # TARGET NAME
  ln -sfn "$1" "$2.tmp" && mv -T "$2.tmp" "$2"
}
while [ "$(date +%s)" -lt "$end" ]; do
  swap / x;  swap ok x
  swap evil p; swap good p
done
