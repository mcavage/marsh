#!/usr/bin/env bash
H=$(dirname "$0"); source $H/lib.sh; LOG=$OUT/log
BREAKS=filter-case-sensitive bash $H/session.sh up raw > $LOG/smoke-case-sensitive.txt 2>&1
CAL=1 BREAKS=filter-off bash $H/session.sh up raw down > $LOG/calibrate-filter-off.txt 2>&1
echo done
