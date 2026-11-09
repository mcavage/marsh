#!/usr/bin/env bash
# Volume ownership (B2/A4): two scopes race for one name; anonymous and
# image-declared volumes carry their creator's scope.
#   volumes.sh SCOPE_A SCOPE_B
source "$(dirname "$0")/lib.sh"
A=${1:-a}; B=${2:-b}
IMG=busybox:latest
ROUNDS=${ROUNDS:-12}
T=$(mktemp -d)
dk $A docker pull -q $IMG >/dev/null; dk $B docker pull -q $IMG >/dev/null
owner() { prod volume inspect "$1" --format '{{index .Labels "dev.marsh.scope"}}' 2>/dev/null; }
sees() { dk $1 docker volume ls -q 2>/dev/null | grep -qx "$2"; }

echo "== explicit: both scopes create the same name at once ($ROUNDS rounds)"
two=0; none=0; split=0
for r in $(seq 1 $ROUNDS); do
  n=race-ex-$r
  ( dk $A docker volume create $n >/dev/null 2>&1; echo $? > $T/a ) &
  ( dk $B docker volume create $n >/dev/null 2>&1; echo $? > $T/b ) &
  wait
  ok=$(( $(cat $T/a) == 0 )); ok=$(( ok + ($(cat $T/b) == 0) ))
  [ $ok -eq 2 ] && two=$((two+1))
  [ $ok -eq 0 ] && none=$((none+1))
  o=$(owner $n)
  # exactly the winner sees it, and the engine's label agrees with the ledger
  sa=0; sb=0; sees $A $n && sa=1; sees $B $n && sb=1
  [ $((sa+sb)) -ne 1 ] && split=$((split+1))
  if [ "$o" = "$A" ] && [ $sa -ne 1 ]; then split=$((split+1)); fi
  if [ "$o" = "$B" ] && [ $sb -ne 1 ]; then split=$((split+1)); fi
  prod volume rm $n >/dev/null 2>&1
  dk $A docker volume rm $n >/dev/null 2>&1; dk $B docker volume rm $n >/dev/null 2>&1
done
want "explicit create: rounds in which both scopes were told they own the name" $two 0
want "explicit create: rounds in which neither was" $none 0
want "explicit create: rounds in which the ledger and the engine's label disagree, or both/none see it" $split 0

echo "== implicit: both scopes run a container with -v NAME:/d at once, each writing its own sentinel"
bothrun=0; wrongdata=0
for r in $(seq 1 $ROUNDS); do
  n=race-im-$r
  ( dk $A docker run --rm -v $n:/d $IMG sh -c "echo $A > /d/who; sleep 1; cat /d/who" >$T/oa 2>&1; echo $? > $T/a ) &
  ( dk $B docker run --rm -v $n:/d $IMG sh -c "echo $B > /d/who; sleep 1; cat /d/who" >$T/ob 2>&1; echo $? > $T/b ) &
  wait
  ra=$(cat $T/a); rb=$(cat $T/b)
  [ "$ra" = 0 ] && [ "$rb" = 0 ] && bothrun=$((bothrun+1))
  # whoever ran must have read back its own sentinel: nobody wrote into the other's volume
  [ "$ra" = 0 ] && [ "$(tail -1 $T/oa)" != "$A" ] && wrongdata=$((wrongdata+1))
  [ "$rb" = 0 ] && [ "$(tail -1 $T/ob)" != "$B" ] && wrongdata=$((wrongdata+1))
  o=$(owner $n); [ "$o" = "$A" ] || [ "$o" = "$B" ] || wrongdata=$((wrongdata+1))
  prod volume rm $n >/dev/null 2>&1
  dk $A docker volume rm $n >/dev/null 2>&1; dk $B docker volume rm $n >/dev/null 2>&1
done
want "implicit create: rounds in which both scopes mounted the same volume" $bothrun 0
want "implicit create: rounds in which a scope read another's data or the volume has no owner" $wrongdata 0

echo "== a name another scope owns stays refused, whichever way it is reached"
dk $A docker volume create own-by-a >/dev/null
expect refused "b: docker volume create own-by-a" -- dk $B docker volume create own-by-a
expect refused "b: docker run -v own-by-a:/d" -- dk $B docker run --rm -v own-by-a:/d $IMG true
expect refused "b: docker run --mount type=volume,src=own-by-a" -- dk $B docker run --rm --mount type=volume,src=own-by-a,dst=/d $IMG true
want "the volume is still a's" "$(owner own-by-a)" "$A"
dk $A docker volume rm own-by-a >/dev/null

echo "== a volume the engine has and nobody owns is not adopted by the first to name it"
prod volume create foreign-unowned >/dev/null
expect refused "a: docker volume create foreign-unowned" -- dk $A docker volume create foreign-unowned
expect refused "a: docker run -v foreign-unowned:/d" -- dk $A docker run --rm -v foreign-unowned:/d $IMG true
prod volume rm foreign-unowned >/dev/null

echo "== anonymous and image-declared volumes belong to their creator"
dk $A docker pull -q postgres:17 >/dev/null
dk $A docker rm -f av-a av-pg >/dev/null 2>&1; dk $B docker rm -f av-b >/dev/null 2>&1
dk $A docker run -d --name av-a -v /anon-data $IMG sleep 300 >/dev/null
dk $A docker run -d --name av-pg -e POSTGRES_PASSWORD=x postgres:17 >/dev/null
dk $B docker run -d --name av-b -v /anon-data $IMG sleep 300 >/dev/null
VA=$(prod inspect av-a --format '{{range .Mounts}}{{.Name}}{{end}}')
VP=$(prod inspect av-pg --format '{{range .Mounts}}{{if eq .Destination "/var/lib/postgresql/data"}}{{.Name}}{{end}}{{end}}')
[ -z "$VP" ] && VP=$(prod inspect av-pg --format '{{range .Mounts}}{{.Name}} {{end}}' | awk '{print $1}')
VB=$(prod inspect av-b --format '{{range .Mounts}}{{.Name}}{{end}}')
echo "   a anonymous=$VA  a image-declared=$VP  b anonymous=$VB"
want "the anonymous volume carries a's scope label" "$(owner $VA)" "$A"
want "the image-declared volume carries a's scope label" "$(owner $VP)" "$A"
sees $A $VA && pass "a lists its anonymous volume" || fail "a does not list its own anonymous volume"
sees $A $VP && pass "a lists its image-declared volume" || fail "a does not list its image-declared volume"
sees $B $VA && fail "b lists a's anonymous volume" || pass "b does not list a's anonymous volume"
dk $B docker volume prune -f >/dev/null 2>&1
prod volume ls -q | grep -qx "$VA" && pass "b's prune left a's anonymous volume alone" || fail "b's prune removed a's volume"
prod volume ls -q | grep -qx "$VB" && pass "b's prune left b's in-use anonymous volume alone" || fail "b's prune removed a volume in use"
dk $A docker rm -f av-a av-pg >/dev/null 2>&1; dk $B docker rm -f av-b >/dev/null 2>&1
prod volume ls -q | grep -qx "$VA" && pass "removing the container leaves its volume (as Docker does)" || fail "volume vanished with its container"
dk $A docker volume prune -f >/dev/null 2>&1
prod volume ls -q | grep -qx "$VA" && fail "a cannot reclaim its own anonymous volume (prune)" || pass "a's prune reclaims its own anonymous volume"
prod volume ls -q | grep -qx "$VP" && fail "a cannot reclaim its image-declared volume (prune)" || pass "a's prune reclaims its image-declared volume"
prod volume ls -q | grep -qx "$VB" && pass "and left b's (now unused) volume alone" || fail "a's prune removed b's volume"
dk $B docker volume prune -f >/dev/null 2>&1
rm -rf $T
summary
