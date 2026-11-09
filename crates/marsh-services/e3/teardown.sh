#!/usr/bin/env bash
# Remove everything bootstrap.sh and the sessions made. Under the VM lock.
SB=${SB:-nfx-e3-a}
sbx exec "$SB" docker rm -f nfx-e3-engine >/dev/null 2>&1
sbx rm -f "$SB" 2>&1 | tail -1
docker volume rm e3-target >/dev/null 2>&1
rm -rf /private/tmp/nfx-e3
sbx ls 2>/dev/null | grep -c '^nfx-e3-' | sed 's/^/nfx-e3 sandboxes left: /'
