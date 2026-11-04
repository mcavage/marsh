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
# local files (an image upload makes the file public), and calendar changes.
for cmd in "gmail send --to a@b.c --subject s --body b" "send --to a@b.c --subject s --body b" \
           "gmail drafts send d1" "gmail drafts post d1" "gmail reply m1 --body b" "gmail forward m1 --to a@b.c" \
           "gmail reply-all m1 --body b" "gmail autoreply from:x --body b" "chat messages send s1 --text hi" \
           "calendar respond primary e1 --status accepted" "calendar out-of-office --from 2026-01-01T09:00:00Z --to 2026-01-02T09:00:00Z" \
           "calendar create primary --summary s --from 2026-01-01T09:00:00Z --to 2026-01-01T10:00:00Z" "calendar update primary e1 --summary s" \
           "calendar working-location --from 2026-01-01 --to 2026-01-02 --type home" "calendar propose-time cal e1" \
           "drive share f1 --email a@b.c" "drive unshare f1 p1" "drive bulk remove-public --file f1" \
           "drive delete f1" "drive delete f1 --permanent" "gmail trash m1" "gmail batch delete m1" "calendar delete primary e1" "tasks delete l1 t1" "keep delete n1" \
           \
           "drive move f1 --parent p1" "drive sync push ./d --parent p1" "drive shortcut create f1 --parent p1" "forms publish f1" \
           "slides create-from-markdown x.md" "drive comments delete f1 c1" "docs comments delete d1 c1" "sheets datasource delete s1 d1" "drive comments update f1 c1 hi" \
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
           "docs delete-tab d1 --tab t1" "docs clear d1" "docs delete d1 --at x" "docs table-row delete d1 --row 1" \
           "sheets delete-tab s1 Tab" "sheets clear s1 A1:B2" "sheets delete-dimension s1 Sheet1 --dimension ROWS --start 1 --end 2" \
           "slides delete-slide p1 s1" "slides element delete p1 o1" "forms delete-question f1 0" \
           "docs create t" "docs write d1 --text x" "docs write d1 --markdown --append --text ![a](https://example.com/a.png)" "docs sed d1 s/a/b/" \
           "docs tabs rename d1 --tab t --title n" "docs add-tab d1 --title t" "docs rename-tab d1 --tab t --title n" \
           "sheets create t" "sheets update s1 A1 x" "sheets append s1 A1 x" "sheets append s1 A1 =IMAGE(\"https://example.com\") --input USER_ENTERED" \
           "sheets copy-paste s1 A1:B2 C1 --type FORMULA" "sheets copy-paste s1 A1:B2 C1" "sheets find-replace s1 a b" "sheets add-tab s1 Tab" "sheets rename-tab s1 Old New" "sheets duplicate-tab s1 Tab New" \
           "slides create t" "slides replace-text p1 a b --all" "forms create --title t" "forms questions add f1 --title t" \
           "tasks add l1 --title t" "tasks done l1 t1" "tasks lists create t" "people me" "time now" "ls" "search x" "upload ./x" \
           "drive search x --parent f1" "tasks add l1 --title t --parent t1" "drive upload ./x --convert-to sheet" \
           "docs comments add d1 hello" "docs comments reply d1 c1 thanks" "docs comments resolve d1 c1" "docs comments reopen d1 c1" \
           "drive comments create f1 hello" "drive comments reply f1 c1 thanks" "drive comments resolve f1 c1" \
           "gmail drafts create --to a@b.c --subject s --body-html <b>x</b>" "gmail drafts create --to a@b.c --subject s --body b --html-from-text"; do
  out=$(gog $cmd)
  echo "$out" | grep -qE "is not enabled|is not available|isn.t allowed in|is fixed to" && fail "allowed command refused: $cmd: $out"
done

# Nothing runs code: --mmdc (an external renderer) is refused wherever it appears.
gog slides create-from-markdown x.md --mmdc /bin/sh | grep -q "is not available" || fail "--mmdc accepted"
gog docs cat d1 --on-new true | grep -qE "is not available|unknown flag" || fail "--on-new accepted"

# New files land in My Drive root: no destination, however the command is spelled.
# A copy is pinned to the root so it can't inherit its source's (maybe shared) folder.
for cmd in "drive upload ./x --parent f1" "upload ./x --parent=f1" "drive mkdir d --parent f1" "docs create t --parent f1" \
           "sheets create t --parent f1"; do
  gog $cmd | grep -q "is not available" || fail "destination accepted: $cmd"
done
for cmd in "drive copy f1 n --parent p1" "docs copy d1 t --parent f1" "sheets copy s1 t --parent f1" "slides copy p1 t --parent f1" \
           "slides create t --parent f1" "slides create-from-template t1 t --parent f1"; do
  gog $cmd | grep -q "is fixed to root" || fail "copy destination accepted: $cmd"
done
for cmd in "drive copy f1 n" "docs copy d1 t" "sheets copy s1 t" "slides copy p1 t" "slides create t" "slides create t --template p1"; do
  gog -n --json $cmd | grep -q '"parent": "root"' || fail "copy isn't pinned to the root: $cmd"
done

# Formulas are allowed: values are written as the agent gives them.
gog -n --json sheets append s1 A1 '=IMAGE("https://example.com/?q="&A1)' | grep -q '"value_input_option": "USER_ENTERED"' || fail "formula write refused or changed"

# Comments are allowed, but not with an @-mention or +email, which Google emails.
gog docs comments add d1 "hi @bob" | grep -q "isn't allowed in" || fail "@-mention accepted in a doc comment"
gog docs comments add d1 "hi+bob@example.com" | grep -q "isn't allowed in" || fail "+email accepted in a doc comment"
gog docs comments reply d1 c1 "thanks @bob" | grep -q "isn't allowed in" || fail "@-mention accepted in a reply"
gog drive comments create f1 "cc cc@example.com" | grep -q "isn't allowed in" || fail "email accepted in a file comment"
gog drive comments reply f1 c1 "thanks +bob@example.com" | grep -q "isn't allowed in" || fail "+email accepted in a file reply"
gog docs comments add d1 "plain comment, no mention" | grep -qE "isn.t allowed in|is not (enabled|available)" && fail "plain comment refused"

# Raw Sheets batchUpdate requests edit inside the spreadsheet: adding a sheet is allowed
# (inline, from a file, and from stdin), data-source and comment requests are not.
add='[{"addSheet":{"properties":{"title":"Notes"}}}]'
work=$(mktemp -d)
chmod 755 "$work"  # mktemp makes it 0700; the container's user must read it
trap 'rm -rf "$work"' EXIT
printf '%s' "$add" > "$work/add.json"
printf '[{"addDataSource":{"dataSource":{}}}]' > "$work/data-source.json"
chmod a+r "$work"/*.json
gogv() { docker run --rm -i --network none -v "$work:/work" -w /work "$image" gog "$@" 2>&1 || true; }
gog -n --json sheets batch-request s1 --requests-json "$add" | grep -q '"op": "sheets.batch-request"' || fail "batch-request refused"
gogv -n --json sheets batch-request s1 --requests-json @add.json </dev/null | grep -q '"op": "sheets.batch-request"' || fail "batch-request @file refused"
printf '%s' "$add" | gogv -n --json sheets batch-request s1 --requests-json @- | grep -q '"op": "sheets.batch-request"' || fail "batch-request @- refused"
gog sheets batch-request s1 --requests-json "$add" --force | grep -qE "is not enabled|reaches outside" && fail "batch-request refused when applied"
for bad in '[{"addDataSource":{}}]' '[{"refreshDataSource":{}}]' '[{"add_data_source":{}}]' '[{"insertComment":{}}]' '[{"deleteComment":{}}]' \
           '[{"addSheet":{"x":[{"insert\u0043omment":1}]}}]' '[{"updateCells":{"rows":[{"values":[{"dataSourceTable":{}}]}]}}]'; do
  gog -n --json sheets batch-request s1 --requests-json "$bad" | grep -q "reaches outside" || fail "request accepted: $bad"
done
gogv -n --json sheets batch-request s1 --requests-json @data-source.json </dev/null | grep -q "reaches outside" || fail "data-source request accepted from a file"
printf '%s' '[{"addDataSource":{}}]' | gogv -n --json sheets batch-request s1 --requests-json=@- | grep -q "reaches outside" || fail "data-source request accepted from stdin"

# Stdin is still the agent's to give: gog reads it itself when no content is given.
out=$(printf '%s' 'from stdin' | docker run --rm -i --network none "$image" gog -n docs insert d1 2>&1 || true)
echo "$out" | grep -q "Dry run" || fail "stdin not passed to gog: $out"

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
