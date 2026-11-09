#!/usr/bin/env bash
# The whole E3 evidence run, one VM-lock-holding invocation:
#   vmlock.py --shared --run e3-all -- bash e3/run-all.sh [breaks]
# 1. the normal session over every step: must exit 0;
# 2. (with `breaks`) the break matrix: every mode must trip its own witness.
H=$(cd "$(dirname "$0")" && pwd)
source $H/lib.sh
LOG=$OUT/log; mkdir -p $LOG
RC=0
STEPS="sec01 raw pins protected scopes_py sec15 compose tc build canon freeze restart diff race bounds limits volumes netsep hijack ledger a12"
BREAKS= bash $H/session.sh up $STEPS 2>&1 | tee $LOG/all-normal.txt; r=${PIPESTATUS[0]}
echo "== normal run: exit $r"; [ $r -eq 0 ] || RC=1
if [ "${1:-}" = breaks ]; then
  bash $H/breaks.sh 2>&1 | tee $LOG/all-breaks.txt; r=${PIPESTATUS[0]}
  echo "== break matrix: exit $r"; [ $r -eq 0 ] || RC=1
fi
exit $RC
