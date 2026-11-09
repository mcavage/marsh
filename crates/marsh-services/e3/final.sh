#!/usr/bin/env bash
# Rebuild, re-run what changed since the last normal pass, then the break matrix and
# the guest-toolchain lint and in-process witnesses. One VM-lock-holding invocation.
H=$(cd "$(dirname "$0")" && pwd)
source $H/lib.sh
LOG=$OUT/log; mkdir -p $LOG
RC=0
bash $H/build-linux.sh 2>&1 | tail -2
BREAKS= bash $H/session.sh up limits 2>&1 | tee $LOG/final-limits.txt; [ ${PIPESTATUS[0]} -eq 0 ] || RC=1
bash $H/breaks.sh "$@" 2>&1 | tee $LOG/final-breaks.txt; [ ${PIPESTATUS[0]} -eq 0 ] || RC=1
bash $H/clippy-linux.sh 2>&1 | tee $LOG/final-clippy-linux.txt
bash $H/test-linux.sh 2>&1 | tee $LOG/final-witness-linux.txt; [ ${PIPESTATUS[0]} -eq 0 ] || RC=1
echo "== final: exit $RC"
exit $RC
