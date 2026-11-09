#!/usr/bin/env bash
# Seed once, then (if LOAD=1) write ~10 tokens/s: upload file first, row second.
set -u
until pg_isready -q; do sleep 0.2; done

psql -v ON_ERROR_STOP=1 -q <<'SQL'
CREATE TABLE IF NOT EXISTS items (id serial PRIMARY KEY, name text NOT NULL, note text);
CREATE TABLE IF NOT EXISTS tokens (token text PRIMARY KEY, at timestamptz DEFAULT now());
CREATE TABLE IF NOT EXISTS markers (name text PRIMARY KEY);
INSERT INTO items (name, note)
  SELECT 'item-' || g, 'seed' FROM generate_series(1, 1000) g
  WHERE NOT EXISTS (SELECT 1 FROM items);
INSERT INTO markers (name) VALUES ('m-seed-1'), ('m-seed-2'), ('m-seed-3') ON CONFLICT DO NOTHING;
SQL
# Deterministic, so the fixture manifest can carry its hash.
[ -f /uploads/seed.bin ] || yes nfx-seed | head -c 1048576 > /uploads/seed.bin

if [ "${LOAD:-0}" != "1" ]; then exec sleep infinity; fi

# One long-lived psql session fed by a fifo keeps each insert ~1 ms.
mkfifo /tmp/q
psql -q < /tmp/q &
exec 3>/tmp/q
while true; do
  t=$(date +%s%N)
  echo "$t" > "/uploads/tok-$t"
  echo "INSERT INTO tokens (token) VALUES ('$t');" >&3
  sleep 0.1
done
