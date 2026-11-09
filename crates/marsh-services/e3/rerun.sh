#!/usr/bin/env bash
H=$(dirname "$0"); source $H/lib.sh; LOG=$OUT/log
bash $H/session.sh up pins restart bounds > $LOG/rerun-normal.txt 2>&1
BREAKS=bind-repin-unchecked bash $H/session.sh up pins > $LOG/break-bind-repin-unchecked.txt 2>&1
BREAKS=bind-pin-leak bash $H/session.sh up pins > $LOG/break-bind-pin-leak.txt 2>&1
echo done
