#!/usr/bin/env bash
# SEC-01: container-create allowlist through the real Docker CLI.
source "$(dirname "$0")/lib.sh"
S=${1:-base}
P=$PROJ
IMG=busybox:latest
E() { expect refused "$1" -- dk $S docker "${@:2}"; }
A() { expect allowed "$1" -- dk $S docker "${@:2}"; }
sbx exec $SB sh -c "mkdir -p $P/sub && echo '{\"defaultAction\":\"SCMP_ACT_ALLOW\"}' > $P/allow-all.json"
dk $S docker pull -q $IMG >/dev/null
n=0; e() { n=$((n+1)); E "$1" run --rm --name evil-$n "${@:2}"; }
e "--privileged" --privileged $IMG true
e "-v /:/host" -v /:/host $IMG true
e "-v /var/lib/docker:/x" -v /var/lib/docker:/x $IMG true
e "-v /pool:/x" -v /pool:/x $IMG true
e "--mount bind /" --mount type=bind,src=/,dst=/h $IMG true
e "-v \$P/../..:/h" -v $P/../..:/h $IMG true
e "-v sibling \$P-evil" -v $P-evil:/h $IMG true
for ns in network pid ipc uts userns cgroupns; do e "--$ns host" --$ns host $IMG true; done
for cap in SYS_ADMIN ALL SYS_MODULE SYS_RESOURCE IPC_OWNER BPF; do e "--cap-add $cap" --cap-add $cap $IMG true; done
e "--device /dev/loop0" --device /dev/loop0 $IMG true
e "--device-cgroup-rule" --device-cgroup-rule 'b 7:* rwm' $IMG true
e "--gpus all" --gpus all $IMG true
e "seccomp=unconfined" --security-opt seccomp=unconfined $IMG true
e "seccomp=allow-all.json" --security-opt seccomp=$P/allow-all.json $IMG true
e "apparmor=unconfined" --security-opt apparmor=unconfined $IMG true
e "systempaths=unconfined" --security-opt systempaths=unconfined $IMG true
e "--cgroup-parent /" --cgroup-parent / $IMG true
e "--log-driver syslog" --log-driver syslog $IMG true
e "--runtime runc-x" --runtime runc-x $IMG true
e "--oom-score-adj -1000" --oom-score-adj -1000 $IMG true
e "--label dev.marsh.scope=x" --label dev.marsh.scope=x $IMG true
e "--network bridge" --network bridge $IMG true
e "--mount rshared" --mount type=bind,src=$P/sub,dst=/h,bind-propagation=rshared $IMG true
e "--mount create-mountpoint" --mount type=bind,src=$P/sub/new,dst=/h,bind-create-src=true $IMG true
e "--mount volume subpath" --mount type=volume,src=somevol,dst=/h,volume-subpath=x $IMG true
e "--mount volume-opt device=/" --mount type=volume,src=somevol2,dst=/h,volume-opt=device=/,volume-opt=type=none,volume-opt=o=bind $IMG true
e "memory above maximum" --memory 1024g $IMG true
e "pids above maximum" --pids-limit 9999999 $IMG true
e "name marsh-svc-engine" --name marsh-svc-engine $IMG true 2>/dev/null
for v in "type=none --opt o=bind --opt device=/var/lib/docker" "type=none --opt o=bind --opt device=../.." "type=nfs --opt device=:/x" "type=overlay --opt o=lowerdir=/" "type=none --opt o=bind --opt device=etc"; do
  E "volume create $v" volume create --opt $v evil-vol$((n++))
done
E "network create macvlan" network create -d macvlan -o parent=eth0 evil-net1
E "network create masquerade off" network create -o com.docker.network.bridge.enable_ip_masquerade=false evil-net2
E "network create name host" network create host2 --label dev.marsh.x=1
dk $S docker run -d --name ok-exec $IMG sleep 300 >/dev/null
E "exec --privileged" exec --privileged ok-exec true
E "inspect marsh-svc-engine" inspect marsh-svc-engine
E "exec marsh-svc-engine" exec marsh-svc-engine true
E "cp from marsh-svc-engine" cp marsh-svc-engine:/etc/passwd /tmp/x
E "docker push" push busybox
# Allowed rows
for cap in NET_BIND_SERVICE NET_RAW NET_ADMIN SYS_NICE SYS_PTRACE; do A "--cap-add $cap" run --rm --cap-add $cap $IMG true; done
A "-v \$P/sub:/s:ro" run --rm -v $P/sub:/s:ro $IMG ls /s
A "-v \$P/sub:/s (write appears on the Mac)" run --rm -v $P/sub:/s $IMG sh -c 'echo w > /s/written'
[ "$(cat $P/sub/written 2>/dev/null)" = w ] && { echo "ok   write appeared on the Mac"; PASS=$((PASS+1)); } || { echo "BAD  write did not appear"; FAIL=$((FAIL+1)); }
A "named volume" run --rm -v ok-vol:/v $IMG true
A "--tmpfs" run --rm --tmpfs /t $IMG true
A "tmpfs volume" volume create --opt type=tmpfs --opt device=tmpfs --opt o=size=64m,mode=1777,uid=1000 ok-tmpfsvol
A "--user 0 --read-only --init --shm-size --add-host" run --rm --user 0 --read-only --init --shm-size 256m --add-host x:1.2.3.4 $IMG true
A "limits within maxima" run --rm --memory 512m --cpus 1 --pids-limit 100 $IMG true
A "--restart always" run -d --name ok-restart --restart always $IMG sleep 300
A "scope network" network create ok-net
A "--network <scope net>" run --rm --network ok-net $IMG true
A "--network container:<own>" run --rm --network container:ok-exec $IMG true
A "-p 5433:5432" run -d --name ok-p1 -p 5433:5432 $IMG sleep 100
A "-p 0.0.0.0:5434:5432" run -d --name ok-p2 -p 0.0.0.0:5434:5432 $IMG sleep 100
echo "-- publishes in the production engine:"
prod inspect ok-p1 ok-p2 --format '{{.Name}} {{json .HostConfig.PortBindings}}'
hosts=$(prod inspect ok-p1 ok-p2 --format '{{range $k,$v := .HostConfig.PortBindings}}{{range $v}}{{.HostIp}} {{end}}{{end}}')
echo "$hosts" | grep -qv '127.0.0.1' && { echo "BAD  non-loopback publish: $hosts"; FAIL=$((FAIL+1)); } || { echo "ok   every publish is 127.0.0.1"; PASS=$((PASS+1)); }
no_evil
echo "-- defaults on a limit-less create:"
prod inspect ok-exec --format 'Memory={{.HostConfig.Memory}} NanoCpus={{.HostConfig.NanoCpus}} PidsLimit={{.HostConfig.PidsLimit}} NetworkMode={{.HostConfig.NetworkMode}} scope={{index .Config.Labels "dev.marsh.scope"}} attempt={{index .Config.Labels "dev.marsh.attempt"}}'
lim=$(prod inspect ok-exec --format '{{.HostConfig.Memory}} {{.HostConfig.NanoCpus}} {{.HostConfig.PidsLimit}}')
[ "$lim" = "4294967296 4000000000 4096" ] && { echo "ok   SEC-20 c1: default limits applied"; PASS=$((PASS+1)); } || { echo "BAD  SEC-20 c1: limits are $lim"; FAIL=$((FAIL+1)); }
# The engine is given the network's full ID (not its name); what it reports back as the
# container's network is the name.
ns=$(prod inspect ok-exec --format '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' | tr -d ' ')
nid=$(prod inspect ok-exec --format '{{.HostConfig.NetworkMode}}')
[ "$ns" = "marsh-scope-$S" ] && pass "default network is the scope's ($ns)" || fail "network $ns"
[ "$nid" = "$(prod network inspect marsh-scope-$S --format '{{.Id}}')" ] && pass "the engine was given the scope network's full ID" || fail "NetworkMode is $nid, not the network's ID"
# cleanup
dk $S docker rm -f ok-exec ok-restart ok-p1 ok-p2 >/dev/null 2>&1
dk $S docker volume rm ok-vol ok-tmpfsvol >/dev/null 2>&1; dk $S docker network rm ok-net >/dev/null 2>&1
echo "== round 2: tightened schema rows"
dk $S docker rm -f cm-1 >/dev/null 2>&1
dk $S docker run -d --name cm-1 busybox sleep 120 >/dev/null
expect refused "commit onto a shared base image name (busybox:latest)" -- dk $S docker commit cm-1 busybox:latest
expect refused "commit into another workspace's namespace" -- dk $S docker commit cm-1 other-workspace/img
expect refused "commit into docker.io/library" -- dk $S docker commit cm-1 docker.io/library/busybox:evil
expect allowed "commit into the workspace's own namespace ($S/c1)" -- dk $S docker commit cm-1 $S/c1
dk $S docker rmi -f $S/c1 >/dev/null 2>&1
dk $S docker rm -f cm-1 >/dev/null 2>&1
expect refused "network create with an MTU option" -- dk $S docker network create -o com.docker.network.driver.mtu=1400 evil-mtu
expect refused "network create with any driver option" -- dk $S docker network create -o com.docker.network.bridge.name=evilbr evil-opt
expect refused "bind with :private propagation" -- dk $S docker run --rm --name evil-prv -v $P/d:/d:private busybox true
expect refused "--mount bind-propagation=private" -- dk $S docker run --rm --name evil-prv2 --mount type=bind,src=$P/d,dst=/d,bind-propagation=private busybox true
expect refused "container name that looks like a full ID" -- dk $S docker run --rm --name $(printf 'ab%.0s' $(seq 1 32)) busybox true
expect refused "network name that looks like a full ID" -- dk $S docker network create $(printf 'cd%.0s' $(seq 1 32))
expect refused "volume name that looks like a full ID" -- dk $S docker volume create $(printf 'ef%.0s' $(seq 1 32))
code=$(sbx exec $SB curl -s -o /dev/null -w '%{http_code}' --unix-socket $(sock $S) -X POST "http://d/v1.47/build?outputs=%5B%7B%22Type%22%3A%22moby%22%2C%22Attrs%22%3A%7B%22push%22%3A%22true%22%7D%7D%5D")
want "build outputs with push is refused" "$code" 403
code=$(sbx exec $SB curl -s -o /dev/null -w '%{http_code}' --unix-socket $(sock $S) -X POST "http://d/v1.47/build?outputs=%5B%7B%22Type%22%3A%22moby%22%2C%22Unexpected%22%3A1%7D%5D")
want "build outputs with an unknown field is refused" "$code" 403
no_evil
summary
