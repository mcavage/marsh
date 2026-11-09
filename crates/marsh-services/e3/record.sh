#!/usr/bin/env bash
# Record real client traffic (CLI, Compose, buildx) with a transparent proxy.
source "$(dirname "$0")/lib.sh"
bash "$(dirname "$0")/engine-up.sh" 2>&1 | tail -1
R=/var/lib/nfx-e3/socks/rec.sock
vx sh -c "rm -f /tmp/rec.txt /tmp/rec.pid; (sh -c 'echo \$\$ > /tmp/rec.pid; exec /marsh/e3-harness record --listen $R --upstream /var/run/docker.sock --out /tmp/rec.txt' >/dev/null 2>&1 &) ; sleep 1; chmod 666 $R"
prod sh -c 'docker rm -f $(docker ps -aq) 2>/dev/null; docker volume prune -af >/dev/null; docker network prune -f >/dev/null' >/dev/null 2>&1
D() { sbx exec "$SB" sh -c "cd $PROJ && export DOCKER_HOST=unix://$R && $*" 2>&1 | tail -${TAIL:-2}; }
D "docker pull -q busybox:latest"
D "docker run --rm busybox echo hi"
D "echo x | docker run --rm -i busybox cat"
D "docker run -d --name w1 -p 18080:80 -p 127.0.0.1:18081:81/udp --restart unless-stopped -e A=B -l k=v --cap-add NET_ADMIN --init --shm-size 64m --user 0 -v $PROJ/d:/d:ro -v namedvol:/n --tmpfs /t:size=10m --memory 256m --cpus 1 --pids-limit 100 --add-host x:1.2.3.4 --health-cmd true --hostname hh --workdir / --entrypoint sh busybox -c 'httpd -f -p 80'"
D "docker exec w1 true; echo foo | docker exec -i w1 cat; docker exec -u 0 -w / -e X=1 w1 pwd"
D "docker logs w1; docker logs --tail 5 w1; timeout 3 docker logs -f w1; true"
D "docker cp w1:/etc/hostname /tmp/hn && docker cp /tmp/hn w1:/tmp/hn2 && docker cp w1:/etc/hostname - | wc -c"
D "docker inspect w1 >/dev/null; docker top w1; docker stats --no-stream w1; docker port w1; docker diff w1; docker export w1 | wc -c"
D "docker network create n1; docker network connect n1 w1; docker network inspect n1 >/dev/null; docker network disconnect n1 w1; docker network rm n1; docker network ls"
D "docker volume create v1; docker volume ls; docker volume inspect v1 >/dev/null; docker volume rm v1"
D "docker update --memory 512m --memory-swap 1g w1; docker stop w1; docker start w1; docker restart w1; docker pause w1; docker unpause w1; docker kill w1; docker rename w1 w2; docker wait w2"
D "docker commit w2 nfx/c1; docker tag nfx/c1 nfx/c2; docker images; docker image inspect nfx/c1 >/dev/null; docker history nfx/c1 >/dev/null; docker save nfx/c1 | wc -c; docker rmi nfx/c2"
D "docker rm -f w2; docker ps -a; docker info >/dev/null; docker version >/dev/null; timeout 2 docker events --since 1m; true"
D "docker run --rm --mount type=bind,src=$PROJ/d,dst=/b,readonly --mount type=volume,src=mv,dst=/v --mount type=tmpfs,dst=/tm,tmpfs-size=1m busybox ls /b /v /tm"
D "docker run --rm --network none --read-only --security-opt no-new-privileges --ulimit nofile=1024 --dns 1.1.1.1 --label a=b busybox true"
D "docker build -t nfx/b1 build-ctx && docker run --rm nfx/b1 cat /built"
D "docker build --no-cache --progress=plain -t nfx/b1 build-ctx"
D "DOCKER_BUILDKIT=0 docker build -t nfx/b3 build-ctx"
D "docker buildx build --load -t nfx/b2 build-ctx"
D "cd stack-a && docker compose up -d --wait && docker compose ps && docker compose logs --no-color | head -3 && docker compose exec -T db psql -U postgres -d app -Atc 'select 1' && docker compose down -v"
D "docker container prune -f; docker volume prune -f; docker network prune -f"
vx cat /tmp/rec.txt > "$OUT/rec.txt"
vx sh -c 'kill $(cat /tmp/rec.pid) 2>/dev/null; rm -f /tmp/rec.pid' || true
wc -l "$OUT/rec.txt"
