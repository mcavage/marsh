#!/bin/sh
# Seed 3 users with avatars through the local APIs (UAT J2). Run from the project
# root with `supabase start` already up. The service key is read into the
# environment and never printed. Users: uat-<run>-user-N@example.test, avatar N =
# 2048 deterministic bytes (`yes avatar-N | head -c 2048`) at avatars/<N>.png.
set -eu
run=${UAT_RUN:-uat}
eval "$(supabase status -o env 2>/dev/null | grep -E '^(API_URL|SERVICE_ROLE_KEY)=')"
: "${API_URL:?supabase is not running}" "${SERVICE_ROLE_KEY:?no key}"
for n in 1 2 3; do
  email="$run-user-$n@example.test"
  curl -sf -m 20 -H "Authorization: Bearer $SERVICE_ROLE_KEY" -H "apikey: $SERVICE_ROLE_KEY" -H "Content-Type: application/json" \
    -d "{\"email\":\"$email\",\"password\":\"nfx-pass-$n-0123\",\"email_confirm\":true}" "$API_URL/auth/v1/admin/users" >/dev/null
  yes "avatar-$n" | head -c 2048 | curl -sf -m 20 -H "Authorization: Bearer $SERVICE_ROLE_KEY" -H "apikey: $SERVICE_ROLE_KEY" \
    -H "Content-Type: application/octet-stream" -H "x-upsert: true" --data-binary @- "$API_URL/storage/v1/object/avatars/$n.png" >/dev/null
done
echo "seeded 3 users with avatars"
