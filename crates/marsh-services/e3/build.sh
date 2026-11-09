#!/usr/bin/env bash
# EP-04: docker build (BuildKit session over h2c), legacy build, buildx, compose build.
source "$(dirname "$0")/lib.sh"
S=${1:-base}
B=$PROJ/build-ctx
mkdir -p $B; printf 'FROM busybox:latest\nCOPY hello /hello\nRUN echo built > /built && sleep 1\n' > $B/Dockerfile; echo hello > $B/hello
expect allowed "docker build (BuildKit)" -- dk $S docker build -t nfx/b1 $B
out=$(dk $S docker run --rm nfx/b1 cat /built 2>&1); [ "$out" = built ] && { echo "ok   built image runs"; PASS=$((PASS+1)); } || { echo "BAD  run built image: $out"; FAIL=$((FAIL+1)); }
out=$(dk $S docker build --progress=plain -t nfx/b1 $B 2>&1); echo "$out" | grep -q CACHED && { echo "ok   rebuild is CACHED"; PASS=$((PASS+1)); } || { echo "BAD  rebuild not cached"; FAIL=$((FAIL+1)); }
expect allowed "DOCKER_BUILDKIT=0 docker build" -- dk $S env DOCKER_BUILDKIT=0 docker build -t nfx/b3 $B
expect allowed "docker buildx build --load" -- dk $S docker buildx build --load -t nfx/b2 $B
expect refused "docker build --network host" -- dk $S docker build --network host -t nfx/b4 $B
expect refused "legacy build --cgroup-parent /" -- dk $S env DOCKER_BUILDKIT=0 docker build --cgroup-parent / -t nfx/b5 $B
# A remote context is refused by the filter itself (the CLI would otherwise hand the URL to the engine).
code=$(sbx exec $SB curl -s -o /dev/null -w '%{http_code}' --unix-socket $(sock $S) -X POST "http://d/v1.47/build?remote=https%3A%2F%2Fexample.com%2Fx.git&t=nfx/b6")
want "POST /build?remote=<url> is refused" "$code" 403
code=$(sbx exec $SB curl -s -o /dev/null -w '%{http_code}' --unix-socket $(sock $S) -X POST "http://d/v1.47/build?remote=https%3A%2F%2Fexample.com%2Fx.git&version=2&t=nfx/b6")
want "POST /build?remote=<url>&version=2 (BuildKit) is refused" "$code" 403
out=$(dk $S docker buildx build --allow security.insecure --load -t nfx/b7 $B 2>&1); rc=$?
echo "$out" | tail -2 | sed 's/^/   | /'
if [ $rc -ne 0 ] && echo "$out" | grep -qiE "not allowed|marsh:|entitlement|insecure"; then pass "--allow security.insecure is not honoured: $(echo "$out" | grep -iE 'not allowed|marsh:|entitlement|insecure' | head -1 | cut -c1-90)"; else fail "--allow security.insecure: rc=$rc and no entitlement refusal: $(echo "$out" | tail -2)"; fi
dk $S docker buildx rm -f evilbx >/dev/null 2>&1
expect refused "buildx create --driver docker-container --bootstrap" -- dk $S docker buildx create --driver docker-container --name evilbx --bootstrap
summary
