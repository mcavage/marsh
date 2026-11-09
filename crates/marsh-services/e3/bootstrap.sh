#!/usr/bin/env bash
# Create the E3 scratch project and sandbox, and build the harness binary.
# Everything it makes is named nfx-e3-* / lives under /private/tmp/nfx-e3, and is
# removed by teardown.sh. Run under the VM lock (vmlock.py --shared --run e3-boot).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
PROJ=${PROJ:-/private/tmp/nfx-e3/proj}
SB=${SB:-nfx-e3-a}
mkdir -p "$PROJ/d" "$PROJ/src"
echo ok-file > "$PROJ/d/ok.txt"
[ -d "$PROJ/stack-a" ] || cp -R "$HOME/dev/narwhal-fork-spike/stacks/a" "$PROJ/stack-a"
if [ ! -d "$PROJ/.git" ]; then
  git -C "$PROJ" init -q && git -C "$PROJ" -c user.email=e3@example.com -c user.name=e3 -c commit.gpgsign=false add -A \
    && git -C "$PROJ" -c user.email=e3@example.com -c user.name=e3 -c commit.gpgsign=false commit -q -m seed
fi
bash "$HERE/build-linux.sh" 2>&1 | tail -5
sbx ls 2>/dev/null | grep -q "^$SB " && { echo "sandbox $SB exists"; } || sbx create --name "$SB" shell "$PROJ" 2>&1 | tail -3
sbx exec "$SB" docker version --format 'sandbox docker {{.Server.Version}}'
