#!/bin/sh
# Stop and delete this project's local Supabase stack (the fixture's own state only).
set -u
supabase stop --no-backup 2>/dev/null
exit 0
