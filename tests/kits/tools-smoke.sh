#!/bin/sh
# Offline checks of the built marsh-tools Kit image (no credentials, no network
# calls succeed). Usage: tests/kits/tools-smoke.sh IMAGE
set -eu
image=$1
gog() { docker run --rm --network none -e GOG_ENABLE_COMMANDS_EXACT=gmail.send "$image" gog "$@" 2>&1 || true; }
fail() { echo "FAIL: $*" >&2; exit 1; }

docker run --rm --entrypoint /usr/local/libexec/gog "$image" --version | grep -q . || fail "gog binary"

# Never-allowed commands are refused by gog before any request.
for cmd in "gmail send --to a@b.c --subject s --body b" "send --to a@b.c --subject s --body b" \
           "gmail drafts send d1" "gmail drafts post d1" "gmail reply m1 --body b" "gmail forward m1 --to a@b.c" \
           "drive share f1 --email a@b.c" "drive delete f1" "gmail trash m1" "calendar delete primary e1" \
           "auth list" "drive upload x"; do
  gog $cmd | grep -q "is not enabled" || fail "not refused: $cmd"
done

# The wrapper's own flags can't be overridden.
for flag in --enable-commands-exact=gmail.send --enable-commands=gmail --disable-commands=x \
            --access-token=x --home=/tmp --client=x --readonly=false --gmail-no-send=false; do
  gog "$flag" gmail send --to a@b.c | grep -q "fixed by this tool" || fail "flag accepted: $flag"
done

# Without its list the wrapper refuses to run (an empty list would allow everything).
docker run --rm --network none --user root --entrypoint sh "$image" -c \
  'rm /usr/local/share/gog-allowed-commands; /usr/local/bin/marsh-tools gog gmail search x' 2>&1 | grep -q "list is missing" || fail "runs without allowlist"

# Proxy settings survive the wrapper: SBX attaches the credential at its proxy.
docker run --rm --network none -e HTTPS_PROXY=http://127.0.0.1:9 "$image" gog gmail search x 2>&1 \
  | grep -qi "proxy" || fail "gog ignored the proxy settings"

# An allowed command gets past the allowlist (and then fails only on the network).
out=$(gog gmail search newer_than:1d --max 1)
echo "$out" | grep -q "is not enabled" && fail "allowed command refused: $out"

# bamboohr: only directory reads exist, and the company comes from the image.
docker run --rm --network none "$image" bamboohr update-employee 2>&1 | grep -q "operation is not allowed" || fail "bamboohr write op"
docker run --rm --network none "$image" bamboohr find-employee --query x 2>&1 | grep -q "BAMBOOHR_COMPANY_DOMAIN" || fail "bamboohr without a company"

# The tool comes from marsh's job record; an unknown name is refused.
job() { printf '{"version":1,"job":"j","name":"%s"}' "$1"; }
docker run --rm --network none --user root --entrypoint sh "$image" -c \
  "mkdir -p /run/marsh && echo '$(job bamboohr)' > /run/marsh/job.json && /usr/local/bin/marsh-tools update-employee" 2>&1 \
  | grep -q "operation is not allowed" || fail "job name did not pick bamboohr"
docker run --rm --network none --user root --entrypoint sh "$image" -c \
  "mkdir -p /run/marsh && echo '$(job other)' > /run/marsh/job.json && /usr/local/bin/marsh-tools gmail search x" 2>&1 \
  | grep -q "register this Kit under one of" || fail "unknown tool name accepted"
echo "smoke: ok"
