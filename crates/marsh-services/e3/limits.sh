#!/usr/bin/env bash
# B2 resource bounds, exact: the maximum is accepted and lands in the cgroup, one
# more is refused, every spelling of "unlimited" becomes the default or is
# refused, partial updates are validated after merging, and the limits bite
# (pressure controls).
source "$(dirname "$0")/lib.sh"
S=${1:-base}
IMG=busybox:latest
GIB=$((1<<30))
MAXMEM=$((64*GIB)); DEFMEM=$((4*GIB))
dk $S docker pull -q $IMG >/dev/null
dk $S docker rm -f lim-max lim-def lim-up lim-q >/dev/null 2>&1
cg() { dk $S docker exec "$1" cat "/sys/fs/cgroup/$2" 2>&1 | tr '\n' ' ' | sed 's/ $//'; }
insp() { prod inspect "$1" --format "$2"; }

echo "== the maxima are accepted and are what the cgroup enforces"
# (32 cpus as quota/period: `--cpus 32` is refused by Docker itself on a VM with fewer cpus)
expect allowed "memory 64g, swap 64g, 32 cpus (quota/period), 65536 pids" -- dk $S docker run -d --name lim-max --memory $MAXMEM --memory-swap $MAXMEM --cpu-quota 3200000 --cpu-period 100000 --pids-limit 65536 $IMG sleep 300
want "memory.max at the maximum"  "$(cg lim-max memory.max)" "$MAXMEM"
want "pids.max at the maximum"    "$(cg lim-max pids.max)" 65536
want "cpu.max at 32 cpus"         "$(cg lim-max cpu.max)" "3200000 100000"
dk $S docker rm -f lim-max >/dev/null 2>&1

echo "== one past each maximum is refused"
expect refused "memory max+1 byte"        -- dk $S docker run --rm --name lim-q --memory $((MAXMEM+1)) $IMG true
expect refused "memory-swap max+1 byte"   -- dk $S docker run --rm --name lim-q --memory-swap $((MAXMEM+1)) $IMG true
expect refused "cpus 32.01"               -- dk $S docker run --rm --name lim-q --cpus 32.01 $IMG true
expect refused "pids-limit 65537"         -- dk $S docker run --rm --name lim-q --pids-limit 65537 $IMG true
expect refused "memory-reservation max+1" -- dk $S docker run --rm --name lim-q --memory-reservation $((MAXMEM+1)) $IMG true
expect refused "cpu-quota above 32 cpus"  -- dk $S docker run --rm --name lim-q --cpu-quota 3200001 --cpu-period 100000 $IMG true
expect refused "memory-swap -1 (unlimited swap) at create" -- dk $S docker run --rm --name lim-q --memory 256m --memory-swap -1 $IMG true

echo "== every spelling of unlimited gets the default"
dk $S docker run -d --name lim-def --pids-limit 0 --memory 0 --cpus 0 $IMG sleep 300 >/dev/null
want "pids-limit 0 -> default pids.max" "$(cg lim-def pids.max)" 4096
want "memory 0 -> default memory.max" "$(cg lim-def memory.max)" "$DEFMEM"
want "cpus 0 -> default cpu.max (4 cpus)" "$(cg lim-def cpu.max)" "400000 100000"
dk $S docker rm -f lim-def >/dev/null 2>&1
dk $S docker run -d --name lim-def --pids-limit -1 $IMG sleep 300 >/dev/null
want "pids-limit -1 -> default pids.max" "$(cg lim-def pids.max)" 4096
dk $S docker rm -f lim-def >/dev/null 2>&1

echo "== update: unlimited encodings are refused, partial updates are checked against what the container has"
dk $S docker run -d --name lim-up --cpu-quota 100000 --cpu-period 100000 --memory 256m $IMG sleep 300 >/dev/null
expect refused "update --pids-limit -1"  -- dk $S docker update --pids-limit -1 lim-up
# The CLI sends nothing for `--pids-limit 0` (unchanged); the raw API with an explicit 0 is the unlimited encoding.
dk $S docker update --pids-limit 0 lim-up >/dev/null 2>&1
want "update --pids-limit 0 from the CLI changes nothing" "$(insp lim-up '{{.HostConfig.PidsLimit}}')" 4096
for body in '{"PidsLimit":0}' '{"PidsLimit":-1}' '{"MemorySwap":-1}'; do
  code=$(sbx exec $SB curl -s -o /dev/null -w '%{http_code}' --unix-socket $(sock $S) -X POST -H 'Content-Type: application/json' -d "$body" http://d/v1.47/containers/lim-up/update)
  want "raw update $body is refused" "$code" 403
done
want "...and nothing changed" "$(insp lim-up '{{.HostConfig.PidsLimit}} {{.HostConfig.MemorySwap}}')" "4096 536870912"
expect refused "update --memory-swap -1" -- dk $S docker update --memory-swap -1 lim-up
expect refused "update --memory above the maximum" -- dk $S docker update --memory $((MAXMEM+1)) lim-up
expect refused "update --cpu-period 1000 (merged with the 100000 quota: 100 cpus)" -- dk $S docker update --cpu-period 1000 lim-up
want "the refused updates changed nothing" "$(insp lim-up '{{.HostConfig.CpuPeriod}} {{.HostConfig.CpuQuota}} {{.HostConfig.PidsLimit}}')" "100000 100000 4096"
expect allowed "update --cpu-period 10000 --cpu-quota 100000 (10 cpus)" -- dk $S docker update --cpu-period 10000 --cpu-quota 100000 lim-up
expect allowed "update --memory 512m --memory-swap 1g" -- dk $S docker update --memory 512m --memory-swap 1g lim-up
want "memory.max follows the update" "$(cg lim-up memory.max)" $((512<<20))
expect refused "update --restart always (not a resource)" -- dk $S docker update --restart always lim-up
want "restart policy unchanged" "$(insp lim-up '{{.HostConfig.RestartPolicy.Name}}')" no
dk $S docker rm -f lim-up >/dev/null 2>&1

echo "== the limits bite (pressure controls)"
# A fork storm in a detached exec, measured from outside: at the limit nothing in the
# container can fork (not even `cat`), and the storm's own shell dies on the failed fork,
# so the cgroup's process count is read from the engine container's side.
STORM='n=0; while [ $n -lt 5500 ]; do sleep 120 & n=$((n+1)); done'
storm() { # storm [docker run flags]  -> processes in the container's cgroup
  dk $S docker rm -f lim-q >/dev/null 2>&1
  dk $S docker run -d --name lim-q "$@" $IMG sleep 900 >/dev/null
  dk $S docker exec -d lim-q sh -c "$STORM" >/dev/null 2>&1
  local id f c last=-1
  id=$(prod inspect lim-q --format '{{.Id}}')
  f=$(vx sh -c "find /sys/fs/cgroup -name pids.current -path '*$id*' 2>/dev/null | head -1")
  c=0
  for i in $(seq 1 40); do sleep 1; c=$(vx cat $f 2>/dev/null); [ -n "$c" ] && [ "$c" = "$last" ] && break; last=$c; done
  echo "$c"
  dk $S docker rm -f lim-q >/dev/null 2>&1
}
cur=$(storm)
if [ -n "$cur" ] && [ "$cur" -le 4096 ] && [ "$cur" -ge 3500 ]; then pass "default pids limit: a fork storm stops at the limit ($cur processes)"; else fail "default pids limit: fork storm reached [$cur]"; fi
cur=$(storm --pids-limit 65536)
if [ -n "$cur" ] && [ "$cur" -ge 5400 ]; then pass "control: with the maximum the same storm runs past 4096 ($cur processes)"; else fail "control: the storm did not get past 4096 even with 65536 allowed [$cur]"; fi
dk $S docker run --rm --name lim-q --memory 64m --memory-swap 64m $IMG sh -c 'tail /dev/zero' >/dev/null 2>&1
want "a memory hog under --memory 64m is OOM-killed (exit 137)" "$?" 137
out=$(dk $S docker run --rm --name lim-q --memory 64m --memory-swap 64m $IMG sh -c 'head -c 4000000 /dev/zero | wc -c' 2>&1)
want "control: a small allocation under the same limit completes" "$out" 4000000
summary
