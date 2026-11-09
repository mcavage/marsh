#!/usr/bin/env bash
# B2: containers of different workspaces are on different networks. A listener in
# scope A is reachable from A (by name and by address) and not from B (control
# for the second half: break `scope-default-network-off` puts both on Docker's
# shared default bridge).
#   netsep.sh SCOPE_A SCOPE_B
source "$(dirname "$0")/lib.sh"
A=${1:-a}; B=${2:-b}
IMG=busybox:latest
dk $A docker pull -q $IMG >/dev/null; dk $B docker pull -q $IMG >/dev/null
dk $A docker rm -f ns-a1 ns-a2 >/dev/null 2>&1; dk $B docker rm -f ns-b1 >/dev/null 2>&1
dk $A docker run -d --name ns-a1 $IMG sh -c 'while :; do echo hello-from-a1 | nc -l -p 9000; done' >/dev/null
dk $B docker run -d --name ns-b1 $IMG sh -c 'while :; do echo hello-from-b1 | nc -l -p 9000; done' >/dev/null
sleep 1
IPA=$(prod inspect ns-a1 --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}')
IPB=$(prod inspect ns-b1 --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}')
NA=$(prod inspect ns-a1 --format '{{range $k,$v := .NetworkSettings.Networks}}{{$k}}{{end}}')
NB=$(prod inspect ns-b1 --format '{{range $k,$v := .NetworkSettings.Networks}}{{$k}}{{end}}')
echo "   a1=$IPA on $NA, b1=$IPB on $NB"
[ -n "$IPA" ] && [ -n "$IPB" ] && pass "both listeners have addresses" || fail "no address: a=[$IPA] b=[$IPB]"
if [ "$NA" != "$NB" ]; then pass "the two scopes' containers are on different networks"; else fail "both scopes' containers are on the same network ($NA)"; fi
dial() { dk $1 docker run --rm $IMG sh -c "nc -w 3 $2 9000 < /dev/null" 2>&1 | head -1; }
# Controls: the listener works, from its own scope, by address and by name.
want "control: scope A reaches its own listener by address" "$(dial $A $IPA)" hello-from-a1
want "control: scope A reaches its own listener by name" "$(dial $A ns-a1)" hello-from-a1
dk $A docker run -d --name ns-a2 $IMG sleep 300 >/dev/null
want "control: a second container of scope A reaches it too" "$(dk $A docker exec ns-a2 sh -c "nc -w 3 $IPA 9000 < /dev/null" 2>&1 | head -1)" hello-from-a1
# The separation itself.
got=$(dial $B $IPA)
[ "$got" = hello-from-a1 ] && fail "scope B reached scope A's listener at $IPA" || pass "scope B cannot reach scope A's listener by address (got [$got])"
got=$(dial $B ns-a1)
[ "$got" = hello-from-a1 ] && fail "scope B resolved and reached scope A's listener by name" || pass "scope B cannot reach scope A's listener by name (got [$got])"
got=$(dial $A $IPB)
[ "$got" = hello-from-b1 ] && fail "scope A reached scope B's listener" || pass "scope A cannot reach scope B's listener (got [$got])"
want "control: scope B still reaches its own" "$(dial $B $IPB)" hello-from-b1
dk $A docker rm -f ns-a1 ns-a2 >/dev/null 2>&1; dk $B docker rm -f ns-b1 >/dev/null 2>&1
summary
