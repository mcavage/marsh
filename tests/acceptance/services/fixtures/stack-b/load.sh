#!/usr/bin/env bash
# seed.bin is deterministic (yes nfx-seed | head -c 1 MiB: sha256 3714b445…).
# Stands in for the dev server: uploads a file to Supabase storage, then
# inserts the matching row through the REST API, ~10 times a second.
# Run from the stack folder. Keys come from the local stack and are never printed.
set -u
eval "$(supabase status -o env 2>/dev/null | grep -E '^(API_URL|SERVICE_ROLE_KEY)=')"
: "${API_URL:?supabase not running}" "${SERVICE_ROLE_KEY:?no key}"
auth=(-H "Authorization: Bearer $SERVICE_ROLE_KEY" -H "apikey: $SERVICE_ROLE_KEY")

yes nfx-seed | head -c 1048576 | curl -sf -m 10 "${auth[@]}" -H "Content-Type: application/octet-stream" \
  -H "x-upsert: true" --data-binary @- "$API_URL/storage/v1/object/uploads/seed.bin" >/dev/null \
  || echo "seed upload failed" >&2
[ "${1:-}" = "--seed-only" ] && exit 0

errs=0
while true; do
  t=$(date +%s%N)
  if curl -sf -m 5 "${auth[@]}" -H "Content-Type: text/plain" --data "$t" \
       "$API_URL/storage/v1/object/uploads/tok-$t" >/dev/null; then
    curl -sf -m 5 "${auth[@]}" -H "Content-Type: application/json" -H "Prefer: return=minimal" \
      -d "{\"token\":\"$t\"}" "$API_URL/rest/v1/tokens" >/dev/null || errs=$((errs+1))
  else
    errs=$((errs+1))
  fi
  echo "$errs" > /tmp/stackb-load-errors
  sleep 0.1
done
