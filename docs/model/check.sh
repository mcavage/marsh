#!/usr/bin/env bash
# Run TLC on every config. Positive configs (Module.cfg, Module_*_pass.cfg) must pass; each
# negative control (Module_<Bug>.cfg) must fail with its expected invariant.
# Exits nonzero on any mismatch. Runs TLC in Docker by default; TLC_RUNNER=java
# runs a local `java` instead (CI). The TLA+ jar lives outside the repository
# and is never committed.
set -u
cd "$(dirname "$0")"
JAR="${TLA2TOOLS_JAR:-/private/tmp/marsh-tla2tools-1.8.0.jar}"
export DOCKER_HOST="${DOCKER_HOST:-unix://$HOME/.docker/run/docker.sock}"
IMAGE="${TLC_IMAGE:-eclipse-temurin:21-jre}"
[ -f "$JAR" ] || { echo "missing $JAR (set TLA2TOOLS_JAR)" >&2; exit 2; }
OUT="$(mktemp -d)"

tlc() { # cfg module
  if [ "${TLC_RUNNER:-docker}" = java ]; then
    java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -noGenerateSpecTE -deadlock -workers "${TLC_WORKERS:-auto}" \
      -metadir "$OUT/meta-$1" -config "$1" "$2"
    return
  fi
  docker run --rm -v "$PWD:/m" -v "$JAR:/tla2tools.jar:ro" -w /m "$IMAGE" \
    java -XX:+UseParallelGC -cp /tla2tools.jar tlc2.TLC -noGenerateSpecTE -deadlock -workers "${TLC_WORKERS:-auto}" \
    -metadir "/tmp/tlc-$1" -config "$1" "$2"
}

status=0
for cfg in ${1:-*.cfg}; do
  base="${cfg%.cfg}"; module="${base%%_*}.tla"
  log="$OUT/$base.log"
  start=$(date +%s); tlc "$cfg" "$module" >"$log" 2>&1; rc=$?; secs=$(( $(date +%s) - start ))
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
  printf '%-36s expect=%-22s %-4s states=%-10s %ss\n' "$cfg" "$expected" "$verdict" "${states:-?}" "$secs"
  if [ "$verdict" != ok ]; then status=1; tail -30 "$log" | sed 's/^/    /'; fi
done
echo "logs: $OUT"
exit $status
