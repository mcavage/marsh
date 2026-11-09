#!/usr/bin/env bash
# EP-01-ish: unchanged `docker compose up` of the spike's stacks/a, with the ./app bind.
source "$(dirname "$0")/lib.sh"
S=${1:-stacka}
D=$PROJ/stack-a
cd_run() { sbx exec $SB sh -c "cd $D && export DOCKER_HOST=unix://$(sock $S) && $*"; }
cmp_before=$(shasum $D/compose.yaml 2>/dev/null | cut -c1-16)
cd_run "docker compose down -v >/dev/null 2>&1; true"
echo "== docker compose up -d --wait"
t0=$(date +%s)
out=$(cd_run "docker compose up -d --wait" 2>&1); rc=$?
echo "$out" | tail -12
echo "   took $(( $(date +%s)-t0 ))s"
want "docker compose up -d --wait exits 0" "$rc" 0
echo "== compose ps"; ps_out=$(cd_run "docker compose ps --format '{{.Service}} {{.State}}'" 2>&1); echo "$ps_out"
nsvc=$(echo "$ps_out" | grep -c . || true); nrun=$(echo "$ps_out" | grep -c ' running' || true)
[ "$nsvc" -ge 4 ] && [ "$nsvc" = "$nrun" ] && pass "all $nsvc services of the stack are running" || fail "services running: $nrun of $nsvc"
echo "== pin of ./app visible in findmnt (engine mount namespace)"
vx sh -c 'findmnt -rn -o TARGET,SOURCE,OPTIONS | grep /var/lib/marsh/binds' | cut -c1-150
app=$(prod inspect stack-a-app-1 --format '{{range .Mounts}}{{if eq .Destination "/app"}}{{.Source}} rw={{.RW}}{{end}}{{end}}')
echo "   app container's /app mount: $app"
case "$app" in /var/lib/marsh/binds/*" rw=false") echo "ok   ./app is a pin and read-only (as the compose file says)"; PASS=$((PASS+1));; *) echo "BAD  /app mount: $app"; FAIL=$((FAIL+1));; esac
echo "== the app container ran the bind-mounted script"
cd_run "docker compose exec -T db psql -U postgres -d app -Atc 'select count(*) from items'" 2>&1 | tail -1
cd_run "docker compose exec -T db psql -U postgres -d app -Atc 'select 1'" | grep -q 1 && pass "exec psql" || fail "exec psql"
n=$(cd_run "docker compose exec -T db psql -U postgres -d app -Atc 'select count(*) from items'" | tail -1)
[ "$n" = 1000 ] && pass "1000 seeded rows written by the app container from the ./app bind" || fail "items=$n"
echo "== network: stack containers are on the compose network, labelled with the scope"
prod ps --format '{{.Names}} scope={{.Label "dev.marsh.scope"}}'
cd_run "docker compose logs --no-color 2>&1 | head -3"
cd_run "docker compose build 2>&1 | tail -1"
cd_run "docker compose down -v 2>&1 | tail -4"
after=$(shasum $D/compose.yaml | cut -c1-16)
want "compose.yaml unchanged" "$after" "$cmp_before"
sleep 2
pins=$(vx sh -c 'findmnt -rn -o TARGET | grep -c /var/lib/marsh/binds/stacka/ || true' | tr -d ' ')
want "no mount of a stacka pin is left after down" "$pins" 0
files=$(vx sh -c 'ls -A /var/lib/marsh/binds/stacka 2>/dev/null | wc -l' | tr -d ' ')
want "no pin path of the stack is left on disk after down" "$files" 0
want "no container of the stack is left" "$(prod ps -aq --filter label=dev.marsh.scope=stacka | grep -c . || true)" 0
want "no volume of the stack is left" "$(prod volume ls -q --filter label=dev.marsh.scope=stacka | grep -c . || true)" 0
want "no network of the stack is left" "$(prod network ls -q --filter label=dev.marsh.scope=stacka | grep -c . || true)" 0

echo "== hostile Compose files: every expansion that asks for more than B2 allows is refused, nothing is created"
H=$D/hostile; mkdir -p $H
hostile() { # hostile NAME 'service yaml lines' [REGEX the refusal must match; default marsh:]
  local re=${3:-marsh:}
  printf 'services:\n  evil-%s:\n    image: busybox\n    command: sleep 60\n%b\n' "$1" "$2" > $H/$1.yaml
  out=$(cd_run "docker compose -f $H/$1.yaml -p hostile$1 up -d" 2>&1); rc=$?
  if [ $rc -ne 0 ] && echo "$out" | grep -qE "$re"; then pass "compose: $1 is refused ($(echo "$out" | grep -oE "$re[^\"]*" | head -1 | cut -c1-70))"; else fail "compose: $1 was not refused (rc=$rc): $(echo "$out" | tail -2 | cut -c1-150)"; fi
  cd_run "docker compose -f $H/$1.yaml -p hostile$1 down -v" >/dev/null 2>&1
}
hostile privileged '    privileged: true'
hostile hostnet '    network_mode: host'
hostile rootbind '    volumes:\n      - /:/host'
hostile etcbind '    volumes:\n      - /etc:/host-etc:ro'
hostile sockhost '    volumes:\n      - /var/lib/docker:/d'
hostile capadmin '    cap_add:\n      - SYS_ADMIN'
hostile capall '    cap_add:\n      - ALL'
hostile pidhost '    pid: host'
hostile ipchost '    ipc: host'
hostile usernshost '    userns_mode: host'
hostile devices '    devices:\n      - /dev/sda:/dev/sda'
hostile seccomp '    security_opt:\n      - seccomp=unconfined'
hostile apparmor '    security_opt:\n      - apparmor=unconfined'
hostile sysctl '    sysctls:\n      - kernel.shm_rmid_forced=0'
hostile cgroupparent '    cgroup_parent: /'
hostile foreignnet '    networks:\n      - bridge\nnetworks:\n  bridge:\n    external: true' 'marsh:|could not be found'
hostile runtime '    runtime: runc\n    device_cgroup_rules:\n      - "c *:* rwm"'
no_evil
rm -rf $H
summary
