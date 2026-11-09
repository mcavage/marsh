#!/usr/bin/env bash
# Single-check probes for the break matrix: labels | limits | publish
source "$(dirname "$0")/lib.sh"
S=base
dk $S docker rm -f ok-q evil-q >/dev/null 2>&1
# Controls: the endpoint answers, an ordinary run works, the scope lists its own object.
expect allowed "control: docker version" -- dk $S docker version --format '{{.Server.Version}}'
expect allowed "control: an ordinary run works" -- dk $S docker run --rm --name ok-q busybox true
expect allowed "control: docker ps" -- dk $S docker ps -a
case $1 in
  labels)  expect refused "--label dev.marsh.scope=x (SEC-16 c2)" -- dk $S docker run --rm --name evil-q --label dev.marsh.scope=x busybox true
           expect refused "network create --label dev.marsh.internal=1" -- dk $S docker network create --label dev.marsh.internal=1 evil-qnet
           expect refused "volume create --label dev.marsh.foo=1" -- dk $S docker volume create --label dev.marsh.foo=1 evil-qvol;;
  limits)  dk $S docker run -d --name ok-q busybox sleep 100 >/dev/null
           lim=$(prod inspect ok-q --format '{{.HostConfig.Memory}} {{.HostConfig.NanoCpus}} {{.HostConfig.PidsLimit}}')
           [ "$lim" = "4294967296 4000000000 4096" ] && { echo "ok   SEC-20 c1: default limits applied"; PASS=$((PASS+1)); } || { echo "BAD  SEC-20 c1: limits are $lim"; FAIL=$((FAIL+1)); };;
  publish) dk $S docker run -d --name ok-q -p 0.0.0.0:5439:5432 busybox sleep 100 >/dev/null
           ips=$(prod inspect ok-q --format '{{range $k,$v := .HostConfig.PortBindings}}{{range $v}}{{.HostIp}} {{end}}{{end}}')
           [ "$ips" = "127.0.0.1 " ] && { echo "ok   EP-10 c1: publish forced to 127.0.0.1"; PASS=$((PASS+1)); } || { echo "BAD  EP-10 c1: non-loopback publish: [$ips]"; FAIL=$((FAIL+1)); };;
esac
dk $S docker rm -f ok-q >/dev/null 2>&1; dk $S docker network rm evil-qnet >/dev/null 2>&1; dk $S docker volume rm evil-qvol >/dev/null 2>&1
summary
