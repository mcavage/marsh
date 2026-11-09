#!/bin/sh
# Remove the local image (idempotent); containers using it must be gone first.
docker rmi -f nfx/local-app:1 >/dev/null 2>&1
exit 0
