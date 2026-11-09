# Helpers for E3 sessions (source me). Runs on the Mac; talks to sandbox $SB.
SB=${SB:-nfx-e3-a}
PROJ=${PROJ:-/private/tmp/nfx-e3/proj}
OUT=${OUT:-/private/var/folders/zr/1nbpv_812d17xnm1kdwwvq1m0000gn/T/opencode/e3-out}
# Run in the engine container (root).
ex() { sbx exec "$SB" docker exec "$@"; }
vx() { sbx exec "$SB" docker exec nfx-e3-engine "$@"; }
# The engine's own socket: ground truth, never the filter.
prod() { vx docker -H unix:///var/run/docker.sock "$@"; }
# Add a scope (workspace) served on /run/s-NAME.sock.
scope() { # scope NAME WORKSPACE ATTEMPT [ROOT]
  local root=${4:-$PROJ}
  vx sh -c "echo '{\"listen\":\"/var/lib/nfx-e3/socks/s-$1.sock\",\"workspace\":\"$2\",\"attempt\":\"$3\",\"root\":\"$root\"}' > /pool/marsh/scopes/$1.scope"
  for i in $(seq 1 40); do vx test -S /var/lib/nfx-e3/socks/s-$1.sock && return 0; sleep 0.25; done; echo "scope $1 did not appear" >&2; return 1
}
# Docker client through a scope's filtered socket.
dk() { local s=$1; shift; sbx exec "$SB" env DOCKER_HOST=unix:///var/lib/nfx-e3/socks/s-$s.sock "$@"; }
# Same, but in the engine container (an "internal" client).
dki() { local s=$1; shift; vx env DOCKER_HOST=unix:///var/lib/nfx-e3/socks/s-$s.sock "$@"; }
sock() { echo /var/lib/nfx-e3/socks/s-$1.sock; }

# --- assertions ---------------------------------------------------------------
PASS=0; FAIL=0
# expect refused|allowed LABEL -- cmd...   (cmd runs in the VM through dk)
expect() {
  local want=$1 label=$2; shift 3
  local out rc got
  out=$("$@" 2>&1); rc=$?
  if echo "$out" | grep -qE "marsh:|not allowed by build daemon"; then got=refused; elif [ $rc -eq 0 ]; then got=allowed; else got=error; fi
  if [ "$got" = "$want" ]; then PASS=$((PASS+1)); s=ok; else FAIL=$((FAIL+1)); s=BAD; fi
  printf '%-4s %-8s %-52s %s\n' "$s" "$got" "$label" "$(echo "$out" | grep -oE 'marsh:.*' | head -1 | cut -c1-90)"
  if [ "$s" = BAD ]; then echo "$out" | head -3 | sed 's/^/       /'; fi
  return 0
}
# Ground truth: nothing named evil-* exists in the production engine.
no_evil() {
  local n; n=$(prod ps -a --format '{{.Names}}' | grep -c '^evil-' || true)
  if [ "$n" = 0 ]; then echo "ok   no evil-* object in the production engine"; PASS=$((PASS+1)); else echo "BAD  $n evil-* containers exist:"; prod ps -a --format '{{.Names}} {{.Image}}' | grep '^evil-'; FAIL=$((FAIL+1)); fi
}
pass() { PASS=$((PASS+1)); echo "ok   $*"; }
fail() { FAIL=$((FAIL+1)); echo "BAD  $*"; }
# check LABEL -- cmd...   passes when cmd exits 0 (output discarded)
check() { local l=$1; shift 2; if "$@" >/dev/null 2>&1; then pass "$l"; else fail "$l"; fi; }
# nocheck LABEL -- cmd...  passes when cmd exits nonzero
nocheck() { local l=$1; shift 2; if "$@" >/dev/null 2>&1; then fail "$l"; else pass "$l"; fi; }
# want LABEL GOT EXPECTED
want() { if [ "$2" = "$3" ]; then pass "$1 ($2)"; else fail "$1: got [$2], wanted [$3]"; fi; }
# The exit status of the script is the verdict: nonzero when any check failed.
# A script that checked nothing has proved nothing.
summary() { echo "== pass=$PASS fail=$FAIL"; [ "$FAIL" -eq 0 ] && [ "$PASS" -gt 0 ]; }
