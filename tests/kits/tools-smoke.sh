#!/bin/sh
# Offline checks of the built marsh-tools Kit image (no credentials, no network
# calls succeed). Usage: tests/kits/tools-smoke.sh IMAGE
set -eu
image=$1
gog() { docker run --rm --network none -e GOG_ENABLE_COMMANDS_EXACT=gmail.send "$image" gog "$@" 2>&1 || true; }
fail() { echo "FAIL: $*" >&2; exit 1; }

docker run --rm --entrypoint /usr/local/libexec/gog "$image" --version | grep -q . || fail "gog binary"

# Every REFUSE category of the policy (kits/marsh-tools/README.md) is refused
# by gog before any request: sends and notifications, sharing, deletes, account
# settings, admin and auth, hooks and passthroughs, and local-image publishing.
for cmd in "gmail send --to a@b.c --subject s --body b" "send --to a@b.c --subject s --body b" \
           "gmail drafts send d1" "gmail drafts post d1" "gmail reply m1 --body b" "gmail forward m1 --to a@b.c" \
           "gmail reply-all m1 --body b" "gmail autoreply from:x --body b" "chat messages send s1 --text hi" \
           "calendar respond primary e1 --status accepted" "calendar out-of-office --from 2026-01-01T09:00:00Z --to 2026-01-02T09:00:00Z" \
           "drive share f1 --email a@b.c" "drive unshare f1 p1" "drive bulk remove-public --file f1" "drive comments create f1 hi" \
           "drive delete f1" "gmail trash m1" "gmail batch delete m1" "calendar delete primary e1" "tasks delete l1 t1" "keep delete n1" \
           "docs delete-tab d1 --tab t1" "docs clear d1" "sheets delete-tab s1 Tab" "sheets clear s1 A1:B2" "sheets batch-request s1 --requests-json []" \
           "slides delete-slide p1 s1" "drive move f1 --parent p1" "forms publish f1" \
           "gmail settings filters create --from a@b.c --trash" "gmail filters create --from a@b.c --trash" "gmail settings forwarding create a@b.c" \
           "gmail settings delegates add a@b.c" "gmail settings sendas update a@b.c" "gmail settings vacation update --enable" \
           "auth list" "auth tokens export a@b.c" "login a@b.c" "config set x y" "admin users list" "backup push" "batch end b1" \
           "api call drive v3 files.list" "mcp" "update status" "zoom auth setup" "classroom announcements create c1 --text hi" \
           "contacts delete c1" "docs insert-image d1 --url https://example.com/a.png" "slides insert-image p1 ./x.png" "docs comments poll d1 --state-file s.json --on-new true"; do
  gog $cmd | grep -q "is not enabled" || fail "not refused: $cmd"
done

# A sample of every ALLOW category gets past the allowlist (and then fails only
# on the network or its own arguments).
for cmd in "gmail search newer_than:1d --max 1" "gmail drafts create --to a@b.c --subject s --body b" "gmail settings filters list" \
           "calendar events --max 1" "calendar create primary --summary s --from 2026-01-01T09:00:00Z --to 2026-01-01T10:00:00Z" \
           "calendar update primary e1 --summary s" "drive ls" "drive upload ./x" "drive mkdir d" "drive rename f1 n" \
           "docs create t" "docs write d1 --text x" "docs tabs rename d1 --tab t --title n" "docs add-tab d1 --title t" "docs rename-tab d1 --tab t --title n" \
           "sheets create t" "sheets update s1 A1 x" "sheets add-tab s1 Tab" "sheets rename-tab s1 Old New" "sheets duplicate-tab s1 Tab New" \
           "slides create t" "slides replace-text p1 a b --all" "forms create --title t" "forms questions add f1 --title t" \
           "tasks add l1 --title t" "tasks done l1 t1" "tasks lists create t" "people me" "time now" "ls" "search x" "upload ./x"; do
  out=$(gog $cmd)
  echo "$out" | grep -q "is not enabled" && fail "allowed command refused: $cmd: $out"
done

# Calendar events are made without guests: the flags that invite or notify are rejected.
for flag in --attendees=a@b.c --add-attendee=a@b.c --send-updates=all; do
  gog calendar create primary --summary s "$flag" | grep -q "no guests" || fail "calendar flag accepted: $flag"
done

# Every command gog has is allowed or refused, nothing in both.
"$(dirname "$0")/tools-policy.py" "$image" || fail "policy lists out of step with gog"

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
