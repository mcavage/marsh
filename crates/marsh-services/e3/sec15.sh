#!/usr/bin/env bash
# SEC-15: substituted Docker sockets (A3.1), for the scopes named on the command
# line, e.g. `sec15.sh base a`. Needs the extra scope `base2` (workspace base,
# attempt a2) from session.sh `up`.
#
#  - the three spellings (/var/run/docker.sock, /run/docker.sock, the job
#    endpoint alias /run/marsh/docker.sock) are substituted, never the real socket
#  - live foreign objects exist in every scope before the inside view is looked at,
#    and the inside view lists the scope's own object (a control) but never another's
#  - the authority behind a substituted socket is the creating workspace AND attempt
#  - the raw framing/casing matrix (rawdocker.py) runs inside a socket holder and
#    inside a holder started by that holder
source "$(dirname "$0")/lib.sh"
CLI=docker:29-cli
PY=python:3-alpine
P=$PROJ
RAWRUN='python3 /r.py /var/run/docker.sock --no-truth'
for S in "$@"; do dk $S docker pull -q $CLI >/dev/null; dk $S docker pull -q busybox >/dev/null; dk $S docker pull -q $PY >/dev/null; done

echo "== seed live foreign objects in every scope (so that 'not visible' cannot mean 'not there')"
for S in "$@"; do
  dk $S docker rm -f sf-$S >/dev/null 2>&1; dk $S docker volume rm sf-vol-$S >/dev/null 2>&1; dk $S docker network rm sf-net-$S >/dev/null 2>&1
  dk $S docker volume create sf-vol-$S >/dev/null
  dk $S docker network create sf-net-$S >/dev/null
  dk $S docker run -d --name sf-$S -v sf-vol-$S:/d busybox sleep 900 >/dev/null
  prod inspect sf-$S >/dev/null 2>&1 && pass "seeded sf-$S in the engine" || fail "could not seed sf-$S"
done

for S in "$@"; do
echo "== scope $S"
for host in /var/run/docker.sock /run/docker.sock /run/marsh/docker.sock; do
  n=$(echo $host | tr / _)
  cn=sock$S$n
  dk $S docker rm -f $cn >/dev/null 2>&1
  expect allowed "docker.sock spelling $host accepted" -- dk $S docker run -d --name $cn -v $host:/var/run/docker.sock $CLI sleep 600
  src=$(prod inspect $cn --format '{{range .Mounts}}{{.Source}}{{end}}')
  case "$src" in /run/marsh/sockets/$S/*/docker.sock) pass "c3: mount source for $host is the scope's substituted socket ($src)";; *) fail "c3: mount source for $host is $src";; esac
  [ "$src" = /var/run/docker.sock ] && fail "the production engine's socket was mounted ($host)"
  inx() { dk $S docker exec $cn "$@"; }
  expect allowed "inside: docker ps (own scope)" -- inx docker ps
  expect allowed "inside: docker run busybox true" -- inx docker run --rm busybox true
  expect refused "inside: --privileged" -- inx docker run --rm --name evil-in1 --privileged busybox true
  expect refused "inside: -v /:/h" -- inx docker run --rm --name evil-in2 -v /:/h busybox true
  expect refused "inside: --network host" -- inx docker run --rm --name evil-in3 --network host busybox true
  expect refused "inside: --cap-add SYS_ADMIN" -- inx docker run --rm --name evil-in4 --cap-add SYS_ADMIN busybox true
  out=$(inx docker ps -a --format '{{.Names}}' 2>&1)
  echo "$out" | grep -qx "sf-$S" && pass "c1 control: inside sees this scope's own seeded container" || fail "c1 control: inside does not see sf-$S: $(echo $out | tr '\n' ' ')"
  for O in "$@"; do
    [ "$O" = "$S" ] && continue
    echo "$out" | grep -qx "sf-$O" && fail "c1: inside $S sees scope $O's container sf-$O" || pass "c1: inside $S does not see scope $O's container"
    inx docker volume ls -q | grep -qx "sf-vol-$O" && fail "c1: inside $S sees $O's volume" || pass "c1: inside $S does not see $O's volume"
    inx docker network ls --format '{{.Name}}' | grep -qx "sf-net-$O" && fail "c1: inside $S sees $O's network" || pass "c1: inside $S does not see $O's network"
    expect refused "c1: inside $S: inspect $O's container" -- inx docker inspect sf-$O
    expect refused "c1: inside $S: stop $O's container" -- inx docker stop sf-$O
  done
  want "c1: foreign containers are untouched" "$(prod inspect sf-$S --format '{{.State.Status}}')" running
  # nested: the inner container holds the socket again and repeats the attacks
  expect allowed "nested: container with the socket" -- inx docker run -d --name nested$n -v /var/run/docker.sock:/var/run/docker.sock $CLI sleep 300
  expect refused "nested: --privileged" -- inx docker exec nested$n docker run --rm --name evil-in5 --privileged busybox true
  expect refused "nested: -v /:/h" -- inx docker exec nested$n docker run --rm --name evil-in6 -v /:/h busybox true
  nsrc=$(prod inspect nested$n --format '{{range .Mounts}}{{.Source}}{{end}}')
  case "$nsrc" in /run/marsh/sockets/$S/*) pass "c1: nested mount is substituted too ($nsrc)";; *) fail "nested source $nsrc";; esac
  dk $S docker rm -f nested$n $cn >/dev/null 2>&1
done

echo "== scope $S: the raw framing/casing matrix inside a socket holder, and inside a holder's holder"
raw_total() { grep -oE '"total": [0-9]+' | head -1 | grep -oE '[0-9]+'; }
dk $S docker rm -f rawholder >/dev/null 2>&1
dk $S docker run -d --name rawholder -v /var/run/docker.sock:/var/run/docker.sock -v $P/.e3/rawdocker.py:/r.py:ro $PY sleep 900 >/dev/null
out=$(dk $S docker exec rawholder sh -c "$RAWRUN" 2>&1); rc=$?
tot=$(echo "$out" | raw_total); echo "$out" | grep -E '^FAIL' | head -5
[ $rc -eq 0 ] && [ "${tot:-0}" -ge 100 ] && pass "raw matrix inside the holder: $tot cases, none failed, controls healthy" || fail "raw matrix inside the holder: rc=$rc total=${tot:-0}: $(echo "$out" | tail -2 | cut -c1-200)"
dk $S docker rm -f rawholder >/dev/null 2>&1
dk $S docker rm -f rawouter >/dev/null 2>&1
dk $S docker run -d --name rawouter -v /var/run/docker.sock:/var/run/docker.sock $CLI sleep 900 >/dev/null
out=$(dk $S docker exec rawouter docker run --rm -v /var/run/docker.sock:/var/run/docker.sock -v $P/.e3/rawdocker.py:/r.py:ro $PY sh -c "$RAWRUN" 2>&1); rc=$?
tot=$(echo "$out" | raw_total); echo "$out" | grep -E '^FAIL' | head -5
[ $rc -eq 0 ] && [ "${tot:-0}" -ge 100 ] && pass "raw matrix inside a nested holder: $tot cases, none failed, controls healthy" || fail "raw matrix inside a nested holder: rc=$rc total=${tot:-0}: $(echo "$out" | tail -2 | cut -c1-200)"
dk $S docker rm -f rawouter >/dev/null 2>&1
done

echo "== the authority is the creating workspace AND attempt"
if vx test -S $(sock base2); then
  for sc in base:a1 base2:a2; do
    sk=${sc%%:*}; att=${sc##*:}
    dk $sk docker rm -f attauth-$att attchild-$att >/dev/null 2>&1
    dk $sk docker run -d --name attauth-$att -v /var/run/docker.sock:/var/run/docker.sock $CLI sleep 300 >/dev/null
    src=$(prod inspect attauth-$att --format '{{range .Mounts}}{{.Source}}{{end}}')
    case "$src" in /run/marsh/sockets/base/$att/docker.sock) pass "the socket handed to attempt $att is attempt $att's ($src)";; *) fail "attempt $att got $src";; esac
    dk $sk docker exec attauth-$att docker run -d --name attchild-$att busybox sleep 60 >/dev/null 2>&1
    want "an object made through attempt $att's socket carries attempt $att" "$(prod inspect attchild-$att --format '{{index .Config.Labels "dev.marsh.attempt"}}' 2>/dev/null)" $att
    want "...and the workspace" "$(prod inspect attchild-$att --format '{{index .Config.Labels "dev.marsh.scope"}}' 2>/dev/null)" base
    dk $sk docker rm -f attauth-$att attchild-$att >/dev/null 2>&1
  done
else
  fail "scope base2 (workspace base, attempt a2) is not served; session.sh up creates it"
fi
for S in "$@"; do
  dk $S docker rm -f sf-$S >/dev/null 2>&1; dk $S docker volume rm sf-vol-$S >/dev/null 2>&1; dk $S docker network rm sf-net-$S >/dev/null 2>&1
done
no_evil
summary
