#!/usr/bin/env bash
# SEC-04 (deterministic + static) and SEC-14 (pin lifetime).
source "$(dirname "$0")/lib.sh"
S=${1:-base}
P=$PROJ
IMG=busybox:latest
RUN=$$
CAN=/etc/nfx-vmroot-$RUN
vx sh -c "echo VMROOT-CANARY-$RUN > $CAN"
sbx exec $SB sh -c "rm -rf $P/race $P/race2 $P/race3 $P/race3.orig $P/race4; mkdir -p $P/race/d $P/race2/d $P/race3/d; echo ok-file > $P/race/d/ok.txt; echo ok-file2 > $P/race2/d/ok.txt; echo ok-file3 > $P/race3/d/ok.txt"
dk $S docker pull -q $IMG >/dev/null
sw() { sbx exec $SB sh -c "$*"; }   # act as the attacker on the project folder

echo "== SEC-04 c1: create (pins), swap d for a symlink to /, then start"
dk $S docker rm -f evil-pin >/dev/null 2>&1
dk $S docker create --name evil-pin -v $P/race/d:/h $IMG sh -c "cat /h/ok.txt; cat /h/$CAN 2>&1; ls /h"
sw "mv $P/race/d $P/race/d.orig && ln -s / $P/race/d"
out=$(dk $S docker start -a evil-pin 2>&1)
echo "$out" | sed 's/^/   | /'
if echo "$out" | grep -q "VMROOT-CANARY"; then echo "BAD  c1: the container saw the VM root"; FAIL=$((FAIL+1)); \
elif echo "$out" | grep -q "ok-file"; then echo "ok   c1: the container saw the original inode, not the VM root"; PASS=$((PASS+1)); \
else echo "BAD  c1: unexpected output"; FAIL=$((FAIL+1)); fi
sw "rm $P/race/d && mv $P/race/d.orig $P/race/d"
dk $S docker rm -f evil-pin >/dev/null

echo "== SEC-04 c2: static escapes"
sw "ln -sfn / $P/race/tolink; ln -sfn /proc/self/root $P/race/self; rm -rf $P/race/new"
expect refused "symlink to /"            -- dk $S docker run --rm -v $P/race/tolink:/h $IMG ls /h
expect refused "magic link /proc/self/root" -- dk $S docker run --rm -v $P/race/self:/h $IMG ls /h
expect refused "\$P/../.."                -- dk $S docker run --rm -v $P/../..:/h $IMG ls /h
expect refused "sibling \$P-evil"         -- dk $S docker run --rm -v $P-evil:/h $IMG ls /h
expect refused "outside the project"      -- dk $S docker run --rm -v /etc:/h $IMG ls /h
expect refused "mount of a missing source (Mounts never creates)" -- dk $S docker run --rm --mount type=bind,src=$P/race/new2,dst=/h $IMG ls /h
expect allowed "missing leaf is created inside the project" -- dk $S docker run --rm -v $P/race/new:/h $IMG ls /h
[ -d $P/race/new ] && { echo "ok   c2: the missing leaf exists inside the project"; PASS=$((PASS+1)); } || { echo "BAD  c2: missing leaf not created in the project"; FAIL=$((FAIL+1)); }
expect refused "volume with device=etc"   -- dk $S docker volume create --opt type=none --opt o=bind --opt device=etc evil-v1
sw "rm -f $P/race/tolink $P/race/self"

echo "== SEC-14: pin lifetime"
dk $S docker rm -f kk1 kk2 kk3 >/dev/null 2>&1
dk $S docker run -d --name kk1 --restart always -v $P/race/d:/h $IMG sh -c 'while :; do sleep 1; done' >/dev/null
dk $S docker run -d --name kk2 --restart always -v $P/race2/d:/h $IMG sh -c 'while :; do sleep 1; done' >/dev/null
dk $S docker run -d --name kk3 --restart always -v $P/race3/d:/h $IMG sh -c 'while :; do sleep 1; done' >/dev/null
KPIN=$(prod inspect kk1 --format '{{range .Mounts}}{{.Source}}{{end}}')
K2PIN=$(prod inspect kk2 --format '{{range .Mounts}}{{.Source}}{{end}}')
echo "   pins: k=$KPIN k2=$K2PIN"
dk $S docker stop kk1 >/dev/null
sw "mv $P/race/d $P/race/d.orig && ln -s / $P/race/d"
dk $S docker start kk1 >/dev/null
sleep 1
seen=$(dk $S docker exec kk1 sh -c "ls /h; cat /h/$CAN 2>&1" 2>&1)
if echo "$seen" | grep -q "VMROOT"; then echo "BAD  c1: after stop/swap/start k sees the VM root"; FAIL=$((FAIL+1)); \
elif echo "$seen" | grep -q ok.txt; then echo "ok   c1: after stop/swap/start kk1 still sees the original inode"; PASS=$((PASS+1)); else echo "BAD  c1: $seen"; FAIL=$((FAIL+1)); fi
sw "rm $P/race/d && mv $P/race/d.orig $P/race/d"
echo "-- kk2: replace the LEAF with a symlink to /; kk3: replace the PARENT with a symlink to a tree whose d is a symlink to /; then restart the engine (reboot)"
sw "mv $P/race2/d $P/race2/d.orig && ln -s / $P/race2/d"
sw "mv $P/race3 $P/race3.orig && mkdir $P/race4 && ln -s / $P/race4/d && ln -s race4 $P/race3"
echo "-- fp1: a pin whose source is gone before the restart (cannot be restored); vf-b: volumes-from, original deleted"
sw "rm -rf $P/race5 $P/race6; mkdir -p $P/race5/d $P/race6/d; echo fp-ok > $P/race5/d/ok.txt; echo vf-ok > $P/race6/d/ok.txt"
dk $S docker rm -f fp1 vf-a vf-b >/dev/null 2>&1
dk $S docker run -d --name fp1 --restart always -v $P/race5/d:/h $IMG sh -c 'while :; do sleep 1; done' >/dev/null
FPPIN=$(prod inspect fp1 --format '{{range .Mounts}}{{.Source}}{{end}}')
dk $S docker run -d --name vf-a -v $P/race6/d:/h $IMG sleep 600 >/dev/null
VFPIN=$(prod inspect vf-a --format '{{range .Mounts}}{{.Source}}{{end}}')
dk $S docker run -d --name vf-b --restart always --volumes-from vf-a $IMG sh -c 'while :; do sleep 1; done' >/dev/null
dk $S docker rm -f vf-a >/dev/null
sleep 1
vx findmnt -rn -o TARGET | grep -q "$(basename "$VFPIN")" && pass "volumes-from: the pin outlives the deleted original (vf-b holds it)" || fail "volumes-from: the pin was released while vf-b still takes volumes from it"
out=$(dk $S docker exec vf-b ls /h 2>&1 | tr '\n' ' ')
echo "$out" | grep -q ok.txt && pass "volumes-from: vf-b still sees the original's data: $out" || fail "volumes-from: vf-b sees [$out]"
sw "rm -rf $P/race5/d"
docker_restart() { sbx exec $SB docker restart nfx-e3-engine >/dev/null; for i in $(seq 1 120); do prod info >/dev/null 2>&1 && return 0; sleep 0.5; done; }
docker_restart
sleep 3
vx tail -4 /var/log/e3-agent.log
for c in kk1 kk2 kk3; do
  seen=$(prod exec $c sh -c "ls /h 2>&1; cat /h/$CAN 2>&1" 2>&1 | tr '\n' ' ')
  echo "   $c after reboot: running=$(prod inspect $c --format '{{.State.Running}}') sees: $seen"
  if echo "$seen" | grep -q "VMROOT"; then echo "BAD  c1: $c sees the VM root after reboot"; FAIL=$((FAIL+1)); else echo "ok   c1: $c never saw the VM root after reboot"; PASS=$((PASS+1)); fi
done
seen=$(prod exec kk1 ls /h 2>&1 | tr '\n' ' ')
echo "$seen" | grep -q ok.txt && { echo "ok   restore: kk1 (untouched source) has its pin back and sees: $seen"; PASS=$((PASS+1)); } || { echo "BAD  restore: kk1 lost its pin across the reboot: [$seen]"; FAIL=$((FAIL+1)); }
echo "-- restore must fail closed: fp1's pin could not be recreated"
for i in $(seq 1 20); do [ "$(prod inspect fp1 --format '{{.State.Running}}')" = false ] && break; sleep 1; done
sleep 4
want "failed restore: fp1 does not run" "$(prod inspect fp1 --format '{{.State.Running}}')" false
vx test -L $FPPIN && pass "failed restore: its pin path is a dangling symlink (docker cannot create an empty directory there)" || fail "failed restore: pin path is not a poisoned symlink: $(vx ls -ld $FPPIN 2>&1)"
vx test -d $FPPIN && fail "failed restore: an (empty) directory stands where the pin was" || pass "failed restore: no directory at the pin path"
expect refused "failed restore: docker start fp1 is refused by the filter" -- dk $S docker start fp1
seen=$(prod exec vf-b ls /h 2>&1 | tr '\n' ' ')
echo "$seen" | grep -q ok.txt && pass "volumes-from after the restart: vf-b has the inherited pin back ($seen)" || fail "volumes-from after the restart: vf-b sees [$seen]"
dk $S docker rm -f fp1 >/dev/null 2>&1
sleep 1
vx test -e $FPPIN -o -L $FPPIN && fail "failed restore: the poisoned pin path survived removal of its container" || pass "failed restore: the poisoned path is removed with the container"
dk $S docker rm -f vf-b >/dev/null 2>&1
sleep 1
vx test -e $VFPIN && fail "volumes-from: the pin survived the removal of its last holder" || pass "volumes-from: the pin is released with its last holder"
sw "rm -rf $P/race5 $P/race6"
sw "rm $P/race2/d && mv $P/race2/d.orig $P/race2/d; rm $P/race3 && mv $P/race3.orig $P/race3; rm -rf $P/race4"
echo "== c2: remove k; its pin must disappear"
dk $S docker rm -f kk1 >/dev/null
sleep 1
if vx findmnt -rn -o TARGET | grep -q "$(basename "$KPIN")"; then echo "BAD  c2: pin of k still mounted"; FAIL=$((FAIL+1)); else echo "ok   c2: no pin of k under /var/lib/marsh/binds"; PASS=$((PASS+1)); fi
for i in $(seq 1 20); do vx test -e $KPIN || break; sleep 0.5; done
vx test -e $KPIN && { echo "BAD  c2: pin path of k still exists"; FAIL=$((FAIL+1)); } || { echo "ok   c2: pin path removed"; PASS=$((PASS+1)); }
echo "== a file bind: the mountpoint file is removed with its container"
sw "echo filedata > $P/race/f.txt"
dk $S docker rm -f kf1 >/dev/null 2>&1
dk $S docker run -d --name kf1 -v $P/race/f.txt:/f.txt $IMG sleep 300 >/dev/null
FPIN=$(prod inspect kf1 --format '{{range .Mounts}}{{.Source}}{{end}}')
vx test -f $FPIN && pass "file pin: the pin is a file mount ($FPIN)" || fail "file pin: $FPIN is not a file"
want "file pin: the container reads it" "$(dk $S docker exec kf1 cat /f.txt)" filedata
dk $S docker rm -f kf1 >/dev/null
for i in $(seq 1 20); do vx test -e $FPIN || break; sleep 0.5; done
vx test -e $FPIN && fail "file pin: the mountpoint file was left behind" || pass "file pin: the mountpoint file is gone"
sw "rm -f $P/race/f.txt"

echo "== a path on another mount is refused (NO_XDEV), and allowed again once the mount is gone"
vx sh -c "mkdir -p $P/xdev && mount -t tmpfs tmpfs $P/xdev && mkdir -p $P/xdev/sub && echo x-ok > $P/xdev/sub/f"
expect refused "bind of a path on a tmpfs mounted inside the project (NO_XDEV)" -- dk $S docker run --rm -v $P/xdev/sub:/h $IMG ls /h
expect refused "bind of the mount point itself" -- dk $S docker run --rm -v $P/xdev:/h $IMG ls /h
vx umount $P/xdev
expect allowed "after the unmount the same bind is allowed (control)" -- dk $S docker run --rm -v $P/xdev:/h $IMG ls /h
sw "rm -rf $P/xdev"

echo "== c3: volume-subpath refused"
expect refused "volume-subpath" -- dk $S docker run --rm --mount type=volume,src=sp,dst=/h,volume-subpath=x $IMG true
dk $S docker rm -f kk2 kk3 >/dev/null 2>&1
vx rm -f $CAN
no_evil
summary
