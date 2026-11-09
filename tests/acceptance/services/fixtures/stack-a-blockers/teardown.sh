#!/bin/sh
# Remove the objects only this stack's setup and blockers create. Idempotent.
set -u
docker rm -f nfx-mail >/dev/null 2>&1
docker volume rm nfx-ext nfx-fixed >/dev/null 2>&1
exit 0
