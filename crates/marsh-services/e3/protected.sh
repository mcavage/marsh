#!/usr/bin/env bash
# SEC-05 c3/c4: .git and .marsh stay read-only through a bind of the project.
source "$(dirname "$0")/lib.sh"
S=${1:-base}
P=$PROJ
IMG=busybox:latest
# Strays from an earlier run under a break (protected-subpin-off and friends) are removed first.
sbx exec $SB sh -c "cd $P && rm -f .git/x .git/y .git/nr-pwn .git/ci-pwn .git/late-pwn .marsh/x .marsh/y .marsh/ci-pwn .marsh/checkpoints/late .marsh/checkpoints/late-pwn .git/hooks/x"
sbx exec $SB sh -c "cd $P && mkdir -p .marsh/checkpoints src && echo r > .marsh/checkpoints/README && echo base > SENTINEL && (git add -A >/dev/null 2>&1; true)"
sbx exec $SB test -d $P/.git || { echo "BAD  precondition: $P is not a git repository"; exit 1; }
h() { sbx exec $SB sh -c "cd $P && sha256sum .git/HEAD .git/index .marsh/checkpoints/README SENTINEL 2>/dev/null | cut -c1-16 | tr '\n' ' '"; }
before=$(h)
dk $S docker rm -f root-w >/dev/null 2>&1
dk $S docker run -d --name root-w -v $P:/p $IMG sleep 600 >/dev/null
echo "   mounts of root-w in the production engine:"; prod inspect root-w --format '{{range .Mounts}}{{.Source}} ro={{not .RW}}{{end}}'
echo "   findmnt:"; vx sh -c 'findmnt -rn -o TARGET,OPTIONS | grep /var/lib/marsh/binds' | cut -c1-110
dk $S docker rm -f evil-pw >/dev/null 2>&1
for f in /p/.git/x /p/.git/HEAD /p/.marsh/x /p/.git/hooks/x /p/.marsh/checkpoints/README; do
  out=$(dk $S docker exec root-w sh -c "echo pwn > $f" 2>&1)
  if [ $? -ne 0 ] && echo "$out" | grep -qi "read-only"; then echo "ok   c3: write to $f fails (read-only file system)"; PASS=$((PASS+1)); else echo "BAD  c3: write to $f: $out"; FAIL=$((FAIL+1)); fi
done
echo hostile > /tmp/hostile.txt; sbx exec $SB sh -c "echo hostile > /tmp/hostile.txt"
sbx exec $SB sh -c "tar cf /tmp/h.tar -C /tmp hostile.txt"
for d in /p/.git/ /p/.marsh/; do
  out=$(sbx exec $SB env DOCKER_HOST=unix://$(sock $S) docker cp /tmp/hostile.txt root-w:$d 2>&1)
  if [ $? -ne 0 ]; then echo "ok   c3: docker cp into $d fails: $(echo $out | cut -c1-70)"; PASS=$((PASS+1)); else echo "BAD  c3: docker cp into $d succeeded"; FAIL=$((FAIL+1)); fi
done
after=$(h)
[ "$before" = "$after" ] && { echo "ok   c3: hash canaries unchanged"; PASS=$((PASS+1)); } || { echo "BAD  c3: canaries changed: $before vs $after"; FAIL=$((FAIL+1)); }
[ ! -e $P/.git/x ] && [ ! -e $P/.marsh/x ] && echo "ok   c3: nothing appeared under .git or .marsh on the Mac side" || { echo "BAD  stray files"; FAIL=$((FAIL+1)); }
out=$(dk $S docker exec root-w sh -c "echo allowed > /p/src/x && cat /p/src/x" 2>&1)
[ "$out" = allowed ] && { echo "ok   c4: write to /p/src/x works"; PASS=$((PASS+1)); } || { echo "BAD  c4: $out"; FAIL=$((FAIL+1)); }
[ "$(cat $P/src/x)" = allowed ] && { echo "ok   c4: and it appears on the Mac"; PASS=$((PASS+1)); }
# A sub-path bind of .git is read-only too.
out=$(dk $S docker run --rm -v $P/.git:/g $IMG sh -c 'echo x > /g/y' 2>&1)
echo "$out" | grep -qi "read-only" && { echo "ok   c3: -v \$P/.git:/g is read-only"; PASS=$((PASS+1)); } || { echo "BAD  c3: -v .git:/g rw: $out"; FAIL=$((FAIL+1)); }
out=$(dk $S docker run --rm -v $P/.git:/g:rw $IMG sh -c 'echo x > /g/y' 2>&1)
echo "$out" | grep -qi "read-only" && { echo "ok   c3: -v \$P/.git:/g:rw is still read-only"; PASS=$((PASS+1)); } || { echo "BAD  c3: -v .git:/g:rw: $out"; FAIL=$((FAIL+1)); }
dk $S docker rm -f root-w >/dev/null 2>&1
rm -f $P/src/x

echo "== the same protection through the Mounts API (--mount) and docker cp, per spelling"
dk $S docker rm -f root-m >/dev/null 2>&1
dk $S docker run -d --name root-m --mount type=bind,src=$P,dst=/p $IMG sleep 600 >/dev/null
for f in /p/.git/x /p/.git/HEAD /p/.marsh/x /p/.git/hooks/x /p/.marsh/checkpoints/README; do
  out=$(dk $S docker exec root-m sh -c "echo pwn > $f" 2>&1)
  if [ $? -ne 0 ] && echo "$out" | grep -qi "read-only"; then pass "mounts-shape: write to $f fails (read-only file system)"; else fail "mounts-shape: write to $f: $out"; fi
done
for d in /p/.git/ /p/.marsh/; do
  out=$(sbx exec $SB env DOCKER_HOST=unix://$(sock $S) docker cp /tmp/hostile.txt root-m:$d 2>&1)
  if [ $? -ne 0 ]; then pass "mounts-shape: docker cp into $d fails"; else fail "mounts-shape: docker cp into $d succeeded"; fi
done
dk $S docker rm -f root-m >/dev/null 2>&1

echo "== non-recursive bind (bind-recursive=disabled) would drop the read-only sub-mounts: refused"
expect refused "--mount bind-recursive=disabled of the project" -- dk $S docker run --rm --mount type=bind,src=$P,dst=/p,bind-recursive=disabled $IMG sh -c 'echo pwn > /p/.git/nr-pwn'
[ ! -e $P/.git/nr-pwn ] && pass "nonrecursive: nothing was written into .git" || fail "nonrecursive: a file appeared in .git"
sbx exec $SB rm -f $P/.git/nr-pwn

echo "== case-insensitive spellings (APFS over virtiofs): .GIT is .git, decided by inode"
if sbx exec $SB test -d $P/.GIT; then pass "precondition: the project folder is case-insensitive"; else fail "precondition: $P/.GIT does not resolve to .git (case-sensitive folder: this witness proves nothing)"; fi
for src in $P/.GIT $P/.Git/hooks $P/.MARSH $P/.Marsh/checkpoints; do
  out=$(dk $S docker run --rm -v $src:/g $IMG sh -c 'echo x > /g/ci-pwn' 2>&1)
  if echo "$out" | grep -qi "read-only"; then pass "-v $src:/g is read-only"; else fail "-v $src:/g is writable: $out"; fi
done
out=$(dk $S docker run --rm --mount type=bind,src=$P/.GIT,dst=/g $IMG sh -c 'echo x > /g/ci-pwn' 2>&1)
echo "$out" | grep -qi "read-only" && pass "--mount src=.GIT is read-only" || fail "--mount src=.GIT is writable: $out"
[ ! -e $P/.git/ci-pwn ] && [ ! -e $P/.marsh/ci-pwn ] && [ ! -e $P/.git/hooks/ci-pwn ] && [ ! -e $P/.marsh/checkpoints/ci-pwn ] && pass "nothing was written under .git or .marsh" || fail "a file appeared under .git/.marsh"
sbx exec $SB sh -c "rm -f $P/.git/ci-pwn $P/.marsh/ci-pwn $P/.git/hooks/ci-pwn $P/.marsh/checkpoints/ci-pwn"

echo "== .marsh created after the pin is protected as well"
sbx exec $SB sh -c "rm -rf $P/.marsh"
dk $S docker rm -f root-late >/dev/null 2>&1
dk $S docker run -d --name root-late -v $P:/p $IMG sleep 600 >/dev/null
[ -d $P/.marsh ] && pass "a pin of the project root makes sure .marsh exists" || fail "no .marsh after the root pin"
sbx exec $SB sh -c "mkdir -p $P/.marsh/checkpoints && echo late > $P/.marsh/checkpoints/late"
out=$(dk $S docker exec root-late sh -c 'echo pwn > /p/.marsh/checkpoints/late-pwn' 2>&1)
if echo "$out" | grep -qi "read-only"; then pass "late .marsh: a write through the earlier pin fails (read-only)"; else fail "late .marsh: writable through the earlier pin: $out"; fi
out=$(dk $S docker exec root-late sh -c 'cat /p/.marsh/checkpoints/late' 2>&1)
want "late .marsh: the container sees what the host wrote" "$out" late
dk $S docker rm -f root-late >/dev/null 2>&1
sbx exec $SB sh -c "rm -f $P/.marsh/checkpoints/late-pwn $P/.marsh/checkpoints/late"
summary
