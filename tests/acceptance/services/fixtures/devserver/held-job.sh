#!/bin/sh
# The held job of RST-13 (run as `job -- sh held-job.sh` in the original and in
# lane x). It starts the unchanged dev server, waits for /health, captures
# /items (ITEMS1), prints READY, blocks until the harness sends one line on stdin,
# captures /items again (ITEMS2) and stops the server. Output, in order:
#   ITEMS1 <json>      (before the release)
#   READY
#   ITEMS2 <json>      (after the release)
# Both jobs must have printed READY before either is released, so both servers
# are alive on :3000 at the same time (each job has its own netns).
#   held-job.sh [HEALTH_CEILING_SECONDS]   default 30
set -u
ceiling=${1:-30}
npm run dev >/tmp/nfx-devserver.log 2>&1 &
server=$!
waited=0
until curl -sf localhost:3000/health >/dev/null 2>&1; do
  waited=$((waited + 1))
  if [ "$waited" -ge $((ceiling * 5)) ]; then echo "HEALTH-TIMEOUT"; kill "$server" 2>/dev/null; exit 1; fi
  sleep 0.2
done
echo "ITEMS1 $(curl -s localhost:3000/items)"
echo READY
read -r _release
echo "ITEMS2 $(curl -s localhost:3000/items)"
kill "$server" 2>/dev/null
wait "$server" 2>/dev/null
exit 0
