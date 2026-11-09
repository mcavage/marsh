#!/usr/bin/env bash
# One lock-holding session: bring the engine up (optionally with break modes) and run steps.
#   BREAKS=filter-off ./session.sh up sec01 raw pins protected scopes_py sec15 compose tc canon freeze restart diff race bounds limits volumes netsep hijack ledger a12 down
#
# The exit status is the verdict: nonzero if any step's script failed (a step is
# its pipeline's status, `pipefail`, not `tee`'s).
set -o pipefail
source "$(dirname "$0")/lib.sh"
H=$(dirname "$0")
LOG=${LOG:-$OUT/log}; mkdir -p "$LOG"
export E3_BREAKS=${BREAKS:-}
SFX=${E3_BREAKS:+.$E3_BREAKS}
RC=0
step() { echo; echo "######## $1 ${E3_BREAKS:+[breaks: $E3_BREAKS]}"; }
# t NAME CMD...   run a check script, keep its log, fail the session if it fails
t() { local name=$1; shift; step "$name"; "$@" 2>&1 | tee "$LOG/$name$SFX.txt"; local rc=${PIPESTATUS[0]}
      if [ "$rc" -ne 0 ]; then echo "== step failed: $name (exit $rc)"; RC=1; fi; }
py() { sbx exec "$SB" python3 "$PROJ/.e3/$1" "${@:2}"; }
for s in "$@"; do
  case $s in
    up) step up; bash $H/engine-up.sh 2>&1 | tail -3
        scope base base a1; scope a a a1; scope b b a1; scope stacka stacka a1 $PROJ/stack-a; scope tc tc a1; scope base2 base a2
        vx sh -c 'P="docker -H unix:///var/run/docker.sock"; $P rm -f $($P ps -aq) >/dev/null 2>&1; $P volume prune -af >/dev/null 2>&1; $P network prune -f >/dev/null 2>&1; true';;
    record) step record; bash $H/record.sh 2>&1 | tail -3;;
    sec01) t sec01 bash $H/sec01.sh base;;
    raw) t raw py rawdocker.py "$(sock base)" ${CAL:+--calibrate};;
    pins) t pins bash $H/pins.sh base;;
    protected) t protected bash $H/protected.sh base;;
    scopes_py) t scopes py scopes.py /var/lib/nfx-e3/socks;;
    sec15) t sec15 bash $H/sec15.sh base a;;
    compose) t compose bash $H/compose.sh stacka;;
    tc) t tc bash $H/tc.sh tc;;
    race) step race; vx sh -c "echo VMROOT-CANARY-race > /etc/nfx-vmroot-race"
          t race sbx exec $SB timeout $(( ${RACE_SECS:-60} + 120 )) python3 $PROJ/.e3/race.py "$(sock base)" $PROJ ${RACE_SECS:-60} ${RACE_WORKERS:-4} /etc/nfx-vmroot-race;;
    diff) t diff py differential.py $PROJ/.e3/corpus.jsonl /var/lib/nfx-e3/socks/prod.sock "$(sock base)" $PROJ;;
    build) t build bash $H/build.sh base;;
    canon) t canon bash $H/canon.sh;;
    freeze) t freeze bash $H/freeze.sh base;;
    restart) t restart bash $H/restart.sh base;;
    bounds) t bounds py bounds.py "$(sock base)";;
    limits) t limits bash $H/limits.sh base;;
    volumes) t volumes bash $H/volumes.sh a b;;
    netsep) t netsep bash $H/netsep.sh a b;;
    hijack) t hijack py hijack.py "$(sock base)";;
    ledger) t ledger bash $H/ledger.sh base;;
    a12) t a12 bash $H/a12.sh base;;
    quick:*) t "quick-${s#quick:}" bash $H/quick.sh "${s#quick:}";;
    down) step down; sbx rm -f nfx-e3-a >/dev/null 2>&1 || true;;
    *) echo "unknown step $s"; RC=1;;
  esac
done
exit $RC
