#!/bin/sh
# Start Postgres and seed 5 notes with unique text (prefix from UAT_RUN, default
# `uat`). Run from the project root, with the docker CLI reaching the services
# engine. Idempotent.
set -eu
prefix=${UAT_RUN:-uat}
docker compose up -d --wait
for n in 1 2 3 4 5; do
  docker compose exec -T db psql -U postgres -d app -v ON_ERROR_STOP=1 -q -c \
    "insert into notes (title, note) select 'note-$n', '$prefix-note-$n-unique' where not exists (select 1 from notes where title = 'note-$n')"
done
echo "seeded: $(docker compose exec -T db psql -U postgres -d app -Atc 'select count(*) from notes') notes"
