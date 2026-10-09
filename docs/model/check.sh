#!/usr/bin/env bash
# Run TLC on every config. Positive configs (Module.cfg, Module_*_pass.cfg) must pass; each
# negative control (Module_<Bug>.cfg) must fail with its expected invariant.
# Exits nonzero on any mismatch. Runs TLC in Docker by default; TLC_RUNNER=java
# runs a local `java` instead (CI). The TLA+ jar lives outside the repository
# and is never committed.
#
#   check.sh [--quick | --huge | --no-huge] [--list] [--cores N] [--workers N] [CONFIG...]
#
# CONFIG is a config name, glob, or a quoted list of them (default: every config).
# Configs run concurrently inside a core budget; every config is a separate TLC
# process (a separate container under Docker) with its own log, and the verdict
# of each is independent of scheduling. A comment line in a config
#   \* marsh-check: WORD
# is a scheduling hint, measured with two workers: `mid` (20-60 s), `slow` (over
# 60 s; skipped by --quick), `huge` (also slow; tens of millions of states, which
# does not fit one CI job with its siblings), optionally followed by the measured
# seconds as `426s`. Hinted configs start first, longest first, with two workers;
# the rest run with one. Hints never change a verdict.
#
#   --quick     skip every slow config (the fast static gate; the full sweep is the default)
#   --huge      only the huge configs   --no-huge   everything except them (CI shards)
#   --list      print the selected configs, with workers, and exit
#   --cores N   core budget (TLC_CORES; default min(CPUs of the runner, 8))
#   TLC_CACHE=DIR  skip a config whose spec, config, jar and this script are byte-identical to
#               one that already gave its expected verdict (CI keeps DIR between runs; the
#               release run `make regress` does not set it)
#   --workers N TLC workers per config (TLC_WORKERS; default 1, 2 for hinted, half the
#               budget for huge, shared evenly when few configs; "auto" = the default)
set -u
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
case "${TLC_CACHE:-}" in ''|/*) ;; *) TLC_CACHE="$PWD/$TLC_CACHE" ;; esac
cd "$(dirname "$0")"
JAR="${TLA2TOOLS_JAR:-/private/tmp/marsh-tla2tools-1.8.0.jar}"
export DOCKER_HOST="${DOCKER_HOST:-unix://$HOME/.docker/run/docker.sock}"
IMAGE="${TLC_IMAGE:-eclipse-temurin:21-jre}"
RUNNER="${TLC_RUNNER:-docker}"
JAVA_OPTS="${TLC_JAVA_OPTS:--XX:+UseParallelGC}"
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi | cut -d' ' -f1; }

# One config, run by the scheduler below (or by hand): prints one result line.
if [ "${1:-}" = --one ]; then
  cfg=$2; workers=$3; OUT=$4
  base="${cfg%.cfg}"; module="${base%%_*}.tla"; log="$OUT/$base.log"
  # TLC_CACHE: a config whose spec, config, TLA+ tools jar and this script are
  # byte-identical to one that already gave its expected verdict is not re-run.
  if [ -n "${TLC_CACHE:-}" ]; then
    key=$( { echo "$TLC_JAR_SHA"; cat "$module" "$cfg" "$SELF"; } | sha256)
    if [ -f "$TLC_CACHE/$key" ]; then
      printf '%s cached\n' "$(cat "$TLC_CACHE/$key")"; echo ok >"$OUT/$base.verdict"; exit 0
    fi
  fi
  start=$(date +%s)
  if [ "$RUNNER" = java ]; then
    java $JAVA_OPTS -cp "$JAR" tlc2.TLC -noGenerateSpecTE -deadlock -workers "$workers" \
      -metadir "$OUT/meta-$base" -config "$cfg" "$module" >"$log" 2>&1
  else
    docker run --rm --label "marsh-tlc=$OUT" --cpus "$workers" -v "$PWD:/m" -v "$JAR:/tla2tools.jar:ro" -w /m "$IMAGE" \
      java $JAVA_OPTS -cp /tla2tools.jar tlc2.TLC -noGenerateSpecTE -deadlock -workers "$workers" \
      -metadir "/tmp/tlc-$base" -config "$cfg" "$module" >"$log" 2>&1
  fi
  rc=$?; secs=$(( $(date +%s) - start ))
  states=$(grep -Eo '[0-9,]+ distinct states found' "$log" | tail -1 | cut -d' ' -f1)
  if [ "$base" = "${base%%_*}" ] || [ "${base%_pass}" != "$base" ]; then
    expected="pass"
    if [ $rc -eq 0 ] && grep -q "No error has been found" "$log"; then verdict=ok; else verdict=FAIL; fi
  else
    expected=$(awk '/^(INVARIANTS|PROPERTIES)/{getline; print $1; exit}' "$cfg")
    if grep -q '^INVARIANTS' "$cfg"; then pat="Invariant $expected is violated"
    else pat="Temporal property $expected was violated"; fi
    if [ $rc -ne 0 ] && grep -q "$pat" "$log"; then verdict=ok; else verdict=FAIL; fi
  fi
  line=$(printf '%-36s expect=%-22s %-4s states=%-10s %ss w=%s' "$cfg" "$expected" "$verdict" "${states:-?}" "$secs" "$workers")
  echo "$line"
  echo "$verdict" >"$OUT/$base.verdict"
  if [ "$verdict" = ok ] && [ -n "${TLC_CACHE:-}" ]; then
    mkdir -p "$TLC_CACHE" && echo "$line" >"$TLC_CACHE/$key"
  fi
  exit 0
fi

mode=all; list=0; cores="${TLC_CORES:-}"; workers="${TLC_WORKERS:-}"; args=""
while [ $# -gt 0 ]; do
  case "$1" in
    --quick) mode=quick ;;
    --huge) mode=huge ;;
    --no-huge) mode=nohuge ;;
    --list) list=1 ;;
    --cores) cores=$2; shift ;;
    --workers) workers=$2; shift ;;
    -h|--help) sed -n '2,/^set -u/p' "$SELF" | sed '$d;s/^# \{0,1\}//'; exit 0 ;;
    -*) echo "check.sh: unknown option $1" >&2; exit 2 ;;
    *) args="$args $1" ;;
  esac
  shift
done
case "$workers" in ''|auto) workers= ;; *[!0-9]*) echo "check.sh: --workers must be a number or auto" >&2; exit 2 ;; esac
[ "$list" = 1 ] || [ -f "$JAR" ] || { echo "missing $JAR (set TLA2TOOLS_JAR)" >&2; exit 2; }
[ -z "${TLC_CACHE:-}" ] || { TLC_JAR_SHA=$(sha256 <"$JAR"); export TLC_CACHE TLC_JAR_SHA; }

if [ -z "$cores" ]; then
  if [ "$RUNNER" = java ]; then
    detected=$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 2)
  else
    detected=$(docker info --format '{{.NCPU}}' 2>/dev/null || echo 2)
  fi
  case "$detected" in ''|*[!0-9]*) detected=2 ;; esac
  cores=$detected; [ "$cores" -gt 8 ] && cores=8
fi
case "$cores" in ''|*[!0-9]*|0) echo "check.sh: cores must be a positive number" >&2; exit 2 ;; esac

marker() { grep -Eq "^\\\\\\* marsh-check:.*[[:space:]]$2([[:space:]]|\$)" "$1"; } # cfg word
hint() { sed -n 's/^\\\* marsh-check:.* \([0-9][0-9]*\)s\( .*\)\{0,1\}$/\1/p' "$1" | head -1; } # measured seconds, if any
by_hint() { # longest hinted config first
  for c in "$@"; do printf '%s %s\n' "$(hint "$c" | sed 's/^$/0/')" "$c"; done | sort -k1,1nr -k2,2 | cut -d' ' -f2
}
# shellcheck disable=SC2086
set -- ${args:-*.cfg}
huge=(); slow=(); mid=(); rest=()
for cfg in "$@"; do
  [ -f "$cfg" ] || { echo "check.sh: no such config $cfg" >&2; exit 2; }
  if marker "$cfg" huge; then
    [ $mode = quick ] || [ $mode = nohuge ] || huge[${#huge[@]}]=$cfg
  elif marker "$cfg" slow; then
    [ $mode = quick ] || [ $mode = huge ] || slow[${#slow[@]}]=$cfg
  elif marker "$cfg" mid; then
    [ $mode = huge ] || mid[${#mid[@]}]=$cfg
  else
    [ $mode = huge ] || rest[${#rest[@]}]=$cfg
  fi
done
selected=(${huge[@]+"${huge[@]}"})
while read -r c; do [ -n "$c" ] && selected[${#selected[@]}]=$c; done <<LIST
$(by_hint ${slow[@]+"${slow[@]}"})
$(by_hint ${mid[@]+"${mid[@]}"})
LIST
selected=(${selected[@]+"${selected[@]}"} ${rest[@]+"${rest[@]}"})
n=${#selected[@]}
[ "$n" -gt 0 ] || { echo "check.sh: no configs selected" >&2; exit 2; }

# Workers: an explicit count is used as given. Otherwise one per config (most are
# done in a second and extra workers only cost startup), two for a hinted config, half
# the budget for a huge one (the long pole: tens of millions of states) so the rest
# of the sweep runs beside it, and everything still free for the last config to start.
# A selection too small to fill the budget (a CI shard) shares it evenly.
if [ -n "$workers" ]; then base_w=$workers; slow_w=$workers; huge_w=$workers; else
  base_w=1; slow_w=2; huge_w=2
  if [ "$n" -lt "$cores" ]; then base_w=$(( cores / n )); slow_w=$base_w; huge_w=$base_w; fi
  [ ${#huge[@]} -gt 0 ] && [ $(( cores / 2 / ${#huge[@]} )) -gt "$huge_w" ] && huge_w=$(( cores / 2 / ${#huge[@]} ))
  [ "$slow_w" -ge "$base_w" ] || slow_w=$base_w
fi
wfor() { if marker "$1" huge; then echo "$huge_w"; elif marker "$1" slow || marker "$1" mid; then echo "$slow_w"; else echo "$base_w"; fi; }

if [ "$list" = 1 ]; then
  for cfg in "${selected[@]}"; do printf '%-36s workers=%s\n' "$cfg" "$(wfor "$cfg")"; done
  echo "cores=$cores configs=$n"
  exit 0
fi

OUT="$(mktemp -d)"
pids=(); pidw=(); used=0
cleanup() {
  for p in ${pids[@]+"${pids[@]}"}; do kill "$p" 2>/dev/null; done
  if [ "$RUNNER" != java ]; then
    ids=$(docker ps -q --filter "label=marsh-tlc=$OUT" 2>/dev/null)
    [ -z "$ids" ] || docker kill $ids >/dev/null 2>&1
  fi
}
trap 'cleanup; exit 130' INT TERM

reap() { # drop finished jobs from the pool
  local np=() nw=() i
  used=0
  for (( i = 0; i < ${#pids[@]}; i++ )); do
    if kill -0 "${pids[$i]}" 2>/dev/null; then np[${#np[@]}]=${pids[$i]}; nw[${#nw[@]}]=${pidw[$i]}; used=$(( used + pidw[i] ))
    else wait "${pids[$i]}" 2>/dev/null; fi
  done
  pids=(${np[@]+"${np[@]}"}); pidw=(${nw[@]+"${nw[@]}"})
}

wall_start=$(date +%s)
last=$(( n - 1 )); index=0
for cfg in "${selected[@]}"; do
  w=$(wfor "$cfg")
  while :; do
    reap
    [ "${#pids[@]}" -eq 0 ] && break
    [ $(( used + w )) -le "$cores" ] && break
    sleep 0.2
  done
  if [ "$index" -eq "$last" ] && [ -z "$workers" ] && [ $(( cores - used )) -gt "$w" ]; then w=$(( cores - used )); fi
  index=$(( index + 1 ))
  "$SELF" --one "$cfg" "$w" "$OUT" &
  pids[${#pids[@]}]=$!; pidw[${#pidw[@]}]=$w
done
while reap; [ "${#pids[@]}" -gt 0 ]; do sleep 0.2; done
wall=$(( $(date +%s) - wall_start ))

status=0; ok=0
for cfg in "${selected[@]}"; do
  base="${cfg%.cfg}"
  if [ "$(cat "$OUT/$base.verdict" 2>/dev/null)" = ok ]; then ok=$(( ok + 1 )); else
    status=1; echo "FAIL $cfg"; tail -30 "$OUT/$base.log" 2>/dev/null | sed 's/^/    /'
  fi
done
echo "$ok/$n configs ok in ${wall}s (budget $cores cores, mode $mode)"
echo "logs: $OUT"
exit $status
