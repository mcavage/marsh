#!/usr/bin/env bash
# A12 composition (task addendum L4): the engine egress firewall is complete only
# because E3 refuses host networking, non-bridge network drivers, privileged
# containers and dangerous capabilities. From a filtered-socket container:
#   1. each of those is refused;
#   2. the sbx proxy stays blocked: `curl -x gateway.docker.internal:3128
#      https://api.github.com/user` prints a status only (never the body or any
#      credential) and it must be 000 (no connection).
# The firewall rules themselves are S2b's (engine-up); this harness installs the
# same two rules per address family (container traffic in DOCKER-USER, the engine's
# own in OUTPUT) so that the composition can be exercised. A TCP-connect control
# before the rules are installed shows the probe can tell open from blocked; if the
# proxy cannot be reached even then, the run fails: it would prove nothing.
source "$(dirname "$0")/lib.sh"
S=${1:-base}
IMG=busybox:latest
CURLIMG=curlimages/curl:latest
dk $S docker pull -q $IMG >/dev/null; dk $S docker pull -q $CURLIMG >/dev/null
GW4=$(vx getent ahostsv4 gateway.docker.internal 2>/dev/null | awk 'NR==1{print $1}')
GW6=$(vx getent ahostsv6 gateway.docker.internal 2>/dev/null | awk 'NR==1{print $1}')
echo "   gateway.docker.internal -> v4=${GW4:-none} v6=${GW6:-none}"
RULES=()   # "tool|chain|addr"
add_rule() { RULES+=("$1|$2|$3"); }
[ -n "$GW4" ] && { add_rule iptables DOCKER-USER $GW4; add_rule iptables OUTPUT $GW4; }
[ -n "$GW6" ] && { add_rule ip6tables DOCKER-USER $GW6; add_rule ip6tables OUTPUT $GW6; }
rule_do() { # rule_do -I|-D "tool|chain|addr"
  local op=$1 t=${2%%|*} r=${2#*|}; local chain=${r%%|*} addr=${r##*|}
  vx $t $op $chain -p tcp -d $addr ! --dport 53 -j DROP 2>/dev/null
}
cleanup() { for r in "${RULES[@]}"; do rule_do -D "$r"; done; dk $S docker network rm evil-a12-mv evil-a12-ip >/dev/null 2>&1; }
trap cleanup EXIT
if [ ${#RULES[@]} -eq 0 ]; then fail "gateway.docker.internal does not resolve in the engine container: the proxy probe cannot run"; summary; exit; fi

echo "== 1. refusals the firewall depends on"
expect refused "--network host"                -- dk $S docker run --rm --name evil-a12-1 --network host $IMG true
expect refused "network create -d macvlan"     -- dk $S docker network create -d macvlan -o parent=eth0 evil-a12-mv
expect refused "network create -d ipvlan"      -- dk $S docker network create -d ipvlan -o parent=eth0 evil-a12-ip
expect refused "network create -d host"        -- dk $S docker network create -d host evil-a12-host
expect refused "--privileged"                  -- dk $S docker run --rm --name evil-a12-2 --privileged $IMG true
expect refused "--cap-add SYS_ADMIN"           -- dk $S docker run --rm --name evil-a12-3 --cap-add SYS_ADMIN $IMG true
expect refused "--cap-add ALL"                 -- dk $S docker run --rm --name evil-a12-4 --cap-add ALL $IMG true
expect refused "--network bridge (the built-in default)" -- dk $S docker run --rm --name evil-a12-5 --network bridge $IMG true
expect refused "--pid host"                    -- dk $S docker run --rm --name evil-a12-6 --pid host $IMG true
no_evil

echo "== 2. the proxy stays blocked"
for r in "${RULES[@]}"; do rule_do -D "$r"; done
eng() { vx sh -c "nc -z -w 3 $1 3128 && echo open || echo closed" 2>&1 | tail -1; }
cont() { dk $S docker run --rm $IMG sh -c "nc -z -w 3 $1 3128 && echo open || echo closed" 2>&1 | tail -1; }
OPEN=0
[ -n "$GW4" ] && [ "$(eng $GW4)" = open ] && OPEN=1
[ -n "$GW6" ] && [ "$(eng $GW6)" = open ] && OPEN=1
[ $OPEN -eq 1 ] && pass "control: with no firewall rule the proxy port is reachable (from the engine container)" || fail "control: the proxy port is not reachable even without a rule (v4=${GW4:-none} v6=${GW6:-none}): this probe cannot show the rules matter"
[ -n "$GW4" ] && echo "   (a filtered-socket container's own view of the v4 gateway before the rules: $(cont $GW4))"
for r in "${RULES[@]}"; do rule_do -I "$r" && pass "firewall rule installed: $r" || fail "could not install $r"; done
[ -n "$GW4" ] && want "with the rules the proxy port is closed to the engine container (v4)" "$(eng $GW4)" closed
[ -n "$GW6" ] && want "with the rules the proxy port is closed to the engine container (v6)" "$(eng $GW6)" closed
[ -n "$GW4" ] && want "with the rules the proxy port is closed to a filtered-socket container (v4)" "$(cont $GW4)" closed
st=$(dk $S docker run --rm $CURLIMG -s -o /dev/null -w '%{http_code}' --max-time 8 -x gateway.docker.internal:3128 https://api.github.com/user 2>/dev/null | tail -1)
want "curl -x gateway.docker.internal:3128 https://api.github.com/user: status only" "$st" 000
if [ -n "$GW4" ]; then
  st=$(dk $S docker run --rm $CURLIMG -s -o /dev/null -w '%{http_code}' --max-time 8 -x $GW4:3128 https://api.github.com/user 2>/dev/null | tail -1)
  want "the same by IPv4 address" "$st" 000
fi
if [ -n "$GW6" ]; then
  st=$(dk $S docker run --rm $CURLIMG -s -o /dev/null -w '%{http_code}' --max-time 8 -x "[$GW6]:3128" https://api.github.com/user 2>/dev/null | tail -1)
  want "the same by IPv6 address" "$st" 000
fi
cleanup
summary
