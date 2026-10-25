#!/bin/sh
# Offline checks of the built Kit images (no credentials, no network calls
# succeed). Usage: tests/kits/tools-smoke.sh GOG_IMAGE BAMBOOHR_IMAGE
set -eu
gog_image=$1 bamboo_image=$2
gog() { docker run --rm --network none -e GOG_ENABLE_COMMANDS_EXACT=gmail.send "$gog_image" "$@" 2>&1 || true; }
fail() { echo "FAIL: $*" >&2; exit 1; }

docker run --rm --entrypoint /usr/local/libexec/gog "$gog_image" --version | grep -q . || fail "gog binary"

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
docker run --rm --network none --user root --entrypoint sh "$gog_image" -c \
  'rm /usr/local/share/gog-allowed-commands; /usr/local/bin/gog gmail search x' 2>&1 | grep -q "list is missing" || fail "runs without allowlist"

# Proxy settings survive the wrapper: SBX attaches the credential at its proxy.
docker run --rm --network none -e HTTPS_PROXY=http://127.0.0.1:9 "$gog_image" gmail search x 2>&1 \
  | grep -qi "proxy" || fail "gog ignored the proxy settings"

# An allowed command gets past the allowlist (and then fails only on the network).
out=$(gog gmail search newer_than:1d --max 1)
echo "$out" | grep -q "is not enabled" && fail "allowed command refused: $out"

# bamboohr: only directory reads exist.
docker run --rm --network none "$bamboo_image" update-employee 2>&1 | grep -q "operation is not allowed" || fail "bamboohr write op"
echo "smoke: ok"
