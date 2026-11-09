#!/bin/sh
# Remove the database and the build cache (the fixture's own state only).
set -u
docker compose down -v 2>/dev/null
rm -rf .next
