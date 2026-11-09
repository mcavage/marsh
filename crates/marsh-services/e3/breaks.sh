#!/usr/bin/env bash
# The break matrix. For each break mode: bring the engine up with it, run the
# scenario that names it, and require
#   1. the session to FAIL (nonzero),
#   2. at least one BAD line that matches the witness the mode is meant to trip,
#   3. no BAD line that does not match it (the controls around it stay healthy).
# A mode whose scenario passes, or fails for some other reason, fails this script.
#
# In-process modes (hijack-deadlines-off, project-cap-per-root, list-fail-open,
# network-name-forwarded, ...) are in e3/witness.sh.
#
#   breaks.sh            all rows
#   breaks.sh MODE...    only those modes
H=$(cd "$(dirname "$0")" && pwd)
source $H/lib.sh
LOG=$OUT/log; mkdir -p $LOG
ONLY=("$@")
OK=0; NOT=0
# row MODE "STEPS" "REGEX of the BAD labels the mode must trip"
row() {
  local mode=$1 steps=$2 re=$3
  if [ ${#ONLY[@]} -gt 0 ] && ! printf '%s\n' "${ONLY[@]}" | grep -qx "$mode"; then return; fi
  local log=$LOG/break-$mode${4:+.$4}.txt
  BREAKS=$mode bash $H/session.sh up $steps > $log 2>&1; local rc=$?
  local bad hit other oks
  bad=$(grep -E '^(BAD |FAIL |DIFF )' $log)
  hit=$(printf '%s\n' "$bad" | grep -cE "$re")
  other=$(printf '%s\n' "$bad" | grep -E '.' | grep -vcE "$re")
  oks=$(grep -cE '^ok ' $log)
  if [ $rc -ne 0 ] && [ "$hit" -ge 1 ] && [ "$other" -eq 0 ] && [ "$oks" -ge 3 ]; then
    OK=$((OK+1)); printf 'ok   %-34s fails as intended: %s witness line(s) tripped, %s controls ok\n' "$mode" "$hit" "$oks"
  else
    NOT=$((NOT+1)); printf 'BAD  %-34s rc=%s tripped=%s unrelated=%s controls_ok=%s  (%s)\n' "$mode" "$rc" "$hit" "$other" "$oks" "$log"
    printf '%s\n' "$bad" | grep -vE "$re" | head -3 | sed 's/^/       unrelated: /'
  fi
}

row filter-off                      "sec01 raw"        '.'
row filter-no-upgrade               "hijack"           'attach|exec|PTY'
row filter-case-sensitive           "raw"              '.'
row filter-lenient-json             "raw"              '.'
row filter-raw-after-upgrade-header "raw"              '.'
row filter-no-resolve               "pins"             'symlink|magic link|sibling|outside|VM root|missing leaf|restore|volumes-from|NO_XDEV|mount point|tmpfs'
row filter-forward-original-bytes   "canon"            'fixed|chunked|control: ping'
RACE_SECS=60 row bind-no-pin        "pins race"        'c1|c2|VM root|escape|ESCAPE|canary|ORIGINAL|file pin|failed restore|volumes-from|restore|pin'
row bind-repin-unchecked            "pins"             'c1|VM root|reboot|restore'
row bind-pin-leak                   "pins"             'c2|pin of k|pin path|mountpoint|survived|released|file pin|failed restore|volumes-from|poisoned'
row protected-subpin-off            "protected"        'write to|docker cp|canaries|\.GIT|\.Git|\.MARSH|\.Marsh|late|\.git|\.marsh|nonrecursive|-v |--mount|mounts-shape|stray'
row bind-nonrecursive-allowed       "protected"        'bind-recursive|nonrecursive'
row protected-by-name               "protected"        '\.GIT|\.Git|\.MARSH|\.Marsh|ci-pwn|case|under \.git'
row protected-marsh-late-off        "protected"        'late|\.marsh'
row pin-allow-xdev                  "pins"             'NO_XDEV|tmpfs|mount point'
row pin-inherit-off                 "pins"             'volumes-from'
row pin-file-leak                   "pins"             'file pin|failed restore'
row restore-fail-open               "pins"             'failed restore'
row lane-authority-off              "scopes_py"        '.'
row scope-reference-check-off       "scopes_py"        '.'
row socket-substitute-raw           "sec15"            'FAIL|attempt|workspace|evil|raw cases|inside|nested|mount source|production|spelling|c1|c3'
row socket-refuse                   "sec15"            'accepted|socket|spelling|nested|inside|raw matrix|authority|attempt|workspace|c3|c1'
row capture-api-gate-off            "freeze"           'mutation during freeze|while frozen|waited|freeze was|nothing was paused|state changed|after the freeze'
row freeze-no-wait                  "freeze"           'late create|waited|freeze was granted'
row ledger-persist-ignore           "ledger"           'create while|network create while|volume create while|ground truth'
row update-limits-unmerged          "limits"           'cpu-period|the refused updates'
row services-resource-limit-off     "limits"           '.' limits
row restart-update-allowed          "limits"           'restart'
row commit-any-repo                 "sec01"            'commit'
row outputs-open                    "sec01"            'build outputs'
row network-mtu-allowed             "sec01"            'MTU'
row private-propagation-allowed     "sec01"            'private'
row fullid-name-allowed             "sec01"            'full ID'
row anon-volume-unlabeled           "volumes"          'anonymous|image-declared|lists|reclaims|prune|label'
row volume-claim-racy               "volumes"          'explicit|implicit|both|disagree'
row scope-default-network-off       "netsep"           'reached|different networks|same network|by name|own listener'
row reserved-labels-allowed         "quick:labels"     'label' labels
row services-resource-limit-off     "quick:limits"     'limits|SEC-20' quick
row publish-nonloopback             "quick:publish"    'EP-10|publish'
echo "== breaks: $OK ok, $NOT not as intended"
[ $NOT -eq 0 ]
